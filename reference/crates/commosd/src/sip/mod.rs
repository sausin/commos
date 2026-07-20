//! SIP signalling plane (Volume 7) — the real UDP front door for softphones.
//!
//! This is the signalling ingress that lets an actual SIP endpoint (Linphone, a desk phone,
//! a PBX trunk) talk to CommOS. [`SipServer`] terminates SIP over UDP; [`message`] is the
//! pure, unit-tested codec it uses.
//!
//! **What works today:** REGISTER is fully handled — a phone's REGISTER drives the
//! [`crate::control::registrations::RegistrationRegistry`], so real endpoints register and
//! become visible through the control plane and API. OPTIONS/BYE/CANCEL are answered;
//! INVITE is acknowledged at the signalling layer only.
//!
//! **What comes next:** INVITE→Call creation and RTP media negotiation sit behind the
//! existing `MediaPlane` boundary and are the next step — the ingress deliberately does not
//! reach across that boundary yet (see the TODO in [`server`]).

pub mod message;
pub mod server;

pub use server::SipServer;
