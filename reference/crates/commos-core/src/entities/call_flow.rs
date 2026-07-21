//! `CallFlow` entity — Rust projection of
//! `contracts/json-schema/entities/CallFlow.schema.json`.
//!
//! A CallFlow is a versioned, publishable routing program: a `graph` of nodes+edges (an IVR
//! menu, a schedule branch, a route to a queue/voicemail, …) plus a lifecycle that makes
//! publishing **immutable and reversible** (Volume 2 §CallFlow; CMOS-00-ENG-012 append-only;
//! CMOS-01-PRD-031). The state machine is
//!
//! ```text
//! DRAFT ──publish──▶ PUBLISHED ──publish newer──▶ SUPERSEDED
//!   ▲                    │
//!   └──── rollback ───────┘   (rollback republishes a prior version as a new PUBLISHED)
//! ```
//!
//! `graph` is the **draft** working copy an editor mutates; each publish snapshots it into an
//! immutable [`CallFlowRevision`] numbered by `published_version`. Rollback never mutates
//! history — it republishes a prior revision's graph as a *new* revision (append-only). The
//! revision log lives beside the entity (the store), so this row's wire shape matches the
//! frozen schema exactly.

use serde::{Deserialize, Serialize};

use crate::common::{EntityBase, Timestamp, Uuid};

/// Publication lifecycle of a CallFlow (`CallFlow.schema.json` `state`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CallFlowState {
    /// Never published, or edited since the last publish (unpublished changes exist).
    Draft,
    /// The active published revision is current.
    Published,
    /// A newer revision has been published over an earlier one (history now carries
    /// superseded revisions); a rollback returns the flow to `PUBLISHED`.
    Superseded,
}

/// The CallFlow entity. `EntityBase` is flattened so the wire shape is
/// `allOf: [EntityBase] + CallFlow properties`. `name` and `state` are required; `graph`
/// defaults to an empty object and `published_version` is absent until the first publish.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallFlow {
    #[serde(flatten)]
    pub base: EntityBase,
    pub name: String,
    /// The draft graph (nodes+edges) an editor works on. Published revisions are snapshots
    /// of this value, kept immutably in the revision log.
    #[serde(default)]
    pub graph: serde_json::Value,
    /// The number of the currently-active published revision; absent until first publish.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_version: Option<u64>,
    pub state: CallFlowState,
}

impl CallFlow {
    /// Create a new DRAFT CallFlow named `name` with an empty graph and no published version.
    pub fn new(tenant: Uuid, name: impl Into<String>) -> Self {
        CallFlow {
            base: EntityBase::new(tenant),
            name: name.into(),
            graph: serde_json::json!({}),
            published_version: None,
            state: CallFlowState::Draft,
        }
    }

    /// The revision number a fresh publish/rollback will produce (append-only, monotonic).
    pub fn next_version(&self) -> u64 {
        self.published_version.map_or(1, |v| v + 1)
    }

    /// Replace the draft `graph` (an editor edit). Returns the flow to `DRAFT` — there are now
    /// unpublished changes — and versions the entity forward.
    pub fn set_graph(&mut self, graph: serde_json::Value) {
        self.graph = graph;
        self.state = CallFlowState::Draft;
        self.base.touch();
    }

    /// Publish the current draft graph: bump `published_version` and advance `state`. The
    /// first publish is `DRAFT → PUBLISHED`; publishing a newer version over an
    /// already-published flow is `→ SUPERSEDED` (the flow now has superseded generations).
    /// Keyed on whether a prior published version exists, so an intervening draft edit does
    /// not change the outcome. Returns the new revision number; the caller snapshots
    /// `self.graph` into an immutable [`CallFlowRevision`] at this number.
    pub fn mark_published(&mut self) -> u64 {
        let first_publish = self.published_version.is_none();
        let version = self.next_version();
        self.state = if first_publish {
            CallFlowState::Published
        } else {
            CallFlowState::Superseded
        };
        self.published_version = Some(version);
        self.base.touch();
        version
    }

