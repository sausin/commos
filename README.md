# CommOS

**A specification-first blueprint for a modern communications operating system.**

CommOS is not (yet) a program. It is a **contract**. The legacy PBX world —
Asterisk, FreeSWITCH, and their descendants — encodes its behaviour in XML
dialplans, Lua scripts, and tribal knowledge. CommOS inverts that: the durable
asset is a precise, versioned, implementation-independent **specification suite**
plus a set of **machine-readable contracts** and an **executable conformance
harness**. Code is replaceable; the contract is the standard.

The guiding reframe: **voice is one workload.** A PBX is one application running
on a general communications substrate. The same substrate — Identity, Routing,
Presence, Media, Object Storage, Event Bus, Billing, Policy, AI Integration —
extends to messaging, video, intercom, contact centre, AI agents, and IoT
endpoints without a redesign.

## Why specification-first

For a system that is, in effect, a distributed real-time operating system,
implementation quality depends far more on **architecture and contracts** than on
feature lists. Freezing the contracts first lets independent teams — human or AI —
implement compatible components in parallel, and keeps alternative implementations
(a different media engine, a different provisioning engine) viable without breaking
compatibility. The intent is a vendor-neutral platform standard, in the spirit of
what OCI did for containers and Kubernetes did for orchestration.

## Repository layout

| Path | What it is |
|------|-----------|
| [`spec/`](spec/) | The specification suite — 20 volumes (0–19). The normative prose. |
| [`spec/CONVENTIONS.md`](spec/CONVENTIONS.md) | Normative language (RFC 2119), requirement IDs, contract versioning, conformance levels. Read this first. |
| [`spec/GLOSSARY.md`](spec/GLOSSARY.md) | Canonical terms. One word, one meaning. |
| [`contracts/`](contracts/) | Machine-readable contracts: JSON Schema for the domain model and events, OpenAPI for the API. **Normative.** |
| [`conformance/`](conformance/) | The executable conformance harness. Validates that the contracts are self-consistent and that any implementation conforms. |

## The specification suite

Read [`spec/README.md`](spec/README.md) for the full index and the **freeze-status
matrix**. In brief:

- **0 Philosophy** — the constitution: invariants and non-goals every other volume obeys.
- **1 PRD** — personas, epics, acceptance criteria.
- **2 Domain Model** — the keystone: every entity, its lifecycle and relationships.
- **3 Architecture** — subsystems and the control-plane / media-plane split.
- **4 API** — REST/WebSocket conventions and the endpoint catalogue (OpenAPI).
- **5 Events** — the canonical event model, envelope, and delivery guarantees.
- **6–18** — Database, Communications, Provisioning, Security, Billing, AI, Plugin
  SDK, UI/UX, Deployment, Observability, Testing, Performance, Engineering Standards.
- **19 ADRs** — why each significant decision was made, and what would reopen it.

## Specification-first development process

1. Freeze the **domain model** (Volume 2 + `contracts/json-schema/entities`).
2. Freeze the **event model** (Volume 5 + `contracts/json-schema/events`).
3. Freeze the **APIs** (Volume 4 + `contracts/openapi`).
4. Freeze the **UX flows** (Volume 13).
5. Build **executable conformance tests** from the specifications (`conformance/`).
6. **Implement** against those tests.

A contract is *frozen* only when it has a machine-readable form under `contracts/`
and the conformance harness passes against it. Prose without a contract is a draft.

## Running the conformance harness

```bash
python3 -m pip install jsonschema        # one dependency
python3 conformance/run.py               # validates contracts + spec consistency
```

The harness is the arbiter of "does this conform." See [`conformance/README.md`](conformance/README.md).

## Status

This is **v0.4** — **contract-complete**. The foundational spine (Philosophy, Domain,
Events, API, Architecture) is `FROZEN`; the remaining volumes are at `REVIEW`. Every
domain entity (36) and canonical event (74) has a JSON Schema and validated example,
the OpenAPI covers the full API surface (91 paths), the subsystem interfaces are
typed (8), and a second workload (messaging/video/presence/contact-centre) is modelled
alongside voice — proving the substrate is workload-general. The executable
conformance harness runs 500+ checks (green). See the freeze-status matrix in
[`spec/README.md`](spec/README.md).

## Licence

The specification is intended to be openly licensed as a vendor-neutral standard.
Licence selection is tracked in [ADR-0009](spec/019-adrs/README.md).
