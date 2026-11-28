//! Voice on Spacebar servers. Phoenix only speaks Discord's voice protocol
//! (plain UDP with its own encryption) and Spacebar voice servers only take
//! WebRTC, so the gateway points Phoenix's voice connection here: this side
//! talks WebRTC to the server and Discord's protocol to Phoenix.

use std::{
   collections::HashSet,
   io::{self, BufReader},
   net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket},
   sync::{
      Arc,
      atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering},
      mpsc::{self, Receiver, RecvTimeoutError, Sender},
   },
   thread,
   time::{Duration, Instant},
};

use aes_gcm::{
   Aes256Gcm, Key, KeyInit, Nonce, Tag,
   aead::{AeadInPlace, OsRng, rand_core::RngCore},
};
use serde_json::{Value, json};
use str0m::{
   Candidate, Event, IceConnectionState, Input, Output, Rtc,
   change::{SdpAnswer, SdpPendingOffer},
   media::{Direction, MediaKind, Mid, Pt},
   net::{Protocol, Receive},
   rtp::{RtpPacket, RtpWrite, SeqNo, Ssrc},
};

use super::{Writer, upstream::Socket, wire, wire::Message};

const MODE: &str = "aead_aes256_gcm_rtpsize";
/// The payload type Phoenix plays Opus from.
const PHOENIX_OPUS: u8 = 120;
const OPUS: u8 = 111;
const PHOENIX_HEARTBEAT_MS: u64 = 13_750;
const DISCOVERY_LEN: usize = 74;
const TAG_LEN: usize = 16;
const NONCE_TAIL: usize = 4;
const READ_TIMEOUT: Duration = Duration::from_millis(500);
const IDLE: Duration = Duration::from_secs(1);
/// Speakers are learned from their packets, so a flood can't grow this
/// without end.
const MAX_SPEAKERS: usize = 256;

/// The voice address Phoenix gets for `endpoint` when voice goes through the
/// relay at `relay`.
pub fn bridged(relay: &str, endpoint: &str) -> Option<String> {
   let base = relay.strip_suffix("/g")?;
   if endpoint.starts_with(base) {
      return None;
   }
   let host = endpoint
      .strip_prefix("wss://")
      .or_else(|| endpoint.strip_prefix("ws://"))
      .unwrap_or(endpoint);
   let host = host.split(['/', '?']).next().unwrap_or(host);
   let host = host.strip_suffix(":443").unwrap_or(host);
   valid_host(host).then(|| format!("{base}/r/{host}"))
}

fn valid_host(host: &str) -> bool {
   !host.is_empty()
      && host
         .bytes()
         .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b':'))
}

enum Work {
   Phoenix(Vec<u8>, SocketAddr),
   Server(Vec<u8>, SocketAddr, SocketAddr),
   Select(Option<String>),
   Answer(Value),
   Announce(Duration),
   Stop,
}

struct Shared {
   phoenix: Writer,
   upstream: Arc<Socket>,
   seq: AtomicU64,
   our_ssrc: AtomicU32,
   phoenix_port: AtomicU16,
   closed: AtomicBool,
   work: Sender<Work>,
}

impl Shared {
   fn to_phoenix(&self, op: u64, d: &Value) {
      let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
      let frame = json!({ "op": op, "d": d, "seq": seq });
      let _ = wire::write_message(&self.phoenix, &Message::Text(frame.to_string().into_bytes()));
   }

   fn to_server(&self, op: u64, d: &Value) {
      let frame = json!({ "op": op, "d": d });
      if let Err(err) = self.upstream.send(&Message::Text(frame.to_string().into_bytes())) {
         log::warn!("voice: send to server: {err}");
      }
   }

   fn close(&self, code: u16, reason: &str) {
      if self.closed.swap(true, Ordering::AcqRel) {
         return;
      }
      log::info!("voice: session closed: {reason}");
      let _ = self.work.send(Work::Stop);
      self.upstream.shutdown(1000, b"");
      wire::close(&self.phoenix, code, reason.as_bytes());
   }
}

