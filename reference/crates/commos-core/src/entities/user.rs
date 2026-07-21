//! `User` entity — Rust projection of
//! `contracts/json-schema/entities/User.schema.json`.
//!
//! A User is the identity workload's principal: a named actor within a tenant that
//! capabilities and workload artefacts attribute back to (Volume 2 §Identity;
//! CMOS-02-DOM-113 attribution chain). It is a *peer* domain entity on the same
//! substrate (CMOS-02-DOM-100). `state` drives the identity lifecycle machine
//! (`INVITED → ACTIVE → SUSPENDED → DEACTIVATED`).

use serde::{Deserialize, Serialize};

use crate::common::{EntityBase, Uuid};

/// User lifecycle state (`User.schema.json` `state`; Volume 2 §Identity lifecycle).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum UserState {
    Invited,
    Active,
    Suspended,
    Deactivated,
}

/// The User entity. `EntityBase` is flattened so the wire shape is
/// `allOf: [EntityBase] + User properties`, matching the schema. Only `display_name`
/// and `state` are required; the rest are optional.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct User {
    #[serde(flatten)]
    pub base: EntityBase,
    pub display_name: String,
    pub state: UserState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub department_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_centre_id: Option<Uuid>,
    /// Capability grants attributed to this principal (CMOS-02-DOM-113).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
}

impl User {
    /// Create a new User in the `ACTIVE` state with no optional fields set. Callers set
    /// `email` / `department_id` / `cost_centre_id` / `capabilities` on the returned value.
    pub fn new(tenant: Uuid, display_name: impl Into<String>) -> Self {
        User {
            base: EntityBase::new(tenant),
            display_name: display_name.into(),
            state: UserState::Active,
            email: None,
            department_id: None,
            cost_centre_id: None,
            capabilities: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_starts_active_v0() {
        let t = Uuid::now_v7();
        let u = User::new(t, "Ada Lovelace");
        assert_eq!(u.state, UserState::Active);
        assert_eq!(u.base.version, 0);
        assert_eq!(u.base.tenant_id, t);
        assert_eq!(u.display_name, "Ada Lovelace");
    }

    #[test]
    fn serialised_field_names_match_schema() {
        let mut u = User::new(Uuid::now_v7(), "Ada Lovelace");
        u.email = Some("ada@example.com".into());
        u.department_id = Some(Uuid::now_v7());
        u.cost_centre_id = Some(Uuid::now_v7());
        u.capabilities = vec!["voice.call".into()];
        let json = serde_json::to_value(&u).unwrap();
        // Flattened EntityBase + User properties, faithful casing.
        assert_eq!(json["display_name"], "Ada Lovelace");
        assert_eq!(json["state"], "ACTIVE");
        assert_eq!(json["email"], "ada@example.com");
        assert_eq!(json["capabilities"][0], "voice.call");
        assert!(json.get("id").is_some());
        assert!(json.get("tenant_id").is_some());
        assert!(json.get("department_id").is_some());
        assert!(json.get("cost_centre_id").is_some());
        // Round-trips.
        let back: User = serde_json::from_value(json).unwrap();
        assert_eq!(back.state, UserState::Active);
        assert_eq!(back.display_name, "Ada Lovelace");

        // Every state variant renders SCREAMING_SNAKE.
        let render = |s| {
            let mut x = User::new(Uuid::now_v7(), "x");
            x.state = s;
            serde_json::to_value(&x).unwrap()["state"].clone()
        };
        assert_eq!(render(UserState::Invited), "INVITED");
        assert_eq!(render(UserState::Suspended), "SUSPENDED");
        assert_eq!(render(UserState::Deactivated), "DEACTIVATED");
    }

    #[test]
    fn empty_optionals_are_omitted() {
        let u = User::new(Uuid::now_v7(), "x");
        let json = serde_json::to_value(&u).unwrap();
        assert!(json.get("email").is_none());
        assert!(json.get("department_id").is_none());
        assert!(json.get("cost_centre_id").is_none());
        assert!(json.get("capabilities").is_none());
    }
}
