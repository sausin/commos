//! SDP/SRTP negotiation and SIP response construction (ringing / 200 OK) helpers.

use super::*;

impl SipServer {
    /// Build the SDP answer advertising `rtp_port` for the negotiated `audio` codec + DTMF
    /// payload type `te_pt` (see [`media_sdp`]). Passes `crypto` through so an SRTP-negotiated
    /// endpoint answers over `RTP/SAVP` with its SDES key.
    pub(super) fn build_sdp(
        &self,
        rtp_port: u16,
        audio: &codec::Codec,
        te_pt: u8,
        crypto: Option<&sdes::CryptoAttr>,
    ) -> String {
        media_sdp(self.media_ip, rtp_port, audio, te_pt, crypto)
    }

    /// Negotiate SRTP for an endpoint media path from the caller's SDP `body`: when SRTP is
    /// enabled and the caller offers the secure profile with a supported SDES key, return the
    /// `a=crypto` line to advertise back (a fresh CommOS key) and the [`srtp::SrtpSession`] that
    /// keys the media task — `inbound` from the caller's key, `outbound` from ours. `None` when
    /// SRTP is off or the caller offered plain RTP (which is then answered in the clear).
    pub(super) fn negotiate_srtp(
        &self,
        body: &str,
        secure: bool,
    ) -> Option<(sdes::CryptoAttr, srtp::SrtpSession)> {
        Some(srtp_answer(&self.caller_crypto(body, secure)?))
    }

    /// The caller's SDES key from its SDP `body`, when SRTP is enabled, the caller offered the
    /// secure profile, AND the signalling arrived over a confidential (TLS) transport. SDES puts
    /// the master key in the SDP, so accepting it over plaintext UDP would hand the key to any
    /// passive observer of the signalling — no real confidentiality. Over cleartext transports we
    /// therefore decline SRTP and answer plain RTP rather than pretend to encrypt. `None` for a
    /// plain-RTP caller or an insecure transport.
    pub(super) fn caller_crypto(&self, body: &str, secure: bool) -> Option<sdes::CryptoAttr> {
        (self.srtp_enabled && secure && sdes::offers_savp(body))
            .then(|| sdes::CryptoAttr::from_sdp(body))
            .flatten()
    }

    /// Mark the Call ANSWERED at the instant CommOS answers the caller with 200 OK — the true
    /// connect time. Called from every answer path (bridge, trunk, voicemail, echo, IVR) just
    /// before the 200 OK goes out. Best-effort: a failure is logged, never fatal to the call
    /// (an already-answered Call — e.g. an IVR that then bridges — simply logs an illegal
    /// transition, which is harmless).
    pub(super) async fn mark_answered(&self, call_id: Uuid) {
        if let Err(e) = self
            .routing
            .apply_fact(MediaFact::Answered {
                tenant_id: self.default_tenant,
                call_id,
                answered_at: Timestamp::now(),
            })
            .await
        {
            tracing::debug!(error = %e, %call_id, "marking call answered failed");
        }
    }

    /// Capture the caller leg's dialog identifiers from its INVITE, so a callee-originated BYE can
    /// be propagated back to the caller (tearing its phone down too). CommOS is the UAS on this
    /// leg: our local identity is the caller's `To` plus the tag we answered with, the remote is
    /// the caller's `From`, and the BYE is sent to the caller's actual socket (`src`) — reliable on
    /// UDP even behind NAT. Uses the caller's `Contact` as the request-URI, falling back to `From`.
    pub(super) fn caller_leg(&self, msg: &SipMessage, call_id: Uuid, src: SocketAddr) -> CalleeLeg {
        let our_tag: String = call_id
            .to_string()
            .chars()
            .filter(|c| *c != '-')
            .take(16)
            .collect();
        let from = match msg.header("To") {
            Some(to) if msg.to_tag().is_some() => to.to_string(),
            Some(to) => format!("{to};tag={our_tag}"),
            None => format!("<sip:commos@{}>;tag={our_tag}", self.media_ip),
        };
        let to = msg
            .header("From")
            .map(str::to_string)
            .unwrap_or_else(|| format!("<sip:{src}>"));
        let request_uri = msg
            .header("Contact")
            .and_then(extract_uri)
            .or_else(|| msg.header("From").and_then(extract_uri))
            .unwrap_or_else(|| format!("sip:{src}"));
        CalleeLeg {
            addr: src,
            request_uri,
            from,
            to,
            call_id: msg.call_id().unwrap_or("").to_string(),
            cseq: 1,
        }
    }

