//! SIP signalling plane (Volume 7) — the real UDP front door for softphones.
//!
//! This is the signalling ingress that lets an actual SIP endpoint (Linphone, a desk phone,
//! a PBX trunk) talk to CommOS. [`SipServer`] terminates SIP over UDP; [`message`] is the
//! pure, unit-tested codec it uses.
//!
//! **What works today:** REGISTER is fully handled — optionally gated by SIP digest auth
//! ([`digest`]) — and drives the [`crate::control::registrations::RegistrationRegistry`], so
//! real endpoints register and become visible through the control plane and API. INVITE
//! creates an inbound Call, reports ring/answer, sets up an RTP path (echo, or a two-leg
//! bridge to a registered callee), and answers `200 OK` with SDP. OPTIONS/BYE/CANCEL are
//! answered; BYE produces the CDR.
//!
//! Media is encrypted with **SRTP** ([`srtp`], RFC 3711 `AES_CM_128_HMAC_SHA1_80`) when a caller
//! offers the secure `RTP/SAVP` profile with an SDES key ([`sdes`], RFC 4568): on the endpoint
//! paths CommOS terminates (echo/voicemail), and across the two-leg **bridge/trunk relay**, where
//! SRTP is terminated independently per leg — CommOS decrypts the caller leg and re-encrypts for
//! the callee/carrier leg, extending encryption end to end without the legs sharing keys. Plain-RTP
//! callers are unaffected. The signalling channel can itself be encrypted with **SIP-over-TLS**
//! ([`tls`], a `--features tls` build) — a stream transport, so [`transport`] re-frames messages by
//! `Content-Length` and answers back on the same connection through a [`transport::Responder`].
//!
//! **What comes next:** the B2BUA bridge is best-effort (see the `TODO(B2BUA)` in [`server`]);
//! full mid-dialog correctness and *outbound* TLS on UAC/trunk legs are the remaining media work.

pub mod codec;
pub mod digest;
pub mod dtmf;
pub mod g711;
pub mod ivr;
pub mod message;
pub mod moh;
pub mod reboot;
pub mod rtp;
pub mod sdes;
pub mod server;
pub mod srtp;
#[cfg(feature = "tls")]
pub mod tls;
pub mod transport;

pub use server::SipServer;