#[derive(serde::Deserialize)]
struct Frame {
   op: u64,
   #[serde(default)]
   d: Value,
}

/// Runs one voice connection for the server at `host`, after Phoenix's
/// websocket upgrade.
pub fn bridge(
   reader: &mut BufReader<TcpStream>,
   phoenix: Writer,
   host: &str,
   headers: &[(String, String)],
) -> io::Result<()> {
   let host = host.split('/').next().unwrap_or(host);
   if !valid_host(host) {
      wire::close(&phoenix, 4000, b"bad voice address");
      return Ok(());
   }
   log::info!("voice: connecting to {host}");
   let upstream = match Socket::connect(&format!("wss://{host}/?v=9"), headers) {
      Ok(socket) => Arc::new(socket),
      Err(err) => {
         wire::close(&phoenix, 4015, b"voice server unreachable");
         return Err(err);
      },
   };
   let phoenix_udp = UdpSocket::bind(("127.0.0.1", 0))?;
   phoenix_udp.set_read_timeout(Some(READ_TIMEOUT))?;
   let (work, inbox) = mpsc::channel();
   let shared = Arc::new(Shared {
      phoenix,
      upstream,
      seq: AtomicU64::new(0),
      our_ssrc: AtomicU32::new(0),
      phoenix_port: AtomicU16::new(phoenix_udp.local_addr()?.port()),
      closed: AtomicBool::new(false),
      work,
   });
   spawn("sunbird-voice-udp", {
      let shared = shared.clone();
      let socket = phoenix_udp.try_clone()?;
      move || read_udp(&shared, &socket, |packet, from, _| Work::Phoenix(packet, from))
   })?;
   spawn("sunbird-voice-server", {
      let shared = shared.clone();
      move || from_server(&shared)
   })?;
   spawn("sunbird-voice-media", {
      let shared = shared.clone();
      let host = host.to_owned();
      move || Media::new(shared, phoenix_udp, host).run(&inbox)
   })?;
   shared.to_phoenix(8, &json!({ "heartbeat_interval": PHOENIX_HEARTBEAT_MS }));
   loop {
      if shared.closed.load(Ordering::Acquire) {
         break;
      }
      match wire::read_message(reader, &shared.phoenix) {
         Ok(Message::Text(bytes)) => from_phoenix(&shared, &bytes),
         Ok(Message::Binary(_)) => {},
         Ok(Message::Close(code, _)) => {
            shared.close(1000, &format!("Phoenix closed {code}"));
            break;
         },
         Err(_) => {
            shared.close(1000, "Phoenix went away");
            break;
         },
      }
   }
   Ok(())
}

fn spawn(name: &str, run: impl FnOnce() + Send + 'static) -> io::Result<()> {
   thread::Builder::new().name(name.to_owned()).spawn(run).map(drop)
}