    /// The display name a called phone should show as the calling party for this call — the
    /// operator's configurable text, read from `display_name_file`. One non-empty line → that text
    /// on every call; multiple lines → one selected per call (varied by `call_id`). `None` when the
    /// file is absent or empty, so callers fall back to the bare "commos" identity. Re-read per
    /// call so edits to the file apply without a restart (the file is tiny and local).
    pub(super) async fn call_display_name(&self, call_id: Uuid) -> Option<String> {
        let content = tokio::fs::read_to_string(&self.display_name_file)
            .await
            .ok()?;
        let lines: Vec<&str> = content
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        let line = match lines.len() {
            0 => return None,
            1 => lines[0],
            n => lines[display_line_index(call_id, n)],
        };
        let sanitized = sip_display_name(line);
        (!sanitized.is_empty()).then_some(sanitized)
    }

    /// Build a `180 Ringing` provisional for the caller's INVITE, establishing the early dialog
    /// with the same To-tag the eventual 200 OK will carry (so the phone treats them as one
    /// dialog and shows a ringing indication while CommOS rings the callee). No SDP.
    pub(super) fn build_ringing(&self, msg: &SipMessage, call_id: Uuid) -> String {
        let mut out = String::with_capacity(256);
        out.push_str("SIP/2.0 180 Ringing\r\n");
        for via in msg.header_all("Via") {
            out.push_str(&format!("Via: {via}\r\n"));
        }
        if let Some(from) = msg.header("From") {
            out.push_str(&format!("From: {from}\r\n"));
        }
        if let Some(to) = msg.header("To") {
            if msg.to_tag().is_some() {
                out.push_str(&format!("To: {to}\r\n"));
            } else {
                // Same tag derivation as `build_invite_ok`, so 180 and 200 share one dialog.
                let tag: String = call_id
                    .to_string()
                    .chars()
                    .filter(|c| *c != '-')
                    .take(16)
                    .collect();
                out.push_str(&format!("To: {to};tag={tag}\r\n"));
            }
        }
        if let Some(cid) = msg.call_id() {
            out.push_str(&format!("Call-ID: {cid}\r\n"));
        }
        if let Some(cseq) = msg.header("CSeq") {
            out.push_str(&format!("CSeq: {cseq}\r\n"));
        }
        out.push_str(&format!("Contact: <sip:commos@{}>\r\n", self.media_ip));
        out.push_str("Server: commosd\r\n");
        out.push_str("Content-Length: 0\r\n\r\n");
        out
    }

    /// Build a `200 OK` for an INVITE with an SDP body, echoing the dialog headers. (The
    /// bodyless [`message::response`] builder can't carry SDP, so INVITE answers are built
    /// here.)
    pub(super) fn build_invite_ok(&self, msg: &SipMessage, sdp: &str, call_id: Uuid) -> String {
        let mut out = String::with_capacity(512);
        out.push_str("SIP/2.0 200 OK\r\n");
        for via in msg.header_all("Via") {
            out.push_str(&format!("Via: {via}\r\n"));
        }
        if let Some(from) = msg.header("From") {
            out.push_str(&format!("From: {from}\r\n"));
        }
        if let Some(to) = msg.header("To") {
            if msg.to_tag().is_some() {
                out.push_str(&format!("To: {to}\r\n"));
            } else {
                // Our (callee) dialog tag, derived from the Call it created.
                let tag: String = call_id
                    .to_string()
                    .chars()
                    .filter(|c| *c != '-')
                    .take(16)
                    .collect();
                out.push_str(&format!("To: {to};tag={tag}\r\n"));
            }
        }
        if let Some(cid) = msg.call_id() {
            out.push_str(&format!("Call-ID: {cid}\r\n"));
        }
        if let Some(cseq) = msg.header("CSeq") {
            out.push_str(&format!("CSeq: {cseq}\r\n"));
        }
        out.push_str(&format!("Contact: <sip:commos@{}>\r\n", self.media_ip));
        out.push_str("Server: commosd\r\n");
        out.push_str("Content-Type: application/sdp\r\n");
        out.push_str(&format!("Content-Length: {}\r\n\r\n", sdp.len()));
        out.push_str(sdp);
        out
    }
}

/// Pick which display-name line to use for a call when the file has several, varied per call so
/// the messages rotate. Derived from the call id's random bits (UUIDv7), so it is stable for a
/// given call but differs between calls without needing an RNG.
fn display_line_index(call_id: Uuid, n: usize) -> usize {
    let sum: u32 = call_id.to_string().bytes().map(u32::from).sum();
    (sum as usize) % n.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_line_index_is_stable_per_call_and_in_range() {
        let id = Uuid::now_v7();
        // Deterministic for a given call, and always a valid index.
        assert_eq!(display_line_index(id, 3), display_line_index(id, 3));
        for n in 1..=5 {
            assert!(display_line_index(id, n) < n);
        }
    }
}
