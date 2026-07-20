//! The typed control → media boundary (CMOS-03-ARCH-001/002/003).
//!
//! The control plane decides *what* happens; the media plane moves real-time media. They
//! communicate **only over a typed interface, never shared mutable memory — even when
//! compiled into one binary** (CMOS-00-ENG-006). Signalling flows control→media as
//! commands; media facts flow back media→control as events (modelled here as a return
//! ack; a full fact stream lands with the RTP subsystem).
//!
//! Because this is a real interface, the media plane can later be split into its own
//! process/node with no control-plane change (CMOS-14-DEP-011, split-media topology).
//! [`LoopbackMedia`] is the in-process binding used by the single binary.

use commos_core::common::Uuid;

/// A command issued by the control plane to the media plane. Mirrors the frozen
/// `ControlMediaCommand` interface (`contracts/json-schema/interfaces/`). The full command
/// set is modelled here; `Hold`/`Resume`/`Hangup` are wired as their `/v1/calls/{id}:*`
/// actions land.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub enum MediaCommand {
    /// Begin signalling for a Call (SIP INVITE / WebRTC offer).
    Originate {
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

/// Acknowledgement returned across the boundary. In the split-media topology this becomes
/// an async fact on the event stream; the control-plane logic is identical either way.
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

    /// Whether this binding answers an originated Call immediately. The loopback binding
    /// does (it has no real peer), so Routing can drive the ring→answer progression and the
    /// full lifecycle is observable without a real SIP peer. A real media plane returns
    /// `false`: those facts arrive asynchronously from the network.
    fn auto_answers(&self) -> bool {
        false
    }
}

/// In-process media binding. It acknowledges commands so the control-plane vertical slice
/// is exercised end-to-end; the real SIP/RTP engine implements the same trait.
pub struct LoopbackMedia;

impl MediaPlane for LoopbackMedia {
    fn auto_answers(&self) -> bool {
        true
    }

    fn dispatch(&self, cmd: MediaCommand) -> MediaAck {
        match cmd {
            MediaCommand::Originate { call_id, from_ref, to_ref } => {
                tracing::info!(%call_id, from = %from_ref, to = %to_ref, "media: originate");
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