fn read_udp(
   shared: &Shared,
   socket: &UdpSocket,
   wrap: impl Fn(Vec<u8>, SocketAddr, SocketAddr) -> Work,
) {
   let Ok(local) = socket.local_addr() else {
      return;
   };
   let mut buf = vec![0; 2048];
   while !shared.closed.load(Ordering::Acquire) {
      match socket.recv_from(&mut buf) {
         Ok((len, from)) => {
            if shared.work.send(wrap(buf[..len].to_vec(), from, local)).is_err() {
               return;
            }
         },
         Err(err) if matches!(err.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {},
         Err(err) if err.kind() == io::ErrorKind::ConnectionReset => {},
         Err(err) => {
            log::warn!("voice: udp: {err}");
            return;
         },
      }
   }
}

fn from_phoenix(shared: &Shared, bytes: &[u8]) {
   let Ok(frame) = serde_json::from_slice::<Frame>(bytes) else {
      return;
   };
   let mut d = frame.d;
   match frame.op {
      3 => {
         let t = d.get("t").cloned().unwrap_or(d);
         let ack = json!({ "op": 6, "d": { "t": t } });
         let _ = wire::write_message(&shared.phoenix, &Message::Text(ack.to_string().into_bytes()));
      },
      7 => shared.close(4006, "resume unsupported"),
      0 => {
         if let Some(object) = d.as_object_mut() {
            object.insert("max_dave_protocol_version".to_owned(), json!(0));
            if let Some(Value::Array(streams)) = object.get_mut("streams") {
               for stream in streams.iter_mut().filter_map(Value::as_object_mut) {
                  stream.remove("active");
               }
            }
         }
         shared.to_server(0, &d);
      },
      1 => {
         let id = d.get("rtc_connection_id").and_then(Value::as_str).map(str::to_owned);
         let _ = shared.work.send(Work::Select(id));
      },
      5 => {
         let ours = shared.our_ssrc.load(Ordering::Relaxed);
         if ours != 0
            && let Some(object) = d.as_object_mut()
         {
            object.insert("ssrc".to_owned(), json!(ours));
         }
         shared.to_server(5, &d);
      },
      12 | 16 | 21 | 23 => {},
      op => shared.to_server(op, &d),
   }
}

fn from_server(shared: &Arc<Shared>) {
   let heartbeat = Arc::new(AtomicU64::new(0));
   loop {
      let message = match shared.upstream.receive() {
         Ok(Message::Text(bytes)) => bytes,
         Ok(Message::Binary(_)) => continue,
         Ok(Message::Close(code, _)) => {
            log::info!("voice: server closed {code}");
            shared.close(4015, "voice server closed");
            return;
         },
         Err(err) => {
            log::info!("voice: server connection ended: {err}");
            shared.close(4015, "voice server lost");
            return;
         },
      };
      let Ok(frame) = serde_json::from_slice::<Frame>(&message) else {
         continue;
      };
      match frame.op {
         8 => {
            let interval = frame
               .d
               .get("heartbeat_interval")
               .and_then(Value::as_u64)
               .unwrap_or(30_000)
               .max(1000);
            let generation = heartbeat.fetch_add(1, Ordering::AcqRel) + 1;
            let shared = shared.clone();
            let heartbeat = heartbeat.clone();
            let _ = spawn("sunbird-voice-beat", move || {
               beat(&shared, &heartbeat, generation, Duration::from_millis(interval));
            });
         },
         2 => {
            let ready = json!({
               "ssrc": frame.d.get("ssrc").cloned().unwrap_or_else(|| json!(0)),
               "ip": "127.0.0.1",
               "port": shared.phoenix_port.load(Ordering::Relaxed),
               "modes": [MODE],
               "experiments": [],
               "streams": frame.d.get("streams").cloned().unwrap_or(json!([])),
            });
            shared.to_phoenix(2, &ready);
         },
         4 => {
            let _ = shared.work.send(Work::Answer(frame.d));
         },
         6 => {},
         11 => {
            for delay in [500, 2000] {
               let _ = shared.work.send(Work::Announce(Duration::from_millis(delay)));
            }
            shared.to_phoenix(11, &frame.d);
         },
         op => shared.to_phoenix(op, &frame.d),
      }
   }
}

fn beat(shared: &Shared, generation: &AtomicU64, mine: u64, interval: Duration) {
   let step = Duration::from_millis(250);
   loop {
      let mut waited = Duration::ZERO;
      while waited < interval {
         thread::sleep(step);
         waited += step;
         if shared.closed.load(Ordering::Acquire) || generation.load(Ordering::Acquire) != mine {
            return;
         }
      }
      let now = std::time::SystemTime::now()
         .duration_since(std::time::UNIX_EPOCH)
         .map_or(0, |since| since.as_millis());
      shared.to_server(3, &json!(u64::try_from(now).unwrap_or(0)));
   }
}

struct Webrtc {
   rtc: Rtc,
   socket: UdpSocket,
   mid: Mid,
   pending: Option<SdpPendingOffer>,
   extmaps: Vec<String>,
   connected: bool,
}

struct Media {
   shared: Arc<Shared>,
   phoenix_udp: UdpSocket,
   host: String,
   phoenix_addr: Option<SocketAddr>,
   key: [u8; 32],
   cipher: Aes256Gcm,
   counter: u32,
   webrtc: Option<Webrtc>,
   ready: bool,
   out_seq: u64,
   sent: u64,
   published: bool,
   speakers: HashSet<u32>,
   announce_at: Vec<Instant>,
   next_timeout: Option<Instant>,
}

impl Media {
   fn new(shared: Arc<Shared>, phoenix_udp: UdpSocket, host: String) -> Self {
      let mut key = [0u8; 32];
      OsRng.fill_bytes(&mut key);
      Self {
         shared,
         phoenix_udp,
         host,
         phoenix_addr: None,
         cipher: Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key)),
         key,
         counter: OsRng.next_u32() >> 1,
         webrtc: None,
         ready: false,
         out_seq: 0,
         sent: 0,
         published: false,
         speakers: HashSet::new(),
         announce_at: Vec::new(),
         next_timeout: None,
      }
   }

   fn run(mut self, inbox: &Receiver<Work>) {
      loop {
         let now = Instant::now();
         let deadline = self
            .announce_at
            .iter()
            .copied()
            .chain(self.next_timeout)
            .min()
            .unwrap_or(now + IDLE);
         let wait = deadline.saturating_duration_since(now);
         match inbox.recv_timeout(wait) {
            Ok(Work::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(Work::Phoenix(packet, from)) => self.phoenix_packet(&packet, from),
            Ok(Work::Server(packet, source, destination)) => self.server_packet(&packet, source, destination),
            Ok(Work::Select(id)) => {
               if let Err(err) = self.start(id) {
                  log::warn!("voice: could not start WebRTC: {err}");
                  self.shared.close(4000, "voice setup failed");
               }
            },
            Ok(Work::Answer(d)) => {
               if let Err(err) = self.answer(&d) {
                  log::warn!("voice: the server's answer didn't apply: {err}");
                  self.shared.close(4000, "voice setup failed");
               }
            },
            Ok(Work::Announce(delay)) => self.announce_at.push(Instant::now() + delay),
            Err(RecvTimeoutError::Timeout) => {},
         }
         let now = Instant::now();
         if self.next_timeout.is_some_and(|at| at <= now) {
            self.next_timeout = None;
            self.mutate(|rtc| rtc.handle_input(Input::Timeout(now)).map_err(|err| err.to_string()));
         }
         let due = self.announce_at.iter().filter(|&&at| at <= now).count();
         self.announce_at.retain(|&at| at > now);
         for _ in 0..due {
            self.announce();
         }
      }
   }

   /// Applies one change to the WebRTC side and sends what it produces.
   fn mutate(&mut self, change: impl FnOnce(&mut Rtc) -> Result<(), String>) {
      let Some(webrtc) = self.webrtc.as_mut() else {
         return;
      };
      if let Err(err) = change(&mut webrtc.rtc) {
         log::warn!("voice: {err}");
      }
      self.drain();
   }

   fn drain(&mut self) {
      let mut packets = Vec::new();
      let mut connected = false;
      let mut failed = None;
      {
         let Some(webrtc) = self.webrtc.as_mut() else {
            return;
         };
         loop {
            match webrtc.rtc.poll_output() {
               Ok(Output::Timeout(at)) => {
                  self.next_timeout = Some(at);
                  break;
               },
               Ok(Output::Transmit(transmit)) => {
                  let _ = webrtc.socket.send_to(&transmit.contents, transmit.destination);
               },
               Ok(Output::Event(Event::RtpPacket(packet))) => packets.push(packet),
               Ok(Output::Event(Event::Connected)) => connected = true,
               Ok(Output::Event(Event::IceConnectionStateChange(IceConnectionState::Disconnected))) => {
                  failed = Some("the voice connection dropped");
               },
               Ok(Output::Event(Event::Closed)) => failed = Some("the voice server hung up"),
               Ok(Output::Event(_)) => {},
               Err(err) => {
                  log::warn!("voice: WebRTC: {err}");
                  failed = Some("WebRTC failed");
                  break;
               },
            }
         }
         if connected && !webrtc.connected {
            webrtc.connected = true;
         } else {
            connected = false;
         }
      }
      for packet in packets {
         self.play(&packet);
      }
      if connected {
         log::info!("voice: connected to {}", self.host);
         let now = Instant::now();
         for delay in [1500, 4000, 9000] {
            self.announce_at.push(now + Duration::from_millis(delay));
         }
      }
      if let Some(reason) = failed {
         self.shared.close(4000, reason);
      }
   }

   fn start(&mut self, rtc_connection_id: Option<String>) -> Result<(), String> {
      if self.webrtc.is_some() {
         return Ok(());
      }
      let local_ip = local_ip(&self.host).map_err(|err| format!("no route to {}: {err}", self.host))?;
      let socket = UdpSocket::bind((local_ip, 0)).map_err(|err| err.to_string())?;
      socket.set_read_timeout(Some(READ_TIMEOUT)).map_err(|err| err.to_string())?;
      let local = socket.local_addr().map_err(|err| err.to_string())?;
      let now = Instant::now();
      let mut rtc = Rtc::builder()
         .set_rtp_mode(true)
         .clear_codecs()
         .enable_opus(true, false)
         .set_crypto_provider(Arc::new(str0m_rust_crypto::default_provider()))
         .build(now);
      let candidate = Candidate::host(local, "udp").map_err(|err| err.to_string())?;
      rtc.add_local_candidate(candidate);
      let mut change = rtc.sdp_api();
      let mid = change.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
      let (offer, pending) = change.apply().ok_or("nothing to offer")?;
      let sdp = offer.to_sdp_string();
      let extmaps = sdp
         .lines()
         .filter(|line| {
            line.starts_with("a=extmap:")
               && (line.contains("ssrc-audio-level") || line.contains("transport-wide-cc"))
         })
         .map(str::to_owned)
         .collect();
      let ours = offered_ssrc(&sdp).unwrap_or(0);
      self.shared.our_ssrc.store(ours, Ordering::Relaxed);
      spawn("sunbird-voice-rtc", {
         let shared = self.shared.clone();
         let socket = socket.try_clone().map_err(|err| err.to_string())?;
         move || read_udp(&shared, &socket, Work::Server)
      })
      .map_err(|err| err.to_string())?;
      self.webrtc = Some(Webrtc {
         rtc,
         socket,
         mid,
         pending: Some(pending),
         extmaps,
         connected: false,
      });
      self.drain();
      let id = rtc_connection_id.unwrap_or_else(|| format!("{:032x}", OsRng.next_u64()));
      self.shared.to_server(
         1,
         &json!({
            "protocol": "webrtc",
            "data": sdp,
            "sdp": sdp,
            "codecs": [{ "name": "opus", "type": "audio", "priority": 1000, "payload_type": OPUS }],
            "rtc_connection_id": id,
         }),
      );
      Ok(())
   }

   fn answer(&mut self, d: &Value) -> Result<(), String> {
      let webrtc = self.webrtc.as_mut().ok_or("no offer was made")?;
      let pending = webrtc.pending.take().ok_or("already answered")?;
      let compact = d.get("sdp").and_then(Value::as_str).unwrap_or_default();
      let sdp = answer_sdp(compact, &webrtc.mid.to_string(), &webrtc.extmaps);
      let answer = SdpAnswer::from_sdp_string(&sdp).map_err(|err| err.to_string())?;
      webrtc.rtc.sdp_api().accept_answer(pending, answer).map_err(|err| err.to_string())?;
      self.drain();
      self.ready = true;
      let text = |name: &str, fallback: &str| {
         d.get(name).and_then(Value::as_str).unwrap_or(fallback).to_owned()
      };
      self.shared.to_phoenix(
         4,
         &json!({
            "mode": MODE,
            "secret_key": self.key.to_vec(),
            "audio_codec": text("audio_codec", "opus"),
            "video_codec": text("video_codec", "H264"),
            "media_session_id": text("media_session_id", "sunbird"),
            "dave_protocol_version": 0,
         }),
      );
      Ok(())
   }

   fn server_packet(&mut self, packet: &[u8], source: SocketAddr, destination: SocketAddr) {
      if let Some(ssrc) = rtp_ssrc(packet) {
         self.learn_speaker(ssrc);
      }
      let now = Instant::now();
      self.mutate(|rtc| {
         let contents = str0m::net::DatagramRecv::try_from(packet).map_err(|err| err.to_string())?;
         rtc.handle_input(Input::Receive(
            now,
            Receive {
               proto: Protocol::Udp,
               source,
               destination,
               contents,
            },
         ))
         .map_err(|err| err.to_string())
      });
   }

   /// The server forwards other speakers without announcing their streams,
   /// and str0m drops packets from streams it doesn't expect.
   fn learn_speaker(&mut self, ssrc: u32) {
      if ssrc == self.shared.our_ssrc.load(Ordering::Relaxed)
         || self.speakers.contains(&ssrc)
         || self.speakers.len() >= MAX_SPEAKERS
      {
         return;
      }
      let Some(mid) = self.webrtc.as_ref().map(|webrtc| webrtc.mid) else {
         return;
      };
      self.speakers.insert(ssrc);
      self.mutate(|rtc| {
         rtc.direct_api().expect_stream_rx(Ssrc::from(ssrc), None, mid, None);
         Ok(())
      });
   }

   fn phoenix_packet(&mut self, packet: &[u8], from: SocketAddr) {
      if packet.len() == DISCOVERY_LEN && packet.starts_with(&[0, 1]) {
         let mut reply = [0u8; DISCOVERY_LEN];
         reply[..2].copy_from_slice(&2u16.to_be_bytes());
         reply[2..4].copy_from_slice(&70u16.to_be_bytes());
         reply[4..8].copy_from_slice(&packet[4..8]);
         reply[8..17].copy_from_slice(b"127.0.0.1");
         reply[72..].copy_from_slice(&from.port().to_be_bytes());
         let _ = self.phoenix_udp.send_to(&reply, from);
         self.phoenix_addr = Some(from);
         return;
      }
      self.phoenix_addr = Some(from);
      if packet.len() < 2 || (72..=76).contains(&(packet[1] & 0x7F)) || !self.ready {
         return;
      }
      let Some((payload, timestamp, marker)) = open(&self.cipher, packet) else {
         return;
      };
      let Some(mid) = self.webrtc.as_ref().map(|webrtc| webrtc.mid) else {
         return;
      };
      self.sent += 1;
      let seq = SeqNo::from(self.out_seq);
      self.out_seq += 1;
      let now = Instant::now();
      let shared = self.shared.clone();
      self.mutate(|rtc| {
         let mut direct = rtc.direct_api();
         let stream = direct.stream_tx_by_mid(mid, None).ok_or("no stream to send on")?;
         shared.our_ssrc.store(*stream.ssrc(), Ordering::Relaxed);
         stream.write_rtp(RtpWrite::new(Pt::from(OPUS), seq, timestamp, now, payload).marker(marker));
         Ok(())
      });
   }

   fn play(&mut self, packet: &RtpPacket) {
      let Some(to) = self.phoenix_addr.filter(|_| self.ready) else {
         return;
      };
      let mut header = [0u8; 12];
      header[0] = 0x80;
      header[1] = PHOENIX_OPUS | if packet.header.marker { 0x80 } else { 0 };
      header[2..4].copy_from_slice(&packet.header.sequence_number.to_be_bytes());
      header[4..8].copy_from_slice(&packet.header.timestamp.to_be_bytes());
      header[8..12].copy_from_slice(&(*packet.header.ssrc).to_be_bytes());
      self.counter = self.counter.wrapping_add(1);
      let Some(sealed) = seal(&self.cipher, &header, &packet.payload, self.counter) else {
         return;
      };
      let _ = self.phoenix_udp.send_to(&sealed, to);
   }

   /// Tells the server about our audio once Phoenix is sending, first with no
   /// stream and then with ours, which the server needs before it forwards it.
   fn announce(&mut self) {
      if self.shared.closed.load(Ordering::Acquire)
         || !self.webrtc.as_ref().is_some_and(|webrtc| webrtc.connected)
      {
         return;
      }
      let ours = self.shared.our_ssrc.load(Ordering::Relaxed);
      if ours == 0 {
         return;
      }
      if self.sent == 0 {
         self.announce_at.push(Instant::now() + Duration::from_millis(500));
         return;
      }
      let audio = if self.published {
         ours
      } else {
         self.published = true;
         self.announce_at.push(Instant::now() + Duration::from_millis(400));
         0
      };
      self.shared.to_server(
         12,
         &json!({ "audio_ssrc": audio, "video_ssrc": 0, "rtx_ssrc": 0, "streams": [] }),
      );
   }
}

