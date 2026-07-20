//! Canonical events — Rust projections of `contracts/json-schema/events/*` (Volume 5).

pub mod call_answered;
pub mod call_busy;
pub mod call_ended;
pub mod call_failed;
pub mod call_flow_published;
pub mod call_held;
pub mod call_no_answer;
pub mod call_rejected;
pub mod call_resumed;
pub mod call_ringing;
pub mod call_started;
pub mod call_transferred;
pub mod channel_created;
pub mod message_sent;
pub mod participant_joined;
pub mod participant_left;
pub mod presence_changed;
pub mod thread_opened;
pub mod video_room_started;
