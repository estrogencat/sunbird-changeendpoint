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
   media::{Direction, KeyframeRequestKind, MediaKind, Mid, Pt},
   net::{Protocol, Receive},
   rtp::{RtpPacket, RtpWrite, SeqNo, Ssrc},
};

use super::{Writer, feedback::Feedback, upstream::Socket, wire, wire::Message};

const MODE: &str = "aead_aes256_gcm_rtpsize";
/// The payload type Phoenix plays Opus from.
const PHOENIX_OPUS: u8 = 120;
const OPUS: u8 = 111;
/// What the voice server asks browsers for, so viewers can decode it.
const H264_PROFILE: u32 = 0x42_e0_1f;
const PHOENIX_HEARTBEAT_MS: u64 = 13_750;
const RTCP_HEADER: usize = 8;
const PSFB: u8 = 206;
const PLI: u8 = 1;
const FIR: u8 = 4;
const KEYFRAME_GAP: Duration = Duration::from_millis(300);
/// Some voice servers ask for a keyframe only once per viewer and never
/// resend lost packets, and a screen share sends none on its own.
const KEYFRAME_REFRESH: Duration = Duration::from_secs(3);
const ONE_BYTE_EXTENSIONS: [u8; 2] = [0xBE, 0xDE];
/// Where Phoenix puts its transport-wide sequence number on video packets
/// (seen on build 356207, it never says so in signalling).
const TRANSPORT_SEQ_EXTENSION: u8 = 5;
const DISCOVERY_LEN: usize = 74;
/// Phoenix measures its ping by sending this with a counter every 5 seconds
/// and timing the echo, which waits out the server's round trip since the
/// bridge itself is next door.
const KEEPALIVE: [u8; 4] = [0x13, 0x37, 0xCA, 0xFE];
const KEEPALIVE_LEN: usize = 8;
const MAX_ECHOES: usize = 8;
/// An ack slower than this means a stuck connection, not a ping to show.
const MAX_RTT_MS: u64 = 10_000;
/// Browsers keep sending Opus frames this small while silent (DTX), and
/// Phoenix shows anyone it gets audio from as speaking.
const SILENCE_LEN: usize = 3;
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
   Select(Value),
   Answer(Value),
   Video(Value),
   Remote(Value),
   Announce(Duration),
   Stop,
}

struct Shared {
   phoenix: Writer,
   upstream: Arc<Socket>,
   seq: AtomicU64,
   our_ssrc: AtomicU32,
   phoenix_port: AtomicU16,
   /// Round trip to the voice server, from its heartbeat acks.
   server_rtt_ms: AtomicU64,
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
   let upstream = match Socket::connect(&voice_url(host), headers) {
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
      server_rtt_ms: AtomicU64::new(0),
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

/// Servers on a local network have no certificate for their address, so
/// those are reached without TLS.
fn voice_url(host: &str) -> String {
   let ip = host
      .parse::<SocketAddr>()
      .map(|addr| addr.ip())
      .or_else(|_| host.parse::<std::net::IpAddr>());
   let local = ip.is_ok_and(|ip| match ip {
      std::net::IpAddr::V4(ip) => ip.is_private() || ip.is_loopback() || ip.is_link_local(),
      std::net::IpAddr::V6(ip) => ip.is_loopback() || ip.is_unique_local(),
   });
   let scheme = if local { "ws" } else { "wss" };
   format!("{scheme}://{host}/?v=9")
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
         let _ = shared.work.send(Work::Select(d));
      },
      12 => {
         let _ = shared.work.send(Work::Video(d));
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
      16 | 21 | 23 => {},
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
         6 => {
            if let Some(sent) = frame.d.as_u64() {
               let rtt = now_ms().saturating_sub(sent);
               if rtt < MAX_RTT_MS {
                  shared.server_rtt_ms.store(rtt, Ordering::Relaxed);
               }
            }
         },
         12 => {
            let _ = shared.work.send(Work::Remote(frame.d));
         },
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
      shared.to_server(3, &json!(now_ms()));
      let mut waited = Duration::ZERO;
      while waited < interval {
         thread::sleep(step);
         waited += step;
         if shared.closed.load(Ordering::Acquire) || generation.load(Ordering::Acquire) != mine {
            return;
         }
      }
   }
}

fn now_ms() -> u64 {
   let now = std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .map_or(0, |since| since.as_millis());
   u64::try_from(now).unwrap_or(0)
}

/// Phoenix's H264 payload types, used on both sides so video needs no
/// rewriting.
#[derive(Clone, Copy)]
struct VideoCodec {
   pt: u8,
   rtx: Option<u8>,
}

impl VideoCodec {
   fn from_codecs(codecs: Option<&Value>) -> Option<Self> {
      let codec = codecs?.as_array()?.iter().find(|codec| {
         codec
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| name.eq_ignore_ascii_case("H264"))
      })?;
      let pt = |name: &str| {
         codec
            .get(name)
            .and_then(Value::as_u64)
            .and_then(|pt| u8::try_from(pt).ok())
            .filter(|pt| (96..128).contains(pt) && ![OPUS, PHOENIX_OPUS].contains(pt))
      };
      let main = pt("payload_type")?;
      Some(Self {
         pt: main,
         rtx: pt("rtx_payload_type").filter(|&rtx| rtx != main),
      })
   }
}