/// The SSRC our offer announces, which str0m only sets up a stream for once
/// the answer is in.
fn offered_ssrc(sdp: &str) -> Option<u32> {
   sdp.lines()
      .find_map(|line| line.strip_prefix("a=ssrc:"))
      .and_then(|rest| rest.split(' ').next())
      .and_then(|ssrc| ssrc.trim().parse().ok())
}

/// The address this machine reaches `host` from.
fn local_ip(host: &str) -> io::Result<std::net::IpAddr> {
   let target = if host.contains(':') {
      host.to_owned()
   } else {
      format!("{host}:443")
   };
   let remote = target
      .to_socket_addrs()?
      .find(SocketAddr::is_ipv4)
      .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no IPv4 address"))?;
   let probe = UdpSocket::bind(("0.0.0.0", 0))?;
   probe.connect(remote)?;
   Ok(probe.local_addr()?.ip())
}

/// The SSRC of a plain RTP packet (not RTCP, STUN or DTLS).
fn rtp_ssrc(packet: &[u8]) -> Option<u32> {
   let first = *packet.first()?;
   let pt = packet.get(1)? & 0x7F;
   if !(128..192).contains(&first) || (64..96).contains(&pt) {
      return None;
   }
   Some(u32::from_be_bytes(*packet.get(8..12)?.first_chunk::<4>()?))
}

