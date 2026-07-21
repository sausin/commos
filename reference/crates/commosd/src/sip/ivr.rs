//! IVR media session (Volume 7) — the "menu/prompt runtime": play a prompt over RTP, collect
//! a DTMF digit, and resolve it to a destination.
//!
//! This is the media-plane execution of an [`Ivr`](commos_core::entities::ivr::Ivr) node.
//! [`run_ivr`] owns a UDP RTP socket: it latches onto the caller (symmetric RTP), streams the
//! prompt as PCMU frames ([`g711`]/20 ms), and concurrently decodes DTMF from the inbound
//! stream — both RFC 4733 telephone-events ([`dtmf`]) and out-of-band SIP INFO digits injected
//! on a channel. Barge-in is supported (a digit pressed during the prompt returns immediately).
//! On an unmatched digit or timeout it applies the node's `invalid_action` (repeat/hangup/route)
//! up to a small attempt cap.

use std::collections::HashMap;
use std::net::SocketAddr;

use tokio::net::UdpSocket;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::watch;
use tokio::time::{Duration, Instant};

use super::dtmf::{decode_telephone_event, DtmfCollector};
use super::rtp::Capture;

/// One PCMU RTP frame is 20 ms == 160 μ-law bytes at 8 kHz.
const FRAME_BYTES: usize = 160;
const FRAME_INTERVAL: Duration = Duration::from_millis(20);
/// A fixed SSRC for the reference's outbound stream (single media source per session).
const SSRC: u32 = 0x00C0_FFEE;
/// Default attempt cap for a `repeat` invalid_action, so a wrong-key loop always terminates.
const DEFAULT_MAX_ATTEMPTS: u32 = 3;
/// Fixed RTP header length (no CSRC/extension — the common case for G.711 desk phones).
const RTP_HEADER_LEN: usize = 12;

/// What an IVR node needs to run: the prompt audio, the DTMF payload type, the digit→
/// destination map, and the timeout / invalid-input behaviour.
pub struct IvrConfig {
    /// Prompt audio as μ-law (`audio/basic`) — a recorded prompt Object, or synthesised tone.
    pub prompt: Vec<u8>,
    /// The negotiated `telephone-event` RTP payload type (from the SDP answer).
    pub te_pt: u8,
    /// `digit → destination_ref` (e.g. `'1' → "voicemail"`).
    pub options: HashMap<char, String>,
    /// Digit-collection window per attempt.
    pub timeout: Duration,
    /// What to do on an unmatched digit / no input: `repeat`, `hangup`, or a `destination_ref`.
    pub invalid_action: String,
    /// Max prompt+collect attempts before giving up (bounds a `repeat` loop).
    pub max_attempts: u32,
}

impl IvrConfig {
    /// Build a config from an [`Ivr`](commos_core::entities::ivr::Ivr)'s wire fields: the
    /// `options` object (`digit → destination_ref`), `timeout_ms` (default 5 s), and
    /// `invalid_action` (default `repeat`). `prompt` and `te_pt` come from the media layer.
    pub fn from_ivr(
        prompt: Vec<u8>,
        te_pt: u8,
        options: &serde_json::Value,
        timeout_ms: Option<i64>,
        invalid_action: Option<&str>,
    ) -> Self {
        let mut map = HashMap::new();
        if let Some(obj) = options.as_object() {
            for (k, v) in obj {
                if let (Some(digit), Some(dest)) = (k.chars().next(), v.as_str()) {
                    if k.chars().count() == 1 {
                        map.insert(digit, dest.to_string());
                    }
                }
            }
        }
        let timeout = Duration::from_millis(timeout_ms.filter(|&t| t > 0).unwrap_or(5000) as u64);
        IvrConfig {
            prompt,
            te_pt,
            options: map,
            timeout,
            invalid_action: invalid_action.unwrap_or("repeat").to_string(),
            max_attempts: DEFAULT_MAX_ATTEMPTS,
        }
    }
}

/// The result of running an IVR node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IvrOutcome {
    /// A digit matched an option (or `invalid_action` named a destination): route the caller.
    Selected { digit: char, destination: String },
    /// The caller pressed nothing within the window (and `invalid_action` was not a route).
    Timeout,
    /// The caller pressed unmatched digits and `invalid_action` resolved to hangup.
    Invalid,
}

/// The result of a completed IVR session: the [`IvrOutcome`] plus the caller's latched RTP
/// address (so a follow-on action — e.g. a voicemail beep — can keep talking to the caller).
pub struct IvrResult {
    pub outcome: IvrOutcome,
    pub peer: Option<SocketAddr>,
}

