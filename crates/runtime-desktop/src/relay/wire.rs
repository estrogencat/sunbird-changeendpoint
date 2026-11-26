//! The websocket framing Phoenix speaks to the relay, RFC 6455 without
//! extensions: none are offered back in the handshake.

use std::{
   io::{self, Read, Write},
   sync::PoisonError,
};

use super::Writer;

const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const LIMIT: u64 = 64 << 20;

const CONTINUATION: u8 = 0;
const TEXT: u8 = 1;
const BINARY: u8 = 2;
const CLOSE: u8 = 8;
const PING: u8 = 9;
const PONG: u8 = 10;

pub enum Message {
   Text(Vec<u8>),
   Binary(Vec<u8>),
   Close(u16, Vec<u8>),
}

fn invalid(message: &str) -> io::Error {
   io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

struct Frame {
   fin: bool,
   opcode: u8,
   payload: Vec<u8>,
}

fn read_frame(reader: &mut impl Read) -> io::Result<Frame> {
   let mut head = [0u8; 2];
   reader.read_exact(&mut head)?;
   let len = match head[1] & 0x7F {
      126 => {
         let mut bytes = [0u8; 2];
         reader.read_exact(&mut bytes)?;
         u64::from(u16::from_be_bytes(bytes))
      },
      127 => {
         let mut bytes = [0u8; 8];
         reader.read_exact(&mut bytes)?;
         u64::from_be_bytes(bytes)
      },
      len => u64::from(len),
   };
   if len > LIMIT {
      return Err(invalid("frame too large"));
   }
   let mask = if head[1] & 0x80 == 0 {
      None
   } else {
      let mut mask = [0u8; 4];
      reader.read_exact(&mut mask)?;
      Some(mask)
   };
   let mut payload = vec![0u8; usize::try_from(len).map_err(|_| invalid("frame too large"))?];
   reader.read_exact(&mut payload)?;
   if let Some(mask) = mask {
      for (byte, key) in payload.iter_mut().zip(mask.iter().cycle()) {
         *byte ^= key;
      }
   }
   Ok(Frame {
      fin: head[0] & 0x80 != 0,
      opcode: head[0] & 0x0F,
      payload,
   })
}

fn write_frame(writer: &Writer, opcode: u8, payload: &[u8]) -> io::Result<()> {
   let mut head = Vec::with_capacity(10);
   head.push(0x80 | opcode);
   match u16::try_from(payload.len()) {
      Ok(len) if len < 126 => head.extend_from_slice(&len.to_be_bytes()[1..]),
      Ok(len) => {
         head.push(126);
         head.extend_from_slice(&len.to_be_bytes());
      },
      Err(_) => {
         head.push(127);
         head.extend_from_slice(&(payload.len() as u64).to_be_bytes());
      },
   }
   let mut stream = writer.lock().unwrap_or_else(PoisonError::into_inner);
   stream.write_all(&head)?;
   stream.write_all(payload)?;
   stream.flush()
}

/// The next whole message, answering pings on the way.
pub fn read_message(reader: &mut impl Read, writer: &Writer) -> io::Result<Message> {
   let mut kind = None;
   let mut data = Vec::new();
   loop {
      let frame = read_frame(reader)?;
      match frame.opcode {
         PING => write_frame(writer, PONG, &frame.payload)?,
         PONG => {},
         CLOSE => {
            let code = frame
               .payload
               .first_chunk::<2>()
               .map_or(1005, |code| u16::from_be_bytes(*code));
            return Ok(Message::Close(
               code,
               frame.payload.get(2..).unwrap_or_default().to_vec(),
            ));
         },
         TEXT | BINARY if kind.is_none() => {
            kind = Some(frame.opcode);
            data = frame.payload;
         },
         CONTINUATION if kind.is_some() => {
            if (data.len() + frame.payload.len()) as u64 > LIMIT {
               return Err(invalid("message too large"));
            }
            data.extend_from_slice(&frame.payload);
         },
         _ => return Err(invalid("unexpected frame")),
      }
      if frame.fin
         && let Some(kind) = kind
      {
         return Ok(if kind == TEXT {
            Message::Text(data)
         } else {
            Message::Binary(data)
         });
      }
   }
}

pub fn write_message(writer: &Writer, message: &Message) -> io::Result<()> {
   match message {
      Message::Text(data) => write_frame(writer, TEXT, data),
      Message::Binary(data) => write_frame(writer, BINARY, data),
      Message::Close(code, reason) => {
         let mut payload = code.to_be_bytes().to_vec();
         payload.extend_from_slice(&reason[..reason.len().min(120)]);
         write_frame(writer, CLOSE, &payload)
      },
   }
}

/// Closes Phoenix's side. It may already be gone.
pub fn close(writer: &Writer, code: u16, reason: &[u8]) {
   let _ = write_message(writer, &Message::Close(code, reason.to_vec()));
}

pub fn accept_key(key: &str) -> String {
   base64(&sha1(format!("{key}{GUID}").as_bytes()))
}

#[expect(
   clippy::many_single_char_names,
   reason = "the names SHA-1 is specified with"
)]
fn sha1(data: &[u8]) -> [u8; 20] {
   let mut state: [u32; 5] = [
      0x6745_2301,
      0xEFCD_AB89,
      0x98BA_DCFE,
      0x1032_5476,
      0xC3D2_E1F0,
   ];
   let mut message = data.to_vec();
   message.push(0x80);
   while message.len() % 64 != 56 {
      message.push(0);
   }
   message.extend_from_slice(&((data.len() as u64).wrapping_mul(8)).to_be_bytes());
   for block in message.as_chunks::<64>().0 {
      let mut words = [0u32; 80];
      for (word, bytes) in words.iter_mut().zip(block.as_chunks::<4>().0) {
         *word = u32::from_be_bytes(*bytes);
      }
      for at in 16..80 {
         words[at] =
            (words[at - 3] ^ words[at - 8] ^ words[at - 14] ^ words[at - 16]).rotate_left(1);
      }
      let [mut a, mut b, mut c, mut d, mut e] = state;
      for (at, word) in words.iter().enumerate() {
         let (mix, constant) = match at {
            0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
            20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
            40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
            _ => (b ^ c ^ d, 0xCA62_C1D6),
         };
         let next = a
            .rotate_left(5)
            .wrapping_add(mix)
            .wrapping_add(e)
            .wrapping_add(constant)
            .wrapping_add(*word);
         e = d;
         d = c;
         c = b.rotate_left(30);
         b = a;
         a = next;
      }
      for (slot, value) in state.iter_mut().zip([a, b, c, d, e]) {
         *slot = slot.wrapping_add(value);
      }
   }
   let mut out = [0u8; 20];
   for (bytes, value) in out.as_chunks_mut::<4>().0.iter_mut().zip(state) {
      *bytes = value.to_be_bytes();
   }
   out
}

fn base64(data: &[u8]) -> String {
   const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
   let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
   for chunk in data.chunks(3) {
      let bytes = [0, 1, 2].map(|at| chunk.get(at).copied().unwrap_or(0));
      let group = u32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]]);
      for at in 0..4 {
         if at <= chunk.len() {
            let index = (group >> (18 - 6 * at)) & 63;
            out.push(char::from(TABLE[index as usize]));
         } else {
            out.push('=');
         }
      }
   }
   out
}

#[cfg(test)]
mod tests {
   #[test]
   fn accept_key_matches_rfc() {
      assert_eq!(
         super::accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
         "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
      );
   }
}