fn header_len(packet: &[u8]) -> usize {
   let csrcs = usize::from(packet[0] & 0x0F);
   let extension = if packet[0] & 0x10 == 0 { 0 } else { 4 };
   12 + csrcs * 4 + extension
}

/// Phoenix's packet as an Opus payload, its RTP time and marker.
fn open(cipher: &Aes256Gcm, packet: &[u8]) -> Option<(Vec<u8>, u32, bool)> {
   if packet.len() < 12 + TAG_LEN + NONCE_TAIL {
      return None;
   }
   let head = header_len(packet);
   let body_end = packet.len().checked_sub(TAG_LEN + NONCE_TAIL)?;
   if head > body_end {
      return None;
   }
   let mut nonce = [0u8; 12];
   nonce[..NONCE_TAIL].copy_from_slice(&packet[packet.len() - NONCE_TAIL..]);
   let mut plain = packet[head..body_end].to_vec();
   let tag = Tag::from_slice(&packet[body_end..body_end + TAG_LEN]);
   cipher
      .decrypt_in_place_detached(Nonce::from_slice(&nonce), &packet[..head], &mut plain, tag)
      .ok()?;
   if packet[0] & 0x10 != 0 {
      let words = u16::from_be_bytes([packet[head - 2], packet[head - 1]]);
      plain.drain(..(usize::from(words) * 4).min(plain.len()));
   }
   let timestamp = u32::from_be_bytes(*packet.get(4..8)?.first_chunk::<4>()?);
   Some((plain, timestamp, packet[1] & 0x80 != 0))
}