/// Run the IVR menu on `sock`: play the prompt, collect a digit, and resolve it — repeating on
/// invalid input per `invalid_action` up to `max_attempts`. Injected SIP INFO digits arrive on
/// `info_rx`. Returns the outcome and the caller's latched address.
pub async fn run_ivr(
    sock: &UdpSocket,
    cfg: &IvrConfig,
    info_rx: &mut UnboundedReceiver<char>,
) -> IvrResult {
    let mut peer: Option<SocketAddr> = None;
    let mut outcome = IvrOutcome::Timeout;
    for _ in 0..cfg.max_attempts.max(1) {
        match play_and_collect(sock, &cfg.prompt, cfg.te_pt, cfg.timeout, info_rx, &mut peer).await {
            Some(digit) => {
                outcome = if let Some(dest) = cfg.options.get(&digit) {
                    IvrOutcome::Selected { digit, destination: dest.clone() }
                } else {
                    // Unmatched digit → apply invalid_action.
                    match cfg.invalid_action.as_str() {
                        "repeat" => continue,
                        "hangup" => IvrOutcome::Invalid,
                        dest => IvrOutcome::Selected { digit, destination: dest.to_string() },
                    }
                };
                break;
            }
            None => {
                // No input this attempt: repeat replays the prompt; anything else ends here.
                if cfg.invalid_action == "repeat" {
                    continue;
                }
                outcome = IvrOutcome::Timeout;
                break;
            }
        }
    }
    IvrResult { outcome, peer }
}

/// One attempt: latch the caller (persisting `peer` across attempts), stream `prompt` as PCMU
/// RTP while decoding inbound DTMF, and return the first digit collected (in-band or injected)
/// within `timeout`, else `None`.
async fn play_and_collect(
    sock: &UdpSocket,
    prompt: &[u8],
    te_pt: u8,
    timeout: Duration,
    info_rx: &mut UnboundedReceiver<char>,
    peer: &mut Option<SocketAddr>,
) -> Option<char> {
    let frames: Vec<&[u8]> = if prompt.is_empty() {
        Vec::new()
    } else {
        prompt.chunks(FRAME_BYTES).collect()
    };
    let mut frame_idx = 0usize;
    let mut seq: u16 = 0;
    let mut ts: u32 = 0;
    let mut first_frame = true;
    let mut collector = DtmfCollector::new();
    let mut buf = [0u8; 2048];

    let mut ticker = tokio::time::interval(FRAME_INTERVAL);
    let deadline = tokio::time::sleep_until(Instant::now() + timeout);
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            _ = &mut deadline => return None,
            // Out-of-band SIP INFO digit.
            Some(d) = info_rx.recv() => return Some(d),
            // Inbound RTP: latch the caller, decode DTMF.
            r = sock.recv_from(&mut buf) => {
                if let Ok((n, from)) = r {
                    peer.get_or_insert(from);
                    if let Some(ev) = decode_telephone_event(&buf[..n], te_pt) {
                        if let Some(d) = collector.push(ev) {
                            return Some(d);
                        }
                    }
                }
            }
            // Playout tick: once latched, send the next prompt frame (if any remain).
            _ = ticker.tick() => {
                if let (Some(dst), true) = (*peer, frame_idx < frames.len()) {
                    let payload = frames[frame_idx];
                    let pkt = rtp_frame(seq, ts, payload, first_frame);
                    let _ = sock.send_to(&pkt, dst).await;
                    seq = seq.wrapping_add(1);
                    ts = ts.wrapping_add(payload.len() as u32);
                    frame_idx += 1;
                    first_frame = false;
                }
            }
        }
    }
}

/// Play `audio` (μ-law) to `peer` as PCMU RTP at 20 ms cadence, returning once every frame is
/// sent. Used for the voicemail "leave a message" beep after an IVR selects voicemail.
pub async fn play(sock: &UdpSocket, peer: SocketAddr, audio: &[u8]) {
    let mut seq: u16 = 0;
    let mut ts: u32 = 0;
    let mut first = true;
    let mut ticker = tokio::time::interval(FRAME_INTERVAL);
    for chunk in audio.chunks(FRAME_BYTES) {
        ticker.tick().await;
        let _ = sock.send_to(&rtp_frame(seq, ts, chunk, first), peer).await;
        seq = seq.wrapping_add(1);
        ts = ts.wrapping_add(chunk.len() as u32);
        first = false;
    }
}

/// Capture the caller's inbound PCMU audio (payload, RTP header stripped) into `capture` until
/// `stop` is signalled or `max` elapses — the voicemail recording phase after an IVR menu.
/// Telephone-event (DTMF) packets are skipped. Mirrors the recording capture path so hangup
/// (graceful `stop`) persists whatever was captured.
pub async fn record_until_stop(
    sock: &UdpSocket,
    te_pt: u8,
    capture: &Capture,
    stop: &mut watch::Receiver<bool>,
    max: Duration,
) {
    let mut buf = [0u8; 2048];
    let deadline = tokio::time::sleep_until(Instant::now() + max);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => return,
            _ = stop.changed() => return,
            r = sock.recv_from(&mut buf) => {
                let Ok((n, _from)) = r else { return };
                // Skip DTMF telephone-events; capture PCMU payload only.
                if super::dtmf::rtp_payload_type(&buf[..n]) == Some(te_pt) {
                    continue;
                }
                if n > RTP_HEADER_LEN {
                    let mut g = capture.lock().expect("capture mutex");
                    g.extend_from_slice(&buf[RTP_HEADER_LEN..n]);
                }
            }
        }
    }
}

