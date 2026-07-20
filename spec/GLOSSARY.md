# CommOS Glossary

Canonical terms. **One word, one meaning.** Every volume MUST use these terms as
defined here. Where legacy PBX vocabulary differs, the legacy term is listed as a
non-normative alias so newcomers can map their mental model — but specs MUST use
the canonical term.

| Term | Definition | Legacy alias |
|------|-----------|--------------|
| **Organisation** | A tenant. The top-level unit of isolation, ownership, and billing. | "customer", "domain" |
| **Cost Centre** | A billing/accounting grouping within an Organisation. | — |
| **Department** | An organisational grouping of Users, below Cost Centre. | "group" |
| **User** | A human (or service) principal. The subject of authentication and the bearer of Identities. | — |
| **Identity** | A verified authentication assertion binding a User to a session at a Device (PIN, FIDO2, NFC, …). Distinct from the User. | — |
| **Device** | A physical or virtual endpoint (desk phone, softphone, ATA, intercom, gateway). Owned by the Organisation, not the User. | "phone" |
| **Extension** | A short dialable address routed by the platform. A convenience label, **not** an identity. | "extension" |
| **DID** | A Direct Inward Dialing number (external, E.164) that routes into the platform. | "DID", "number" |
| **Route** | A declarative rule mapping a source/condition to a destination. Replaces the dialplan. | "dialplan context" |
| **Call Flow** | A versioned graph of routing nodes (time conditions, IVR, queue, …). The visual, declarative dialplan. | "dialplan" |
| **Queue** | An ordered set of waiting Calls served by Agents under a strategy. | "ACD queue" |
| **IVR** | An interactive menu node within a Call Flow. | "auto attendant" |
| **Conference** | A many-party media session mixed/forwarded by the Media Plane. | "conference bridge" |
| **Call** | A signalling+media session between two or more parties. One workload instance. | "channel", "call leg (part of)" |
| **Media Stream** | A single directional flow of RTP/SRTP media within a Call. | — |
| **Gateway** | A Device that bridges the platform to another network (PSTN, mobile/4G, SIP trunk). | "gateway" |
| **Carrier** | An external provider of telephony transport, reached via a Gateway or trunk. | "provider", "ITSP" |
| **Trunk** | A configured signalling relationship with a Carrier. | "SIP trunk" |
| **Control Plane** | The subsystems that decide *what* should happen: identity, policy, routing, provisioning, billing, API. Stateless where possible. | — |
| **Media Plane** | The subsystems that move real-time media: SIP, RTP/SRTP, transcoding, recording, conferencing, WebRTC. | — |
| **Event** | An immutable record that something happened, published to the Event Bus. Past tense. | "CDR fragment", "AMI/ARI event" |
| **Event Bus** | The transport that delivers Events to subscribers with defined ordering and delivery guarantees. | "AMI", "ARI", "ESL" |
| **Command** | A request to change state, expressed via the API. Present/imperative. | — |
| **Workload** | An application built on the substrate. Voice is one workload; messaging, video, contact centre are others. | — |
| **Substrate** | The core platform services shared by all Workloads. | — |
| **Object** | Any large binary artifact (recording, voicemail, firmware, transcript, export) stored via the Object Storage abstraction. | "file" |
| **Object Storage** | The pluggable interface over Local/S3/MinIO/R2/Azure/GCS. The platform never depends on a specific backend. | — |
| **Capability** | A fine-grained, grantable permission (e.g. `provision.devices`). Authorization is capability-based, not role-based. | "role/permission" |
| **Policy** | A declarative rule evaluated by the Policy Engine to allow/deny/route/require-identity for an action. | "class of service", "ACL" |
| **Provisioner** | A vendor plugin implementing the device provisioning contract (build config, reboot, firmware, factory reset). | "provisioning template" |
| **Digital Twin** | The versioned, observable representation of a real-world object (Device, User, Gateway, …) held by the platform. | — |
| **CDR** | Call Detail Record: the billable, attributable record derived from a Call's events. | "CDR" |
| **Correlation ID** | An identifier shared by all Events and API calls arising from one logical operation (e.g. one Call). | — |
| **Idempotency Key** | A client- or platform-supplied key ensuring an operation/event is applied at most once. | — |
| **Tenant** | Synonym for Organisation at the isolation boundary. | — |
| **Emergency Override** | A policy state in which identity/authorization requirements are bypassed for emergency calls. | — |
| **Conformance Profile** | A testable slice of the platform (`core`, `voice`, …). See CONVENTIONS §3. | — |
| **Freeze** | Promotion of a volume/contract to `FROZEN`: normative, versioned, conformance-covered. | — |

> Note (informative): the single most important vocabulary shift from legacy PBXs
> is **Identity ≠ Extension ≠ Device ≠ User**. Legacy systems collapse these into
> "an extension." CommOS keeps them distinct because billing, security, and mobility
> all depend on the difference — a shared reception Device can carry Alice's Identity
> for one call and Bob's for the next, and each call bills to the right User.
