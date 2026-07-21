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
//! **What comes next:** the B2BUA bridge is best-effort (see the `TODO(B2BUA)` in [`server`]);
//! full mid-dialog correctness, PSTN trunking, and SRTP are the remaining media-plane work.

pub mod digest;
pub mod message;
pub mod rtp;
pub mod server;

pub use server::SipServer;
