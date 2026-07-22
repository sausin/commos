//! Music on hold — a looping audio source streamed to a held (or waiting) caller.
//!
//! Today a held call hears whatever silence the held phone emits (see `TODO.md`); this
//! provides a real hold-music source. It loads operator-supplied G.711 μ-law files from
//! `{data_dir}/moh` (the same headerless `.ulaw` format as the IVR/voicemail prompts), and if
//! none are present synthesises a gentle, non-silent tone loop so hold is never dead air.
//!
//! The source is loaded once at startup and shared behind an [`std::sync::Arc`]. The audio is
//! stored as μ-law (CommOS's storage codec) and transcoded to the leg's negotiated codec at
//! stream time, exactly like the prompt path. [`MohSource::stream_until`] plays the loop to a
//! latched peer on a 20 ms cadence until a stop signal fires — the same mechanism the ringback
//! loop and `ivr::play` use, so it is the reusable engine for both hold and (later) queue-wait
//! treatment.
//!
//! NOTE: the streaming half of this engine ([`MohSource::stream_until`] and friends) is
//! complete and tested but not yet spliced into the live two-leg hold bridge — that requires a
//! bridge-relay "play a source to a held leg" mode and is a documented media-plane follow-up.
//! The source is loaded and available; `#[allow(dead_code)]` marks the not-yet-wired paths.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::watch;

use super::g711::{self, G711};
use super::ivr;

/// 20 ms of 8 kHz G.711 = 160 samples/bytes — the frame size the whole media plane uses.
const FRAME_BYTES: usize = 160;
#[allow(dead_code)] // used by stream_until (wired when hold/queue injection lands)
const FRAME_INTERVAL: Duration = Duration::from_millis(20);

/// A looping hold-music source. The stored `ulaw` buffer is a whole number of 20 ms frames
/// (padded with silence if needed) and is never empty, so streaming can always cycle it.
#[derive(Clone, Debug)]
pub struct MohSource {
    ulaw: Vec<u8>,
    /// Whether this source was synthesised (no operator files found) — surfaced for logging.
    pub synthesised: bool,
}

impl MohSource {
    /// Load hold music from `dir`: concatenate every `*.ulaw` file (sorted by name for a
    /// deterministic playlist) into one loop. If the directory is missing or empty, fall back
    /// to [`MohSource::synth`] so hold is never silent.
    pub fn load(dir: &str) -> Self {
        let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("ulaw"))
            .collect();
        files.sort();

        let mut ulaw = Vec::new();
        for path in &files {
            if let Ok(bytes) = std::fs::read(path) {
                ulaw.extend_from_slice(&bytes);
            }
        }
        if ulaw.is_empty() {
            return Self::synth();
        }
        Self { ulaw: pad_to_frame(ulaw), synthesised: false }
    }

    /// Synthesise a gentle, endlessly-loopable hold tune: a slow, low-amplitude arpeggio of
    /// mellow tones with brief gaps, so a caller on hold hears *something* pleasant rather than
    /// dead air. Deterministic (no RNG), a few seconds long.
    pub fn synth() -> Self {
        // A simple major-triad arpeggio (A3, C#4, E4, C#4) at modest amplitude, each note a
        // fifth of a second with a short rest, μ-law encoded — calm and clearly non-silent.
        const AMP: i16 = 6000;
        const NOTE_MS: usize = 220;
        const REST_MS: usize = 60;
        let notes = [220.0_f64, 277.18, 329.63, 277.18];
        let mut ulaw = Vec::new();
        for &f in notes.iter() {
            ulaw.extend_from_slice(&g711::tone(f, NOTE_MS, AMP, G711::Ulaw));
            ulaw.extend_from_slice(&g711::tone(0.0, REST_MS, 0, G711::Ulaw)); // silence gap
        }
        Self { ulaw: pad_to_frame(ulaw), synthesised: true }
    }

    /// The raw μ-law loop (whole 20 ms frames, non-empty).
    #[allow(dead_code)] // accessor for the streaming/injection path + tests
    pub fn ulaw(&self) -> &[u8] {
        &self.ulaw
    }

    /// Duration of one loop in milliseconds.
    pub fn loop_ms(&self) -> usize {
        self.ulaw.len() / FRAME_BYTES * 20
    }

    /// The loop transcoded to `codec` (μ-law passthrough, A-law converted), ready to packetise.
    #[allow(dead_code)] // used by stream_until (wired when hold/queue injection lands)
    pub fn for_codec(&self, codec: G711) -> Vec<u8> {
        g711::transcode_ulaw(&self.ulaw, codec)
    }

    /// Stream the hold loop to `peer` on a 20 ms cadence in the negotiated `codec`/`pt`, looping
    /// forever until `stop` flips to `true` (or the receiver is dropped). Reuses
    /// [`ivr::rtp_frame`] for RTP packetisation, mirroring the ringback/`play` loops.
    #[allow(dead_code)] // spliced into the held-leg bridge / queue-wait loop (follow-up)
    pub async fn stream_until(
        &self,
        sock: &UdpSocket,
        peer: SocketAddr,
        pt: u8,
        codec: G711,
        mut stop: watch::Receiver<bool>,
    ) {
        let audio = self.for_codec(codec);
        if audio.is_empty() {
            return;
        }
        let frames: Vec<&[u8]> = audio.chunks(FRAME_BYTES).collect();
        let mut ticker = tokio::time::interval(FRAME_INTERVAL);
        let mut seq: u16 = 0;
        let mut ts: u32 = 0;
        let mut idx = 0usize;
        loop {
            tokio::select! {
                _ = stop.changed() => {
                    if *stop.borrow() { break; }
                }
                _ = ticker.tick() => {
                    let frame = frames[idx % frames.len()];
                    // Marker bit on the very first packet of the stream (talk-spurt start).
                    let pkt = ivr::rtp_frame(pt, seq, ts, frame, seq == 0);
                    if sock.send_to(&pkt, peer).await.is_err() {
                        break;
                    }
                    seq = seq.wrapping_add(1);
                    ts = ts.wrapping_add(FRAME_BYTES as u32);
                    idx += 1;
                }
            }
        }
    }
}