    /// Roll back to a prior revision's `graph`: adopt it as the draft, republish it as a *new*
    /// revision (append-only — the old revision is untouched), and return to `PUBLISHED`.
    /// Returns the new revision number.
    pub fn mark_rolled_back(&mut self, graph: serde_json::Value) -> u64 {
        let version = self.next_version();
        self.graph = graph;
        self.state = CallFlowState::Published;
        self.published_version = Some(version);
        self.base.touch();
        version
    }
}

/// An immutable snapshot of a CallFlow's `graph` at a published version (Volume 2 Time
/// Machine; CMOS-00-ENG-012 append-only history). Keyed by `(tenant, call_flow_id, version)`;
/// once written it is never mutated — republish/rollback only append new revisions.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallFlowRevision {
    pub tenant_id: Uuid,
    pub call_flow_id: Uuid,
    pub version: u64,
    pub graph: serde_json::Value,
    pub created_at: Timestamp,
}

impl CallFlowRevision {
    /// Capture `graph` as revision `version` of `call_flow_id`, stamped now.
    pub fn new(tenant_id: Uuid, call_flow_id: Uuid, version: u64, graph: serde_json::Value) -> Self {
        CallFlowRevision { tenant_id, call_flow_id, version, graph, created_at: Timestamp::now() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_starts_draft_v0_with_empty_graph() {
        let cf = CallFlow::new(Uuid::now_v7(), "Main IVR");
        assert_eq!(cf.state, CallFlowState::Draft);
        assert_eq!(cf.published_version, None);
        assert_eq!(cf.base.version, 0);
        let j = serde_json::to_value(&cf).unwrap();
        assert_eq!(j["name"], "Main IVR");
        assert_eq!(j["state"], "DRAFT");
        assert!(j.get("published_version").is_none());
    }

    #[test]
    fn publish_then_publish_newer_then_rollback_walks_the_state_machine() {
        let mut cf = CallFlow::new(Uuid::now_v7(), "Flow");
        cf.set_graph(serde_json::json!({"nodes": [{"id": "v1"}]}));

        // DRAFT → PUBLISHED (revision 1).
        assert_eq!(cf.mark_published(), 1);
        assert_eq!(cf.state, CallFlowState::Published);
        assert_eq!(cf.published_version, Some(1));

        // PUBLISHED → SUPERSEDED (revision 2).
        cf.set_graph(serde_json::json!({"nodes": [{"id": "v2"}]}));
        assert_eq!(cf.state, CallFlowState::Draft, "editing returns to DRAFT");
        assert_eq!(cf.mark_published(), 2);
        assert_eq!(cf.state, CallFlowState::Superseded);
        assert_eq!(cf.published_version, Some(2));

        // rollback to revision 1's graph → PUBLISHED (revision 3, republished content).
        let v1_graph = serde_json::json!({"nodes": [{"id": "v1"}]});
        assert_eq!(cf.mark_rolled_back(v1_graph.clone()), 3);
        assert_eq!(cf.state, CallFlowState::Published);
        assert_eq!(cf.published_version, Some(3));
        assert_eq!(cf.graph, v1_graph, "rollback adopts the prior version's graph");
    }

    #[test]
    fn revision_captures_graph_and_key() {
        let tenant = Uuid::now_v7();
        let flow = Uuid::now_v7();
        let g = serde_json::json!({"nodes": []});
        let rev = CallFlowRevision::new(tenant, flow, 2, g.clone());
        assert_eq!(rev.tenant_id, tenant);
        assert_eq!(rev.call_flow_id, flow);
        assert_eq!(rev.version, 2);
        assert_eq!(rev.graph, g);
        // Round-trips.
        let back: CallFlowRevision = serde_json::from_value(serde_json::to_value(&rev).unwrap()).unwrap();
        assert_eq!(back.version, 2);
    }
}
