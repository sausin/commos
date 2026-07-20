# CommOS Specification Suite

Version **0.3.0**. This suite is the normative definition of CommOS. Read
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
| 0 | [Philosophy & Design Principles](000-philosophy/) | REVIEW | — (constitution) | — |
| 1 | [Product Requirements](001-prd/) | DRAFT | — | 0 |
| 2 | [Domain Model](002-domain-model/) | REVIEW | `json-schema/entities/*` | 0 |
| 3 | [System Architecture](003-architecture/) | REVIEW | — | 0, 2, 5 |
| 4 | [API](004-api/) | REVIEW | `openapi/commos.openapi.yaml` | 2, 5 |
| 5 | [Events](005-events/) | REVIEW | `json-schema/envelope` + `events/*` | 2 |
| 6 | [Database](006-database/) | DRAFT | (planned) `sql/` | 2 |
| 7 | [Communications (SIP/RTP)](007-communications/) | DRAFT | (planned) | 2, 3 |
| 8 | [Provisioning](008-provisioning/) | DRAFT | (planned) `json-schema/provisioning` | 2, 9 |
| 9 | [Identity & Security](009-security/) | DRAFT | (planned) | 0, 2 |
| 10 | [Billing](010-billing/) | DRAFT | (planned) `json-schema/cdr` | 2, 5 |
| 11 | [AI Integration](011-ai/) | DRAFT | (planned) | 5, 4 |
| 12 | [Plugin SDK](012-plugin-sdk/) | DRAFT | (planned) | 3, 5 |
| 13 | [UI/UX](013-ui/) | DRAFT | — | 1, 4 |
| 14 | [Deployment](014-deployment/) | DRAFT | — | 3 |
| 15 | [Observability](015-observability/) | DRAFT | (planned) | 3, 5 |
| 16 | [Testing](016-testing/) | DRAFT | `conformance/` | all |
| 17 | [Performance Targets](017-performance/) | DRAFT | — | 3 |
| 18 | [Engineering Standards](018-engineering/) | DRAFT | — | — |
| 19 | [Architectural Decision Records](019-adrs/) | REVIEW | — | all |

> The **spine** (0, 2, 5, 4, 3) is at `REVIEW` with machine-readable contracts in
> place and the harness green. It is the stable base other volumes and
> implementations build on. Promotion of the spine to `FROZEN` is tracked in the
> per-volume change logs and gated on external review.

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

- **0.3.0** — Foundational spine deepened to implementation grade (Vols 0, 2, 3, 4,
  5). Added `contracts/` (JSON Schema for envelope, core entities, and events;
  OpenAPI skeleton) and the executable conformance harness. Added CONVENTIONS and
  GLOSSARY. Breadth volumes given real structure.
- **0.2.0** — Initial 20-volume skeleton with seed content (imported baseline).
