//! The typed control ↔ media boundary (CMOS-03-ARCH-001/002/003).
//!
//! The control plane decides *what* happens; the media plane moves real-time media. They
//! communicate **only over a typed interface, never shared mutable memory — even when
//! compiled into one binary** (CMOS-00-ENG-006). Signalling flows control→media as
//! [`MediaCommand`]s; media state facts flow media→control as [`MediaFact`]s on an async
//! channel (CMOS-03-ARCH-003). This is the honest shape: answer/ring/quality are network
//! events the media plane observes and reports, not values the control plane computes.
//!
//! Because this is a real interface, the media plane can later be split into its own
//! process/node with no control-plane change (CMOS-14-DEP-011, split-media topology).
//! [`LoopbackMedia`] is the in-process binding used by the single binary.

use tokio::sync::mpsc::UnboundedSender;

use commos_core::common::{Timestamp, Uuid};

/// A command issued by the control plane to the media plane. Mirrors the frozen
/// `ControlMediaCommand` interface (`contracts/json-schema/interfaces/`). `tenant_id` is an
/// opaque context token the control plane provides and the media plane echoes back on
/// facts — the media plane never interprets it (it belongs to the control plane).
#[derive(Clone, Debug)]
pub enum MediaCommand {
    /// Begin signalling for a Call (SIP INVITE / WebRTC offer).
    Originate {
        tenant_id: Uuid,
        call_id: Uuid,
        from_ref: String,
        to_ref: String,
    },
    /// Put the Call's media on hold.
    Hold { call_id: Uuid },
    /// Resume held media.
    Resume { call_id: Uuid },
    /// Redirect the Call's media leg to a new target (REFER / re-INVITE).
    Transfer { call_id: Uuid, to_ref: String },
    /// Tear the Call down (BYE / hangup).
    Hangup { call_id: Uuid },
}

/// A fact reported by the media plane back to the control plane (media→control,
/// CMOS-03-ARCH-003). The control plane applies these to Call state and emits the
/// corresponding events. `tenant_id` is the context token echoed from the command.
#[derive(Clone, Debug)]
pub enum MediaFact {
    /// The remote end is ringing.
    Rang { tenant_id: Uuid, call_id: Uuid },
    /// The Call was answered.
    Answered {
        tenant_id: Uuid,
        call_id: Uuid,
        answered_at: Timestamp,
    },
}

/// The channel the media plane uses to report facts to the control plane.
pub type FactSender = UnboundedSender<MediaFact>;

/// Acknowledgement returned synchronously across the command boundary (command accepted /
/// rejected). Ongoing state changes arrive later as [`MediaFact`]s.
/// `Rejected` is produced once negotiation/policy failure paths are wired.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaAck {
    Accepted { call_id: Uuid },
    Rejected { call_id: Uuid, reason: String },
}

/// The media plane as seen by the control plane. One trait; the binding (in-process,
/// gRPC to a media node, …) is swappable without touching Routing.
pub trait MediaPlane: Send + Sync {
    fn dispatch(&self, cmd: MediaCommand) -> MediaAck;
}

/// In-process media binding. It acknowledges commands and — having no real network peer —
/// simulates the peer ringing then answering by emitting the corresponding facts. A real
/// SIP/RTP engine implements the same trait and reports facts from actual signalling.
pub struct LoopbackMedia {
    facts: FactSender,
}

impl LoopbackMedia {
    pub fn new(facts: FactSender) -> Self {
        LoopbackMedia { facts }
    }
}

impl MediaPlane for LoopbackMedia {
    fn dispatch(&self, cmd: MediaCommand) -> MediaAck {
        match cmd {
            MediaCommand::Originate { tenant_id, call_id, from_ref, to_ref } => {
                tracing::info!(%call_id, from = %from_ref, to = %to_ref, "media: originate");
                // Simulate the peer: ring, then answer. Facts are applied asynchronously by
                // the control plane's fact loop, exactly as a real media node would report.
                let _ = self.facts.send(MediaFact::Rang { tenant_id, call_id });
                let _ = self.facts.send(MediaFact::Answered {
                    tenant_id,
                    call_id,
                    answered_at: Timestamp::now(),
                });
                MediaAck::Accepted { call_id }
            }
            MediaCommand::Hold { call_id } => MediaAck::Accepted { call_id },
            MediaCommand::Resume { call_id } => MediaAck::Accepted { call_id },
            MediaCommand::Transfer { call_id, to_ref } => {
                tracing::info!(%call_id, to = %to_ref, "media: transfer");
                MediaAck::Accepted { call_id }
            }
            MediaCommand::Hangup { call_id } => MediaAck::Accepted { call_id },
        }
    }
}