/// Pad a μ-law buffer up to a whole number of 20 ms frames with μ-law silence, so it always
/// packetises into complete frames. A non-empty buffer stays non-empty.
fn pad_to_frame(mut ulaw: Vec<u8>) -> Vec<u8> {
    let rem = ulaw.len() % FRAME_BYTES;
    if rem != 0 {
        let silence = G711::Ulaw.silence();
        ulaw.resize(ulaw.len() + (FRAME_BYTES - rem), silence);
    }
    if ulaw.is_empty() {
        // Guarantee the non-empty invariant even for an all-empty input.
        ulaw = vec![G711::Ulaw.silence(); FRAME_BYTES];
    }
    ulaw
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synth_is_non_silent_and_frame_aligned() {
        let m = MohSource::synth();
        assert!(m.synthesised);
        assert!(!m.ulaw().is_empty());
        assert_eq!(m.ulaw().len() % FRAME_BYTES, 0, "whole frames");
        // Not all silence — at least some samples differ from the μ-law zero level.
        let silence = G711::Ulaw.silence();
        assert!(m.ulaw().iter().any(|&b| b != silence), "synth MoH must be audible");
        assert!(m.loop_ms() >= 500, "a usable loop length");
    }

    #[test]
    fn pad_rounds_up_to_frame_boundary() {
        assert_eq!(pad_to_frame(vec![]).len(), FRAME_BYTES);
        assert_eq!(pad_to_frame(vec![1; 10]).len(), FRAME_BYTES);
        assert_eq!(pad_to_frame(vec![1; FRAME_BYTES]).len(), FRAME_BYTES);
        assert_eq!(pad_to_frame(vec![1; FRAME_BYTES + 1]).len(), FRAME_BYTES * 2);
        // Padding is silence.
        let padded = pad_to_frame(vec![1; 10]);
        assert_eq!(padded[FRAME_BYTES - 1], G711::Ulaw.silence());
    }

    #[test]
    fn for_codec_transcodes_or_passes_through() {
        let m = MohSource::synth();
        // μ-law is a passthrough of the same bytes.
        assert_eq!(m.for_codec(G711::Ulaw), m.ulaw());
        // A-law keeps the same length (byte-for-byte transcode).
        assert_eq!(m.for_codec(G711::Alaw).len(), m.ulaw().len());
    }

    #[test]
    fn load_missing_dir_falls_back_to_synth() {
        let m = MohSource::load("/nonexistent/commos/moh/dir");
        assert!(m.synthesised);
        assert!(!m.ulaw().is_empty());
    }

    #[test]
    fn load_concatenates_ulaw_files_in_name_order() {
        // Write two throwaway .ulaw files + a non-ulaw file that must be ignored.
        let dir = std::env::temp_dir().join(format!("commos-moh-test-{}", commos_core::common::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("b.ulaw"), vec![0x22; FRAME_BYTES]).unwrap();
        std::fs::write(dir.join("a.ulaw"), vec![0x11; FRAME_BYTES]).unwrap();
        std::fs::write(dir.join("notes.txt"), b"ignore me").unwrap();

        let m = MohSource::load(dir.to_str().unwrap());
        assert!(!m.synthesised, "operator files present");
        // Sorted: a.ulaw (0x11) then b.ulaw (0x22).
        assert_eq!(m.ulaw().len(), FRAME_BYTES * 2);
        assert_eq!(m.ulaw()[0], 0x11);
        assert_eq!(m.ulaw()[FRAME_BYTES], 0x22);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn short_file_is_padded_to_a_frame() {
        let dir = std::env::temp_dir().join(format!("commos-moh-pad-{}", commos_core::common::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.ulaw"), vec![0x33; 40]).unwrap(); // less than a frame
        let m = MohSource::load(dir.to_str().unwrap());
        assert_eq!(m.ulaw().len(), FRAME_BYTES);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn stream_until_sends_frames_then_stops() {
        // A receiver socket to catch the streamed RTP; the source streams until we signal stop.
        let recv = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let recv_addr = recv.local_addr().unwrap();
        let send = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let (stop_tx, stop_rx) = watch::channel(false);
        let moh = MohSource::synth();
        let streamer = tokio::spawn(async move {
            moh.stream_until(&send, recv_addr, G711::Ulaw.payload_type(), G711::Ulaw, stop_rx).await;
        });

        // Receive a couple of frames to prove the stream is live and well-formed RTP.
        let mut buf = [0u8; 512];
        let n = recv.recv(&mut buf).await.unwrap();
        assert!(n >= 12 + FRAME_BYTES, "RTP header + a 160-byte payload");
        assert_eq!(buf[0], 0x80, "RTP v2, no padding/extension");
        assert_eq!(buf[1] & 0x7f, G711::Ulaw.payload_type(), "payload type in the packet");

        stop_tx.send(true).unwrap();
        // The streamer observes the stop and returns.
        tokio::time::timeout(Duration::from_secs(1), streamer).await.unwrap().unwrap();
    }
}