fn seal(cipher: &Aes256Gcm, header: &[u8; 12], payload: &[u8], counter: u32) -> Option<Vec<u8>> {
   let mut nonce = [0u8; 12];
   nonce[..4].copy_from_slice(&counter.to_be_bytes());
   let mut body = payload.to_vec();
   let tag = cipher
      .encrypt_in_place_detached(Nonce::from_slice(&nonce), header, &mut body)
      .ok()?;
   let mut out = Vec::with_capacity(12 + body.len() + TAG_LEN + NONCE_TAIL);
   out.extend_from_slice(header);
   out.extend_from_slice(&body);
   out.extend_from_slice(&tag);
   out.extend_from_slice(&nonce[..NONCE_TAIL]);
   Some(out)
}

/// A full answer from the compact one Spacebar sends: its address, ICE
/// credentials, fingerprint and candidates.
fn answer_sdp(compact: &str, mid: &str, extmaps: &[String]) -> String {
   let lines: Vec<&str> = compact.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
   let find = |prefix: &str| lines.iter().copied().find(|line| line.starts_with(prefix));
   let port = find("m=audio ")
      .and_then(|line| line.split(' ').nth(1))
      .unwrap_or("9");
   let ip = find("c=")
      .and_then(|line| line.split_once("IN IP4 "))
      .map_or("0.0.0.0", |(_, ip)| ip.trim());
   let mut out = vec![
      "v=0".to_owned(),
      "o=- 0 0 IN IP4 127.0.0.1".to_owned(),
      "s=-".to_owned(),
      "t=0 0".to_owned(),
      format!("a=group:BUNDLE {mid}"),
      "a=msid-semantic: WMS".to_owned(),
      format!("m=audio {port} UDP/TLS/RTP/SAVPF {OPUS}"),
      format!("c=IN IP4 {ip}"),
      format!("a=rtcp:{port} IN IP4 {ip}"),
   ];
   for prefix in ["a=ice-ufrag:", "a=ice-pwd:", "a=fingerprint:"] {
      if let Some(line) = find(prefix) {
         out.push(line.to_owned());
      }
   }
   out.extend([
      "a=setup:passive".to_owned(),
      format!("a=mid:{mid}"),
      "a=sendrecv".to_owned(),
      "a=rtcp-mux".to_owned(),
   ]);
   out.extend(extmaps.iter().cloned());
   out.push(format!("a=rtpmap:{OPUS} opus/48000/2"));
   out.push(format!("a=fmtp:{OPUS} minptime=10;useinbandfec=1"));
   out.extend(lines.iter().filter(|line| line.starts_with("a=candidate:")).map(|line| (*line).to_owned()));
   out.push("a=end-of-candidates".to_owned());
   out.push(String::new());
   out.join("\r\n")
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn bridges_voice_endpoints() {
      let relay = "ws://127.0.0.1:5000/g";
      assert_eq!(
         bridged(relay, "wss://voice.example.org:443").as_deref(),
         Some("ws://127.0.0.1:5000/r/voice.example.org")
      );
      assert_eq!(
         bridged(relay, "voice.example.org:8443").as_deref(),
         Some("ws://127.0.0.1:5000/r/voice.example.org:8443")
      );
      assert_eq!(bridged(relay, "ws://127.0.0.1:5000/r/voice.example.org"), None);
      assert_eq!(bridged(relay, "wss://bad host"), None);
   }

   #[test]
   fn seals_and_opens_phoenix_packets() {
      let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&[7; 32]));
      let mut header = [0u8; 12];
      header[0] = 0x80;
      header[1] = 0x80 | PHOENIX_OPUS;
      header[4..8].copy_from_slice(&1234u32.to_be_bytes());
      let sealed = seal(&cipher, &header, b"opus", 9).expect("seal");
      assert_eq!(open(&cipher, &sealed), Some((b"opus".to_vec(), 1234, true)));
   }

   #[test]
   fn reads_the_offered_ssrc() {
      let sdp = "m=audio 9 UDP/TLS/RTP/SAVPF 111
a=ssrc:2591998996 cname:abc
";
      assert_eq!(offered_ssrc(sdp), Some(2_591_998_996));
      assert_eq!(offered_ssrc("m=audio 9
"), None);
   }

   #[test]
   fn tells_rtp_from_rtcp() {
      let mut rtp = [0u8; 12];
      rtp[0] = 0x80;
      rtp[1] = 111;
      rtp[8..12].copy_from_slice(&42u32.to_be_bytes());
      assert_eq!(rtp_ssrc(&rtp), Some(42));
      rtp[1] = 0xC8;
      assert_eq!(rtp_ssrc(&rtp), None);
      assert_eq!(rtp_ssrc(&[0, 1, 0, 0]), None);
   }
}