struct Webrtc {
   rtc: Rtc,
   socket: UdpSocket,
   mid: Mid,
   video: Option<(Mid, VideoCodec)>,
   pending: Option<SdpPendingOffer>,
   offer: String,
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
   video_seq: u64,
   sent: u64,
   published: bool,
   speakers: HashSet<u32>,
   /// Phoenix's last video state, with its own SSRCs.
   outgoing: Option<Value>,
   phoenix_video: u32,
   our_video: (u32, u32),
   last_keyframe: Option<Instant>,
   fir_seq: u8,
   announce_at: Vec<Instant>,
   next_timeout: Option<Instant>,
   feedback: Feedback,
   echoes: Vec<(Instant, Vec<u8>, SocketAddr)>,
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
         video_seq: 0,
         sent: 0,
         published: false,
         speakers: HashSet::new(),
         outgoing: None,
         phoenix_video: 0,
         our_video: (0, 0),
         last_keyframe: None,
         fir_seq: 0,
         announce_at: Vec::new(),
         next_timeout: None,
         feedback: Feedback::new(Instant::now()),
         echoes: Vec::new(),
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
            .chain(self.refresh_due())
            .chain(self.feedback.due())
            .chain(self.echoes.iter().map(|(at, ..)| *at))
            .min()
            .unwrap_or(now + IDLE);
         let wait = deadline.saturating_duration_since(now);
         match inbox.recv_timeout(wait) {
            Ok(Work::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(Work::Phoenix(packet, from)) => self.phoenix_packet(&packet, from),
            Ok(Work::Server(packet, source, destination)) => self.server_packet(&packet, source, destination),
            Ok(Work::Video(d)) => self.outgoing_video(d),
            Ok(Work::Remote(d)) => self.remote_video(&d),
            Ok(Work::Select(select)) => {
               if let Err(err) = self.start(&select) {
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
         if self.refresh_due().is_some_and(|at| at <= now) {
            self.ask_phoenix_for_keyframe();
         }
         if self.feedback.due().is_some_and(|at| at <= now) {
            self.send_feedback(now);
         }
         for (_, packet, to) in self.echoes.extract_if(.., |(at, ..)| *at <= now) {
            let _ = self.phoenix_udp.send_to(&packet, to);
         }
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
      let mut keyframe = false;
      let mut failed = None;
      {
         let Some(webrtc) = self.webrtc.as_mut() else {
            return;
         };
         let video_mid = webrtc.video.map(|(mid, _)| mid);
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
               Ok(Output::Event(Event::KeyframeRequest(request))) => {
                  keyframe |= Some(request.mid) == video_mid;
               },
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
      if keyframe {
         self.ask_phoenix_for_keyframe();
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

   fn start(&mut self, select: &Value) -> Result<(), String> {
      if self.webrtc.is_some() {
         return Ok(());
      }
      let rtc_connection_id = select.get("rtc_connection_id").and_then(Value::as_str);
      let codec = VideoCodec::from_codecs(select.get("codecs"));
      let local_ip = local_ip(&self.host).map_err(|err| format!("no route to {}: {err}", self.host))?;
      let socket = UdpSocket::bind((local_ip, 0)).map_err(|err| err.to_string())?;
      socket.set_read_timeout(Some(READ_TIMEOUT)).map_err(|err| err.to_string())?;
      let local = socket.local_addr().map_err(|err| err.to_string())?;
      let now = Instant::now();
      let mut config = Rtc::builder()
         .set_rtp_mode(true)
         .clear_codecs()
         .enable_opus(true, false)
         .set_crypto_provider(Arc::new(str0m_rust_crypto::default_provider()));
      if let Some(codec) = codec {
         config
            .codec_config()
            .add_h264(Pt::from(codec.pt), codec.rtx.map(Pt::from), true, H264_PROFILE);
      }
      let mut rtc = config.build(now);
      let candidate = Candidate::host(local, "udp").map_err(|err| err.to_string())?;
      rtc.add_local_candidate(candidate);
      let mut change = rtc.sdp_api();
      let mid = change.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
      let video = codec.map(|codec| {
         let mid = change.add_media(MediaKind::Video, Direction::SendRecv, None, None, None);
         (mid, codec)
      });
      let (offer, pending) = change.apply().ok_or("nothing to offer")?;
      let sdp = offer.to_sdp_string();
      let ours = offered_ssrc(&sdp).unwrap_or(0);
      self.shared.our_ssrc.store(ours, Ordering::Relaxed);
      spawn("sunbird-voice-rtc", {
         let shared = self.shared.clone();
         let socket = socket.try_clone().map_err(|err| err.to_string())?;
         move || read_udp(&shared, &socket, Work::Server)
      })
      .map_err(|err| err.to_string())?;
      let mut codecs = vec![json!({ "name": "opus", "type": "audio", "priority": 1000, "payload_type": OPUS })];
      if let Some(codec) = codec {
         codecs.push(json!({
            "name": "H264",
            "type": "video",
            "priority": 1000,
            "payload_type": codec.pt,
            "rtx_payload_type": codec.rtx,
         }));
      }
      self.webrtc = Some(Webrtc {
         rtc,
         socket,
         mid,
         video,
         pending: Some(pending),
         offer: sdp.clone(),
         connected: false,
      });
      self.drain();
      let id = rtc_connection_id.map_or_else(|| format!("{:032x}", OsRng.next_u64()), str::to_owned);
      self.shared.to_server(
         1,
         &json!({
            "protocol": "webrtc",
            "data": sdp,
            "sdp": sdp,
            "codecs": codecs,
            "rtc_connection_id": id,
         }),
      );
      Ok(())
   }

   fn answer(&mut self, d: &Value) -> Result<(), String> {
      let webrtc = self.webrtc.as_mut().ok_or("no offer was made")?;
      let pending = webrtc.pending.take().ok_or("already answered")?;
      let compact = d.get("sdp").and_then(Value::as_str).unwrap_or_default();
      let sdp = answer_sdp(compact, &webrtc.offer, webrtc.mid, webrtc.video);
      let answer = SdpAnswer::from_sdp_string(&sdp).map_err(|err| err.to_string())?;
      webrtc.rtc.sdp_api().accept_answer(pending, answer).map_err(|err| err.to_string())?;
      if let Some((mid, _)) = webrtc.video
         && let Some(stream) = webrtc.rtc.direct_api().stream_tx_by_mid(mid, None)
      {
         self.our_video = (*stream.ssrc(), stream.rtx().map_or(0, |rtx| *rtx));
      }
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
      if let Some(ssrc) = rtp_ssrc(packet).filter(|_| packet[1] & 0x7F == OPUS) {
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
      if packet.len() == KEEPALIVE_LEN && packet.starts_with(&KEEPALIVE) {
         if self.echoes.len() < MAX_ECHOES {
            let rtt = Duration::from_millis(self.shared.server_rtt_ms.load(Ordering::Relaxed));
            self.echoes.push((Instant::now() + rtt, packet.to_vec(), from));
         }
         return;
      }
      if packet.len() < 2 || !self.ready {
         return;
      }
      let kind = packet[1] & 0x7F;
      if (72..=79).contains(&kind) {
         self.phoenix_rtcp(packet);
         return;
      }
      let Some(Opened {
         payload,
         timestamp,
         marker,
         transport_seq,
      }) = open(&self.cipher, packet)
      else {
         return;
      };
      if let Some(seq) = transport_seq {
         self.feedback.record(seq, Instant::now());
      }
      if payload.is_empty() {
         return;
      }
      let Some(webrtc) = self.webrtc.as_ref() else {
         return;
      };
      let mid = webrtc.mid;
      if kind != PHOENIX_OPUS {
         if let Some((mid, codec)) = webrtc.video.filter(|(_, codec)| codec.pt == kind) {
            self.send_video(mid, codec, payload, timestamp, marker);
         }
         return;
      }
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

   fn send_video(&mut self, mid: Mid, codec: VideoCodec, payload: Vec<u8>, timestamp: u32, marker: bool) {
      let seq = SeqNo::from(self.video_seq);
      self.video_seq += 1;
      let now = Instant::now();
      self.mutate(|rtc| {
         let mut direct = rtc.direct_api();
         let stream = direct.stream_tx_by_mid(mid, None).ok_or("no video stream to send on")?;
         stream.write_rtp(
            RtpWrite::new(Pt::from(codec.pt), seq, timestamp, now, payload)
               .marker(marker)
               .nackable(true),
         );
         Ok(())
      });
   }

   /// Passes Phoenix's keyframe requests for other people's video on to the
   /// server.
   fn phoenix_rtcp(&mut self, packet: &[u8]) {
      let Some(plain) = open_rtcp(&self.cipher, packet) else {
         return;
      };
      for ssrc in keyframe_requests(&plain) {
         self.mutate(|rtc| {
            if let Some(stream) = rtc.direct_api().stream_rx(&Ssrc::from(ssrc)) {
               stream.request_keyframe(KeyframeRequestKind::Pli);
            }
            Ok(())
         });
      }
   }

   /// Viewers on the server need a keyframe before they can show our video.
   fn refresh_due(&self) -> Option<Instant> {
      let connected = self.webrtc.as_ref().is_some_and(|webrtc| webrtc.connected);
      (connected && self.phoenix_video != 0)
         .then(|| self.last_keyframe.map_or_else(Instant::now, |at| at + KEYFRAME_REFRESH))
   }

   fn ask_phoenix_for_keyframe(&mut self) {
      let now = Instant::now();
      if self.phoenix_video == 0 || self.last_keyframe.is_some_and(|at| now - at < KEYFRAME_GAP) {
         return;
      }
      self.last_keyframe = Some(now);
      let Some(to) = self.phoenix_addr else {
         return;
      };
      self.fir_seq = self.fir_seq.wrapping_add(1);
      let packet = keyframe_request(
         self.shared.our_ssrc.load(Ordering::Relaxed),
         self.phoenix_video,
         self.fir_seq,
      );
      self.counter = self.counter.wrapping_add(1);
      let (header, body) = packet.split_at(RTCP_HEADER);
      if let Some(sealed) = seal(&self.cipher, header, body, self.counter) {
         let _ = self.phoenix_udp.send_to(&sealed, to);
      }
   }

   fn send_feedback(&mut self, now: Instant) {
      let sender = self.shared.our_ssrc.load(Ordering::Relaxed);
      let Some(packet) = self.feedback.build(sender, self.phoenix_video, now) else {
         return;
      };
      let Some(to) = self.phoenix_addr else {
         return;
      };
      self.counter = self.counter.wrapping_add(1);
      let (header, body) = packet.split_at(RTCP_HEADER);
      if let Some(sealed) = seal(&self.cipher, header, body, self.counter) {
         let _ = self.phoenix_udp.send_to(&sealed, to);
      }
   }

   fn outgoing_video(&mut self, d: Value) {
      let was_on = self.phoenix_video != 0;
      self.phoenix_video = ssrc_field(&d, "video_ssrc").unwrap_or(0);
      self.outgoing = Some(d);
      if !self.published || !self.webrtc.as_ref().is_some_and(|webrtc| webrtc.connected) {
         return;
      }
      let ours = self.shared.our_ssrc.load(Ordering::Relaxed);
      let turning_on = !was_on && self.phoenix_video != 0;
      self.shared.to_server(12, &self.media_state(ours, !turning_on));
      if turning_on {
         self.announce_at.push(Instant::now() + Duration::from_millis(400));
      }
   }

   /// What we send as op 12: Phoenix's video state with our SSRCs in place
   /// of its own. Spacebar only publishes video that was off in the update
   /// before, and a Go Live connection starts out with it on, so turning video
   /// on sends it off first.
   fn media_state(&self, audio: u32, video_on: bool) -> Value {
      let (video, rtx) = if !video_on || self.phoenix_video == 0 {
         (0, 0)
      } else {
         self.our_video
      };
      let streams: Vec<Value> = self
         .outgoing
         .as_ref()
         .and_then(|d| d.get("streams"))
         .and_then(Value::as_array)
         .map(|streams| {
            streams
               .iter()
               .map(|stream| {
                  let mut stream = stream.clone();
                  if let Some(object) = stream.as_object_mut() {
                     object.insert("ssrc".to_owned(), json!(video));
                     object.insert("rtx_ssrc".to_owned(), json!(rtx));
                  }
                  stream
               })
               .collect()
         })
         .unwrap_or_default();
      json!({ "audio_ssrc": audio, "video_ssrc": video, "rtx_ssrc": rtx, "streams": streams })
   }

   /// Someone else's streams, which str0m has to expect before it lets their
   /// packets through.
   fn remote_video(&mut self, d: &Value) {
      if let Some(audio) = ssrc_field(d, "audio_ssrc") {
         self.learn_speaker(audio);
      }
      let mid = self.webrtc.as_ref().and_then(|webrtc| webrtc.video).map(|(mid, _)| mid);
      if let (Some(mid), Some(video)) = (mid, ssrc_field(d, "video_ssrc"))
         && (self.speakers.contains(&video) || self.speakers.len() < MAX_SPEAKERS)
      {
         self.speakers.insert(video);
         let rtx = ssrc_field(d, "rtx_ssrc").map(Ssrc::from);
         self.mutate(|rtc| {
            rtc.direct_api().expect_stream_rx(Ssrc::from(video), rtx, mid, None);
            Ok(())
         });
      }
      self.shared.to_phoenix(12, d);
   }

   fn play(&mut self, packet: &RtpPacket) {
      let Some(to) = self.phoenix_addr.filter(|_| self.ready) else {
         return;
      };
      let pt = *packet.header.payload_type;
      let video = self.webrtc.as_ref().and_then(|webrtc| webrtc.video).map(|(_, codec)| codec.pt);
      let pt = if pt == OPUS {
         if packet.payload.len() <= SILENCE_LEN {
            return;
         }
         PHOENIX_OPUS
      } else if Some(pt) == video {
         pt
      } else {
         return;
      };
      let mut header = [0u8; 12];
      header[0] = 0x80;
      header[1] = pt | if packet.header.marker { 0x80 } else { 0 };
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
      if self.sent == 0 && self.phoenix_video == 0 {
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
      self.shared.to_server(12, &self.media_state(audio, audio != 0));
   }
}

fn ssrc_field(d: &Value, name: &str) -> Option<u32> {
   d.get(name)
      .and_then(Value::as_u64)
      .and_then(|ssrc| u32::try_from(ssrc).ok())
      .filter(|&ssrc| ssrc != 0)
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

#[derive(Debug, PartialEq, Eq)]
struct Opened {
   payload: Vec<u8>,
   timestamp: u32,
   marker: bool,
   transport_seq: Option<u16>,
}

/// Phoenix's packet with its header extensions and padding taken off. The
/// payload is empty for bandwidth probes, which carry only padding.
fn open(cipher: &Aes256Gcm, packet: &[u8]) -> Option<Opened> {
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
   let mut transport_seq = None;
   if packet[0] & 0x10 != 0 {
      let words = u16::from_be_bytes([packet[head - 2], packet[head - 1]]);
      let extensions: Vec<u8> = plain.drain(..(usize::from(words) * 4).min(plain.len())).collect();
      if packet[head - 4..head - 2] == ONE_BYTE_EXTENSIONS {
         transport_seq = find_extension(&extensions, TRANSPORT_SEQ_EXTENSION)
            .and_then(|value| value.first_chunk::<2>())
            .map(|value| u16::from_be_bytes(*value));
      }
   }
   if packet[0] & 0x20 != 0 {
      let padding = usize::from(*plain.last()?);
      plain.truncate(plain.len().checked_sub(padding)?);
   }
   let timestamp = u32::from_be_bytes(*packet.get(4..8)?.first_chunk::<4>()?);
   Some(Opened {
      payload: plain,
      timestamp,
      marker: packet[1] & 0x80 != 0,
      transport_seq,
   })
}

/// The value of one-byte header extension `id` (RFC 8285).
fn find_extension(mut extensions: &[u8], id: u8) -> Option<&[u8]> {
   while let [first, rest @ ..] = extensions {
      if *first == 0 {
         extensions = rest;
         continue;
      }
      let (found, len) = (first >> 4, usize::from(first & 0x0F) + 1);
      if found == 15 {
         return None;
      }
      let value = rest.get(..len)?;
      if found == id {
         return Some(value);
      }
      extensions = &rest[len..];
   }
   None
}

/// Phoenix's RTCP packet, decrypted. Only its first 8 bytes travel in the
/// clear.
fn open_rtcp(cipher: &Aes256Gcm, packet: &[u8]) -> Option<Vec<u8>> {
   let body_end = packet.len().checked_sub(TAG_LEN + NONCE_TAIL)?;
   if body_end < RTCP_HEADER {
      return None;
   }
   let mut nonce = [0u8; 12];
   nonce[..NONCE_TAIL].copy_from_slice(&packet[packet.len() - NONCE_TAIL..]);
   let mut plain = packet[RTCP_HEADER..body_end].to_vec();
   let tag = Tag::from_slice(&packet[body_end..body_end + TAG_LEN]);
   cipher
      .decrypt_in_place_detached(Nonce::from_slice(&nonce), &packet[..RTCP_HEADER], &mut plain, tag)
      .ok()?;
   let mut out = packet[..RTCP_HEADER].to_vec();
   out.extend_from_slice(&plain);
   Some(out)
}

/// The SSRCs a compound RTCP packet asks keyframes for (PLI or FIR).
fn keyframe_requests(compound: &[u8]) -> Vec<u32> {
   let mut found = Vec::new();
   let mut rest = compound;
   while let [first, kind, high, low, ..] = *rest {
      let len = (usize::from(u16::from_be_bytes([high, low])) + 1) * 4;
      let Some(packet) = rest.get(..len) else {
         break;
      };
      let at = match first & 0x1F {
         PLI => 8,
         FIR => 12,
         _ => usize::MAX,
      };
      if kind == PSFB
         && let Some(ssrc) = packet.get(at..at.saturating_add(4)).and_then(|bytes| bytes.first_chunk::<4>())
      {
         found.push(u32::from_be_bytes(*ssrc));
      }
      rest = &rest[len..];
   }
   found
}

/// A compound RTCP packet asking `media` for a keyframe: an empty receiver
/// report first, as compound packets must start with one, then a PLI and a
/// FIR.
fn keyframe_request(sender: u32, media: u32, fir_seq: u8) -> Vec<u8> {
   let mut packet = Vec::with_capacity(40);
   packet.extend([0x80, 201, 0, 1]);
   packet.extend(sender.to_be_bytes());
   packet.extend([0x80 | PLI, PSFB, 0, 2]);
   packet.extend(sender.to_be_bytes());
   packet.extend(media.to_be_bytes());
   packet.extend([0x80 | FIR, PSFB, 0, 4]);
   packet.extend(sender.to_be_bytes());
   packet.extend([0; 4]);
   packet.extend(media.to_be_bytes());
   packet.extend([fir_seq, 0, 0, 0]);
   packet
}

fn seal(cipher: &Aes256Gcm, header: &[u8], payload: &[u8], counter: u32) -> Option<Vec<u8>> {
   let mut nonce = [0u8; 12];
   nonce[..4].copy_from_slice(&counter.to_be_bytes());
   let mut body = payload.to_vec();
   let tag = cipher
      .encrypt_in_place_detached(Nonce::from_slice(&nonce), header, &mut body)
      .ok()?;
   let mut out = Vec::with_capacity(header.len() + body.len() + TAG_LEN + NONCE_TAIL);
   out.extend_from_slice(header);
   out.extend_from_slice(&body);
   out.extend_from_slice(&tag);
   out.extend_from_slice(&nonce[..NONCE_TAIL]);
   Some(out)
}

/// A full answer from the compact one Spacebar sends: its address, ICE
/// credentials, fingerprint and candidates, around the media we offered.
fn answer_sdp(compact: &str, offer: &str, audio: Mid, video: Option<(Mid, VideoCodec)>) -> String {
   let lines: Vec<&str> = compact.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
   let find = |prefix: &str| lines.iter().copied().find(|line| line.starts_with(prefix));
   let port = find("m=audio ")
      .and_then(|line| line.split(' ').nth(1))
      .unwrap_or("9");
   let ip = find("c=")
      .and_then(|line| line.split_once("IN IP4 "))
      .map_or("0.0.0.0", |(_, ip)| ip.trim());
   let bundle = video.map_or_else(|| audio.to_string(), |(video, _)| format!("{audio} {video}"));
   let mut out = vec![
      "v=0".to_owned(),
      "o=- 0 0 IN IP4 127.0.0.1".to_owned(),
      "s=-".to_owned(),
      "t=0 0".to_owned(),
      format!("a=group:BUNDLE {bundle}"),
      "a=msid-semantic: WMS".to_owned(),
   ];
   let section = |out: &mut Vec<String>, kind: &str, formats: &str, mid: Mid| {
      out.push(format!("m={kind} {port} UDP/TLS/RTP/SAVPF {formats}"));
      out.push(format!("c=IN IP4 {ip}"));
      out.push(format!("a=rtcp:{port} IN IP4 {ip}"));
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
      out.extend(offered_extmaps(offer, kind));
   };
   section(&mut out, "audio", &OPUS.to_string(), audio);
   out.push(format!("a=rtpmap:{OPUS} opus/48000/2"));
   out.push(format!("a=fmtp:{OPUS} minptime=10;useinbandfec=1"));
   out.extend(lines.iter().filter(|line| line.starts_with("a=candidate:")).map(|line| (*line).to_owned()));
   out.push("a=end-of-candidates".to_owned());
   if let Some((mid, VideoCodec { pt, rtx })) = video {
      let formats = rtx.map_or_else(|| pt.to_string(), |rtx| format!("{pt} {rtx}"));
      section(&mut out, "video", &formats, mid);
      out.extend([
         format!("a=rtpmap:{pt} H264/90000"),
         format!("a=rtcp-fb:{pt} nack"),
         format!("a=rtcp-fb:{pt} nack pli"),
         format!("a=rtcp-fb:{pt} ccm fir"),
         format!("a=fmtp:{pt} level-asymmetry-allowed=1;packetization-mode=1;profile-level-id={H264_PROFILE:06x}"),
      ]);
      if let Some(rtx) = rtx {
         out.push(format!("a=rtpmap:{rtx} rtx/90000"));
         out.push(format!("a=fmtp:{rtx} apt={pt}"));
      }
   }
   out.push(String::new());
   out.join("\r\n")
}

/// The header extensions the voice server keeps from our offer of `kind`.
fn offered_extmaps(offer: &str, kind: &str) -> Vec<String> {
   let wanted: &[&str] = if kind == "audio" {
      &["ssrc-audio-level", "transport-wide-cc"]
   } else {
      &["abs-send-time", "toffset", "playout-delay", "transport-wide-cc"]
   };
   let mut inside = false;
   offer
      .lines()
      .filter(|line| {
         if let Some(media) = line.strip_prefix("m=") {
            inside = media.starts_with(kind);
            return false;
         }
         inside && line.starts_with("a=extmap:") && wanted.iter().any(|uri| line.contains(uri))
      })
      .map(str::to_owned)
      .collect()
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
      assert_eq!(voice_url("192.168.0.4:3004"), "ws://192.168.0.4:3004/?v=9");
      assert_eq!(voice_url("voice.example.org"), "wss://voice.example.org/?v=9");
      assert_eq!(voice_url("8.8.8.8:443"), "wss://8.8.8.8:443/?v=9");
   }

   #[test]
   fn seals_and_opens_phoenix_packets() {
      let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&[7; 32]));
      let mut header = [0u8; 12];
      header[0] = 0x80;
      header[1] = 0x80 | PHOENIX_OPUS;
      header[4..8].copy_from_slice(&1234u32.to_be_bytes());
      let opened = |payload: &[u8], transport_seq| Opened {
         payload: payload.to_vec(),
         timestamp: 1234,
         marker: true,
         transport_seq,
      };
      let sealed = seal(&cipher, &header, b"opus", 9).expect("seal");
      assert_eq!(open(&cipher, &sealed), Some(opened(b"opus", None)));
      header[0] |= 0x20;
      let padded = seal(&cipher, &header, b"opus\0\0\x03", 10).expect("seal");
      assert_eq!(open(&cipher, &padded), Some(opened(b"opus", None)));
      let only_padding = seal(&cipher, &header, b"\0\x02", 11).expect("seal");
      assert_eq!(open(&cipher, &only_padding), Some(opened(b"", None)));
      let mut extended = header.to_vec();
      extended[0] = 0x90;
      extended.extend([0xBE, 0xDE, 0, 1]);
      let sealed = seal(&cipher, &extended, b"\x51\x02\x28\0h264", 12).expect("seal");
      assert_eq!(open(&cipher, &sealed), Some(opened(b"h264", Some(0x0228))));
   }

   #[test]
   fn reads_the_offered_ssrc() {
      let sdp = "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=ssrc:2591998996 cname:abc\r\n";
      assert_eq!(offered_ssrc(sdp), Some(2_591_998_996));
      assert_eq!(offered_ssrc("m=audio 9\r\n"), None);
   }

   #[test]
   fn picks_phoenix_h264() {
      let codecs = json!([
         { "name": "opus", "payload_type": 120 },
         { "name": "H264", "payload_type": 101, "rtx_payload_type": 102 },
      ]);
      let codec = VideoCodec::from_codecs(Some(&codecs)).expect("h264");
      assert_eq!((codec.pt, codec.rtx), (101, Some(102)));
      assert!(VideoCodec::from_codecs(Some(&json!([{ "name": "H264", "payload_type": 111 }]))).is_none());
   }

   #[test]
   fn negotiates_video() {
      let codec = VideoCodec { pt: 101, rtx: Some(102) };
      let mut config = Rtc::builder()
         .set_rtp_mode(true)
         .clear_codecs()
         .enable_opus(true, false)
         .set_crypto_provider(Arc::new(str0m_rust_crypto::default_provider()));
      config.codec_config().add_h264(Pt::from(101), Some(Pt::from(102)), true, H264_PROFILE);
      let mut rtc = config.build(Instant::now());
      let mut change = rtc.sdp_api();
      let audio = change.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
      let video = change.add_media(MediaKind::Video, Direction::SendRecv, None, None, None);
      let (offer, pending) = change.apply().expect("offer");
      let compact = "m=audio 3984 ICE/SDP\na=fingerprint:sha-256 \
         AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA:AA\n\
         c=IN IP4 192.0.2.1\na=ice-ufrag:abcd\na=ice-pwd:abcdefghijklmnopqrstuv\n\
         a=candidate:1 1 udp 2130706431 192.0.2.1 3984 typ host\n";
      let sdp = answer_sdp(compact, &offer.to_sdp_string(), audio, Some((video, codec)));
      let answer = SdpAnswer::from_sdp_string(&sdp).expect("answer parses");
      rtc.sdp_api().accept_answer(pending, answer).expect("answer applies");
      let mut direct = rtc.direct_api();
      let stream = direct.stream_tx_by_mid(video, None).expect("video stream");
      assert!(stream.rtx().is_some());
   }

   #[test]
   fn finds_keyframe_requests() {
      let mut compound = vec![0x81, 201, 0, 1, 0, 0, 0, 1];
      compound.extend([0x81, PSFB, 0, 2, 0, 0, 0, 1, 0, 0, 0, 9]);
      compound.extend([0x84, PSFB, 0, 4, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 7, 1, 0, 0, 0]);
      compound.extend([0x81, PSFB, 0, 9]);
      assert_eq!(keyframe_requests(&compound), vec![9, 7]);
      assert_eq!(keyframe_requests(&keyframe_request(1, 5, 3)), vec![5, 5]);
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