/// Build a PCMU (payload type 0) RTP packet with a 12-byte header. `marker` is set on the
/// first packet of the talkspurt (RFC 3550 §5.1).
fn rtp_frame(seq: u16, ts: u32, payload: &[u8], marker: bool) -> Vec<u8> {
    let mut p = Vec::with_capacity(12 + payload.len());
    p.push(0x80); // version 2, no padding/extension/CSRC
    p.push(if marker { 0x80 } else { 0x00 }); // marker + payload type 0 (PCMU)
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(&ts.to_be_bytes());
    p.extend_from_slice(&SSRC.to_be_bytes());
    p.extend_from_slice(payload);
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sip::dtmf::TELEPHONE_EVENT_PT;
    use tokio::sync::mpsc;

    /// A telephone-event RTP packet for `event`, at the negotiated payload type.
    fn te_packet(event: u8, end: bool) -> Vec<u8> {
        let mut p = vec![0x80u8, TELEPHONE_EVENT_PT & 0x7f, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0];
        p.extend_from_slice(&[event, if end { 0x80 } else { 0 }, 0, 160]);
        p
    }

    fn cfg(options: &[(char, &str)], invalid: &str) -> IvrConfig {
        IvrConfig {
            prompt: crate::sip::g711::beep(60), // short prompt so tests are fast
            te_pt: TELEPHONE_EVENT_PT,
            options: options.iter().map(|(d, r)| (*d, r.to_string())).collect(),
            timeout: Duration::from_millis(600),
            invalid_action: invalid.to_string(),
            max_attempts: 2,
        }
    }

    #[tokio::test]
    async fn plays_prompt_and_collects_matched_digit() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = server.local_addr().unwrap().port();
        let phone = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let srv_addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

        let config = cfg(&[('1', "voicemail"), ('2', "queue:sales")], "repeat");
        let (_tx, mut rx) = mpsc::unbounded_channel::<char>();

        // Drive the IVR on the server socket.
        let handle = tokio::spawn(async move { run_ivr(&server, &config, &mut rx).await.outcome });

        // Latch the session, then press "1".
        phone.send_to(&[0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF], srv_addr).await.unwrap();
        // Give the session a moment, then confirm we receive prompt audio (playout).
        let mut buf = [0u8; 2048];
        let got_audio = tokio::time::timeout(Duration::from_millis(200), phone.recv_from(&mut buf))
            .await
            .is_ok();
        assert!(got_audio, "caller should receive prompt RTP frames");
        // Press "1": a burst ending with the End bit.
        for end in [false, true] {
            phone.send_to(&te_packet(1, end), srv_addr).await.unwrap();
        }

        let outcome = handle.await.unwrap();
        assert_eq!(
            outcome,
            IvrOutcome::Selected { digit: '1', destination: "voicemail".into() }
        );
    }

    #[tokio::test]
    async fn injected_info_digit_is_collected() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let config = cfg(&[('9', "hangup")], "hangup");
        let (tx, mut rx) = mpsc::unbounded_channel::<char>();
        tx.send('9').unwrap(); // SIP INFO digit arrives immediately
        let outcome = run_ivr(&server, &config, &mut rx).await.outcome;
        assert_eq!(outcome, IvrOutcome::Selected { digit: '9', destination: "hangup".into() });
    }

    #[tokio::test]
    async fn no_input_with_hangup_action_times_out() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let config = cfg(&[('1', "voicemail")], "hangup"); // no repeat → single attempt
        let (_tx, mut rx) = mpsc::unbounded_channel::<char>();
        // No caller, no digit → the collection window expires.
        let outcome = run_ivr(&server, &config, &mut rx).await.outcome;
        assert_eq!(outcome, IvrOutcome::Timeout);
    }

    #[test]
    fn from_ivr_parses_options_and_defaults() {
        let opts = serde_json::json!({"1": "voicemail", "2": "queue:sales", "invalid": "x"});
        let cfg = IvrConfig::from_ivr(vec![], TELEPHONE_EVENT_PT, &opts, None, None);
        assert_eq!(cfg.options.get(&'1').map(String::as_str), Some("voicemail"));
        assert_eq!(cfg.options.get(&'2').map(String::as_str), Some("queue:sales"));
        // Multi-char keys are ignored (not single digits).
        assert!(!cfg.options.contains_key(&'i'));
        assert_eq!(cfg.timeout, Duration::from_millis(5000)); // default
        assert_eq!(cfg.invalid_action, "repeat"); // default
    }
}
