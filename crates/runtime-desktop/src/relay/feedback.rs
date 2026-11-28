//! Transport-wide feedback (RTPFB 15) for Phoenix's video. Phoenix only raises
//! its bitrate when told how its packets arrived, and the voice server's
//! feedback ends at the bridge, so the bridge reports arrivals itself.

use std::{
   collections::BTreeMap,
   time::{Duration, Instant},
};

const INTERVAL: Duration = Duration::from_millis(50);
/// Arrivals waiting for a report, capped so a flood can't grow them without
/// end.
const MAX_PENDING: usize = 4096;
const MAX_STATUS: u64 = 1024;
const RTPFB: u8 = 205;
const TRANSPORT_FEEDBACK: u8 = 15;
const TICK_US: i64 = 250;
const REFERENCE_US: i64 = 64_000;

pub struct Feedback {
   epoch: Instant,
   arrivals: BTreeMap<u64, Instant>,
   next: Option<u64>,
   max: Option<u64>,
   count: u8,
   last_sent: Option<Instant>,
}

impl Feedback {
   pub const fn new(epoch: Instant) -> Self {
      Self {
         epoch,
         arrivals: BTreeMap::new(),
         next: None,
         max: None,
         count: 0,
         last_sent: None,
      }
   }

   pub fn record(&mut self, seq: u16, at: Instant) {
      let seq = extend(self.max, seq);
      if self.next.is_some_and(|next| seq < next) || self.arrivals.len() >= MAX_PENDING {
         return;
      }
      self.next.get_or_insert(seq);
      self.max = Some(self.max.map_or(seq, |max| max.max(seq)));
      self.arrivals.insert(seq, at);
   }

   pub fn due(&self) -> Option<Instant> {
      let first = *self.arrivals.values().next()?;
      Some(self.last_sent.map_or(first, |at| at + INTERVAL))
   }

   /// The next report from `sender` about `media`, covering every arrival
   /// since the last one.
   pub fn build(&mut self, sender: u32, media: u32, now: Instant) -> Option<Vec<u8>> {
      let (&first, &first_at) = self.arrivals.iter().next()?;
      let next = self.next.unwrap_or(first);
      let base = if first - next >= MAX_STATUS { first } else { next };
      let end = self.max?.min(base + MAX_STATUS - 1);
      let micros = |at: Instant| {
         i64::try_from(at.saturating_duration_since(self.epoch).as_micros()).unwrap_or(i64::MAX)
      };
      let reference = micros(first_at) / REFERENCE_US;
      let mut previous = reference * REFERENCE_US;
      let mut symbols = Vec::new();
      let mut deltas = Vec::new();
      for seq in base..=end {
         let Some(&at) = self.arrivals.get(&seq) else {
            symbols.push(0u16);
            continue;
         };
         let ticks = (micros(at) - previous) / TICK_US;
         if let Ok(small) = u8::try_from(ticks) {
            symbols.push(1);
            deltas.push(small);
            previous += i64::from(small) * TICK_US;
         } else {
            let large = i16::try_from(ticks).unwrap_or(if ticks < 0 { i16::MIN } else { i16::MAX });
            symbols.push(2);
            deltas.extend(large.to_be_bytes());
            previous += i64::from(large) * TICK_US;
         }
      }
      let mut packet = vec![0x80 | TRANSPORT_FEEDBACK, RTPFB, 0, 0];
      packet.extend(sender.to_be_bytes());
      packet.extend(media.to_be_bytes());
      packet.extend(&base.to_be_bytes()[6..]);
      packet.extend(u16::try_from(symbols.len()).ok()?.to_be_bytes());
      packet.extend(&reference.to_be_bytes()[5..]);
      packet.push(self.count);
      for chunk in symbols.chunks(7) {
         let bits = chunk.iter().enumerate().fold(0xC000u16, |bits, (i, &symbol)| bits | symbol << (12 - 2 * i));
         packet.extend(bits.to_be_bytes());
      }
      packet.extend(deltas);
      let padding: u8 = [0, 3, 2, 1][packet.len() % 4];
      if padding > 0 {
         packet[0] |= 0x20;
         packet.extend(std::iter::repeat_n(0, usize::from(padding) - 1));
         packet.push(padding);
      }
      let words = u16::try_from(packet.len() / 4 - 1).ok()?;
      packet[2..4].copy_from_slice(&words.to_be_bytes());
      self.arrivals = self.arrivals.split_off(&(end + 1));
      self.next = Some(end + 1);
      self.count = self.count.wrapping_add(1);
      self.last_sent = Some(now);
      Some(packet)
   }
}

/// `seq` widened past 16 bits, next to the highest one seen.
fn extend(max: Option<u64>, seq: u16) -> u64 {
   let Some(max) = max else {
      return u64::from(seq) + (1 << 16);
   };
   let [.., high, low] = max.to_be_bytes();
   let step = i64::from(seq.wrapping_sub(u16::from_be_bytes([high, low])).cast_signed());
   max.saturating_add_signed(step)
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn reports_arrivals_and_losses() {
      let epoch = Instant::now();
      let mut feedback = Feedback::new(epoch);
      assert!(feedback.due().is_none());
      feedback.record(65534, epoch + Duration::from_millis(100));
      feedback.record(1, epoch + Duration::from_millis(101));
      assert_eq!(feedback.due(), Some(epoch + Duration::from_millis(100)));
      let packet = feedback.build(7, 9, epoch).expect("report");
      assert_eq!(packet.len() % 4, 0);
      assert_eq!(usize::from(u16::from_be_bytes([packet[2], packet[3]])), packet.len() / 4 - 1);
      assert_eq!(packet[1], RTPFB);
      assert_eq!(&packet[12..16], &[0xFF, 0xFE, 0, 4]);
      assert_eq!(&packet[16..19], &[0, 0, 1]);
      assert_eq!(&packet[20..22], &[0xD0, 0x40]);
      assert_eq!(&packet[22..24], &[144, 4]);
      assert!(feedback.due().is_none());
      feedback.record(65535, epoch);
      assert!(feedback.due().is_none());
   }
}
