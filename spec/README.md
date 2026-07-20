# CommOS Specification Suite

Version **0.4.0**. This suite is the normative definition of CommOS. Read
[`CONVENTIONS.md`](CONVENTIONS.md) and [`GLOSSARY.md`](GLOSSARY.md) before any
volume — they bind every volume that follows.

Treat this suite the way you would treat the specifications for Kubernetes,
PostgreSQL, or the Linux kernel: implementation-independent, versioned, and
precise. Feature descriptions matter far less than contracts.

## Reading order

For **implementers**, follow the freeze order (contracts you can build against
first): **0 → 2 → 5 → 4 → 3**, then the profile you are building
(`voice` → 7, `provisioning` → 8, `billing` → 10, `ai` → 11, `plugins` → 12),
then cross-cutting (6, 9, 14, 15, 18).

For **product/UX**, read **0 → 1 → 13**.

For **operators/evaluators**, read **0 → 3 → 14 → 15 → 17**.

## Freeze-status matrix

Status values are defined in [`CONVENTIONS.md`](CONVENTIONS.md#5-the-freeze-lifecycle).
A volume is `FROZEN` only when every normative statement has an ID, its data shapes
exist under [`contracts/`](../contracts/), and the conformance harness passes for it.

| Vol | Title | Status | Contract artifact | Depends on |
|-----|-------|--------|-------------------|-----------|
| 0 | [Philosophy & Design Principles](000-philosophy/) | FROZEN | — (constitution) | — |
| 1 | [Product Requirements](001-prd/) | REVIEW | — | 0 |
| 2 | [Domain Model](002-domain-model/) | FROZEN | `json-schema/entities/*` (36) | 0 |
| 3 | [System Architecture](003-architecture/) | FROZEN | `json-schema/interfaces/*` | 0, 2, 5 |
| 4 | [API](004-api/) | FROZEN | `openapi/commos.openapi.yaml` (91 paths) | 2, 5 |
| 5 | [Events](005-events/) | FROZEN | `envelope` + `events/*` (74) | 2 |
| 6 | [Database](006-database/) | REVIEW | schema-overview.md | 2 |
| 7 | [Communications (SIP/RTP)](007-communications/) | REVIEW | `interfaces/ControlMedia*` | 2, 3 |
| 8 | [Provisioning](008-provisioning/) | REVIEW | `interfaces/Provisioner*` | 2, 9 |
| 9 | [Identity & Security](009-security/) | REVIEW | `interfaces/PolicyDecision*` | 0, 2 |
| 10 | [Billing](010-billing/) | REVIEW | `entities/CDR` + `interfaces/Rating*` | 2, 5 |
| 11 | [AI Integration](011-ai/) | REVIEW | `entities/AIJob` + events | 5, 4 |
| 12 | [Plugin SDK](012-plugin-sdk/) | REVIEW | `entities/Plugin` + `interfaces/Provisioner*` | 3, 5 |
| 13 | [UI/UX](013-ui/) | REVIEW | — (views over the API) | 1, 4 |
| 14 | [Deployment](014-deployment/) | REVIEW | — | 3 |
| 15 | [Observability](015-observability/) | REVIEW | `events/MediaQualityReported` | 3, 5 |
| 16 | [Testing](016-testing/) | REVIEW | `conformance/` (harness + scenarios) | all |
| 17 | [Performance Targets](017-performance/) | REVIEW | — | 3 |
| 18 | [Engineering Standards](018-engineering/) | REVIEW | — | — |
| 19 | [Architectural Decision Records](019-adrs/) | REVIEW | — | all |

> The **spine** (0, 2, 3, 4, 5) is `FROZEN`: every normative statement has an ID, its
> shapes exist under [`contracts/`](../contracts/), and the conformance harness is
> green (500+ checks). The remaining volumes are at `REVIEW` — implementation-grade
> and contract-backed, pending external review to freeze. The contract set (36
> entities, 74 events, 8 interfaces, 91 API paths) is **complete** for the voice
> workload plus a second (messaging/video/presence/contact-centre) workload.

## The freeze order (why this order)

1. **Domain model first.** Every event payload and API body is a projection of an
   entity. Freezing entity identity, ownership, and lifecycle first prevents churn
   everywhere downstream.
2. **Events second.** Events reference entities. The event envelope and delivery
   guarantees are the integration contract for AI, billing, CRM, and automation —
   the platform's highest-leverage surface.
3. **APIs third.** Commands mutate entities and cause events; the API cannot be
   stable until both are.
4. **UX flows fourth.** Screens are views over the API.
5. **Conformance tests fifth**, derived from 1–4.
6. **Implementation last**, against the tests.

## Change log

- **0.4.0** — **Contract-complete.** Every domain entity (36) and every catalogued
  event (74) now has a JSON Schema + validated example; added typed subsystem
  interfaces (8: control↔media, Provisioner, Policy, Rating); the OpenAPI now covers
  the full endpoint catalogue (91 paths). Added the second-workload entities/events
  (messaging/video/presence/contact-centre) with domain prose
  ([`002-domain-model/workloads.md`](002-domain-model/workloads.md)), proving the
  substrate is workload-general. Extended the harness (interface example validation,
  OpenAPI $ref resolution, and L2 behavioural scenario definitions) to 500+ checks.
  Promoted the spine (0, 2, 3, 4, 5) to `FROZEN` and the breadth volumes to `REVIEW`.
- **0.3.0** — Foundational spine deepened to implementation grade (Vols 0, 2, 3, 4,
  5). Added `contracts/` (JSON Schema for envelope, core entities, and events;
  OpenAPI skeleton) and the executable conformance harness. Added CONVENTIONS and
  GLOSSARY. **All breadth volumes (1, 6–18) authored to implementation-grade drafts**
  and ADRs (19) recorded. Consistency pass: the API catalogue (Vol 4) adopted the
  capability keys introduced by Security (Vol 9) — `secrets.manage`, `certs.manage`,
  `calls.dial.international` — and the event catalogue (Vol 5) names the additional
  `Plugin*`, `Media`, and PKI/secret lifecycle events as `planned`.
- **0.2.0** — Initial 20-volume skeleton with seed content (imported baseline).
