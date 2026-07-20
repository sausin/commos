# Volume 11 — AI Integration Surface

**Status:** DRAFT · **Version:** 0.3.0 · **Subsystem tag:** AI

CommOS **does not implement AI**. It is designed to be the world's best AI
*integration surface*: an external AI system — Claude, OpenAI, Gemini, a self-hosted
vLLM or Ollama model, a custom model, or an orchestrator such as n8n or LangGraph —
plugs in through the substrate's existing primitives (**Events**, **REST API**,
**streaming**, **webhooks**, and **Object Storage**) and returns results through a
uniform **AIJob** lifecycle. No AI-vendor assumption ever leaks into the substrate.
(Serves CMOS-00-ENG-013, non-goal N-2.)

This volume freezes the **integration contract**: how a consumer subscribes to
events, receives **Object references** (never raw media), submits and tracks work as
an AIJob, and returns results as result Objects — with tenant scoping and PII
minimisation throughout.

Entities: [`AIJob`](../002-domain-model/entities.md#webhook--automation--aijob--plugin--auditentry),
[`Object`](../002-domain-model/entities.md#object). Events: `AIJobQueued`,
`AIJobStarted`, `AIJobCompleted`, `AIJobFailed`
([catalog](../005-events/catalog.md#billing-webhook-automation-ai-plugin-audit)).

> Note (informative): the design test for every requirement here is *"could a model
> we have never heard of, hosted somewhere we do not control, consume this without a
> code change to the substrate?"* If not, the requirement is wrong.

---

## 1. Principles (normative)

- **CMOS-11-AI-001** The platform MUST NOT ship, embed, or hard-depend on any LLM,
  ASR, TTS, or embedding model. AI is an **external consumer** reached only through
  the public integration surface. (Serves CMOS-00-ENG-013, N-2.)
- **CMOS-11-AI-002** No AI-vendor name, model identifier, prompt, or SDK MAY appear in
  a substrate contract (schema, endpoint, event). Vendor selection lives entirely in
  the consumer and in tenant configuration. Where a vendor label is stored it MUST be
  an open-set `_key` field (CMOS-CONV-017), e.g. `consumer_key`.
- **CMOS-11-AI-003** Every AI interaction MUST flow through one of the five substrate
  primitives — **Events** (Volume 5), **REST** (Volume 4), **streaming**, **webhooks**
  (CMOS-05-EVT-014), **Object Storage** (CMOS-00-ENG-007) — and MUST NOT require a
  private side channel. (Serves CMOS-00-ENG-003, CMOS-00-ENG-004.)
- **CMOS-11-AI-004** AI capabilities are **workloads on the substrate**, not privileged
  subsystems (CMOS-00-ENG-001); an AI job is subject to the same tenancy, capability,
  audit, and (where metered) billing rules as any other workload.
- **CMOS-11-AI-005** Payloads MUST carry **Object references**, never inline media or
  full transcripts (CMOS-02-DOM-013, CMOS-05-EVT-041). A consumer resolves an Object
  reference under authorization to fetch bytes.

## 2. The integration surface (informative overview)

```
        ┌──────────────────────── CommOS substrate ────────────────────────┐
        │  Events ──▶ Event Bus / Webhooks ──▶ (subscription, capability-gated)
Call ───┼─▶ CallEnded / RecordingUploaded ─────────────┐                     │
        │  Object Storage (recording / transcript) ──── │ ─── object refs ──▶ │──▶ External AI
        │  REST API: POST /ai/jobs, GET /ai/jobs/{id}   │                     │   (Claude / OpenAI /
        │  Streaming: media / event tail (read-only)    │                     │    Gemini / vLLM /
        │                                               ▼                     │    Ollama / n8n / …)
        │  AIJob lifecycle: QUEUED ▶ RUNNING ▶ COMPLETED│FAILED ◀── result ───┼──◀ result Object +
        └───────────────────────────────────────────────────────────────────┘     AIJob callback
```

Four integration patterns, all first-class:

1. **Event-driven** — consumer subscribes (webhook or bus cursor) to trigger events
   and creates AIJobs in response (e.g. transcribe on `RecordingUploaded`).
2. **On-demand** — a client/automation submits an AIJob directly via REST.
3. **Streaming** — consumer tails a read-only event or media stream for real-time
   use (e.g. live coaching), still reporting outcomes as an AIJob.
4. **Orchestrated** — an external orchestrator (n8n/LangGraph) fans multiple AIJobs
   and writes results back through the same lifecycle.

## 3. Subscription & triggering (normative)

- **CMOS-11-AI-010** A consumer MUST subscribe to Events via a capability-gated
  mechanism — a durable bus cursor or a signed **Webhook** (Volume 2 `Webhook`) — and
  MUST receive only events for `tenant_id`s it is authorised for (CMOS-05-EVT-040).
- **CMOS-11-AI-011** Webhook deliveries to AI consumers MUST be HMAC-signed over the
  raw body and MUST emit `WebhookDelivered` / `WebhookDeliveryFailed`
  (CMOS-05-EVT-014); undeliverable events go to the dead-letter stream, never dropped
  (CMOS-05-EVT-013).
- **CMOS-11-AI-012** Trigger events carry Object **references and metadata**, not
  content. On `RecordingUploaded` the consumer receives the recording `Object` id/URI
  and MUST resolve it under authorization to fetch media (CMOS-11-AI-005).
- **CMOS-11-AI-013** A consumer MUST treat events as **facts, not commands**
  (CMOS-05-EVT-003): receiving `CallEnded` does not entitle a consumer to alter the
  Call; it may only create AIJobs and write result Objects.
- **CMOS-11-AI-014** Consumers MUST be idempotent on (`type`, `idempotency_key`)
  (CMOS-05-EVT-011); at-least-once re-delivery MUST NOT create duplicate AIJobs for the
  same logical input (dedupe on `input_refs` + `kind`).

## 4. The AIJob lifecycle (normative)

The `AIJob` entity models one unit of externally-performed AI work. Its status
machine is the contract between substrate and consumer.

```
QUEUED ──▶ RUNNING ──▶ COMPLETED
   │           │
   └───────────┴────▶ FAILED   (with error, retryable flag)
                └────▶ CANCELLED (operator/consumer)
```

| Status | Meaning | Event |
|--------|---------|-------|
| `QUEUED` | Job accepted, input refs resolved, awaiting a consumer. | `AIJobQueued` |
| `RUNNING` | A consumer claimed the job and is processing. | `AIJobStarted` |
| `COMPLETED` | Result Object written and referenced on the job. | `AIJobCompleted` |
| `FAILED` | Terminal failure; `error` + `retryable` recorded. | `AIJobFailed` |
| `CANCELLED` | Withdrawn before completion. | `AIJobFailed` (reason=cancelled) |

- **CMOS-11-AI-020** An AIJob MUST be created via `POST /ai/jobs` (or by an Automation)
  with `kind`, `input_refs[]` (Event and/or Object references), and optional
  `consumer_key` and parameters. The platform MUST assign a UUIDv7 `id`, set status
  `QUEUED`, and emit `AIJobQueued`.
- **CMOS-11-AI-021** Status transitions MUST follow the machine above; an illegal
  transition MUST be rejected (CMOS-02-DOM-007) and each transition MUST emit its named
  event, persisted before acknowledgement (CMOS-05-EVT-010).
- **CMOS-11-AI-022** `input_refs[]` MUST contain only references (Event ids, Object
  ids/URIs) — never inline payloads (CMOS-11-AI-005). The platform MUST validate that
  each referenced Object/Event is within the job's `tenant_id`.
- **CMOS-11-AI-023** On completion the consumer MUST write results as one or more
  **Objects** (`kind = TRANSCRIPT\|EXPORT\|…`) and set `result_object_id` (and/or a
  structured `result` summary within size limits) before the job transitions to
  `COMPLETED`. `AIJobCompleted.data` carries the **result Object reference**, validated
  against
  [`AIJobCompleted.schema.json`](../../contracts/json-schema/events/AIJobCompleted.schema.json).
- **CMOS-11-AI-024** `AIJobFailed` MUST carry a structured `error` (code + message) and
  a `retryable` boolean; retried work MUST reuse the same `correlation_id` and MAY be a
  new AIJob `id`.
- **CMOS-11-AI-025** AIJobs MUST be **cancellable** (`POST /ai/jobs/{id}:cancel`);
  cancellation is a state transition emitting the terminal event, not a hard delete
  (CMOS-02-DOM-003).
- **CMOS-11-AI-026** Every AIJob MUST carry the `correlation_id` of the operation that
  spawned it (e.g. the originating Call) so that AI results are traceable back to the
  workload (CMOS-00-ENG-004) and, where relevant, to a CDR (Volume 10).

## 5. Capabilities the surface enables (informative)

These are **use cases the contract makes possible**, not features CommOS implements.
Each is realised as an AIJob `kind` (open set, `kind` values are `SCREAMING_SNAKE_CASE`
labels agreed between tenant and consumer) over event/Object inputs:

| Capability | Typical trigger | Input refs | Result Object |
|-----------|-----------------|-----------|---------------|
| Transcription | `RecordingUploaded` | recording Object | transcript Object |
| Summarisation | `CallEnded` + transcript | transcript Object | summary Object |
| Sentiment / QA scoring | transcript | transcript Object | structured score Object |
| Translation | transcript | transcript Object | translated transcript Object |
| CRM enrichment | `CallEnded` | CDR + transcript refs | enrichment Object → webhook to CRM |
| Agent coaching | live stream | event/media stream (read-only) | coaching Object |
| Retrieval / knowledge | on-demand | Object corpus refs | answer Object |
| Fraud / anomaly | billing signals (Vol 10 §7) | CDR + signal events | risk Object |
| Lead / call scoring | `CallEnded` | CDR + transcript | score Object |
| Voice bot / IVR agent | live Call | streaming + REST commands | actions via API |

- **CMOS-11-AI-030** The platform MUST NOT special-case any of the above; each is an
  AIJob of a tenant-defined `kind`. Adding a capability MUST require no substrate
  change — only a new consumer and configuration. (Serves CMOS-00-ENG-001.)
- **CMOS-11-AI-031** A voice-bot/IVR consumer MUST act on a live Call only through the
  public API and Call Flow contract (Volume 7), issuing Commands; it MUST NOT reach
  into the media plane directly (CMOS-00-ENG-006).

## 6. Privacy, PII & tenant scoping (normative)

- **CMOS-11-AI-040** All AI inputs and outputs MUST be **tenant-scoped**. An AIJob, its
  `input_refs`, and its result Objects MUST share one `tenant_id`; cross-tenant
  reference is forbidden by construction (CMOS-00-ENG-008, CMOS-CONV-015).
- **CMOS-11-AI-041** Payloads exposed to consumers MUST apply **PII minimisation**:
  only fields necessary for the job's `kind` are included, and PII-bearing fields are
  marked `x-pii: true` (CMOS-05-EVT-042) so redaction is automatable.
- **CMOS-11-AI-042** The platform MUST support **redaction hooks**: a tenant MAY
  configure that Objects/fields passed to a consumer are redacted or tokenised (e.g.
  card numbers, national IDs) before the reference is resolvable by the consumer.
  Redaction is declarative configuration (CMOS-00-ENG-005).
- **CMOS-11-AI-043** Consumer access to an Object MUST be via **short-lived,
  capability-gated** resolution (signed URL or scoped token), consistent with
  zero-trust operation (CMOS-00-ENG-010); a raw, durable backend URL MUST NOT be
  handed to a consumer.
- **CMOS-11-AI-044** A tenant MUST be able to declare **data-residency / egress**
  constraints (e.g. "no payload may leave region X") and the platform MUST refuse to
  deliver to a consumer that violates the declared constraint. (Serves
  CMOS-00-ENG-008.)
- **CMOS-11-AI-045** All AIJob creation, claim, completion, cancellation, and Object
  resolution MUST be recorded as append-only `AuditEntry` records (CMOS-00-ENG-012),
  so "which consumer saw which Object for which tenant" is always reconstructable.

## 7. Security & metering

- **CMOS-11-AI-050** A consumer MUST authenticate and MUST hold explicit Capabilities
  to subscribe, create/claim AIJobs, and resolve Objects (`ai.jobs.write`,
  `ai.jobs.claim`, `ai.objects.read`, …); authorization is capability-based, never
  role-implicit (CMOS-00-ENG-009).
- **CMOS-11-AI-051** AIJob execution MAY be **metered** and rated as a non-voice
  workload through Volume 10 (per-job or per-unit), emitting a CDR-class record; the
  metering path MUST reuse the billing contract, not a parallel one (CMOS-11-AI-004).
- **CMOS-11-AI-052** The substrate MUST remain fully functional with **zero** AI
  consumers configured; AI is strictly additive and MUST NOT be on any core call path
  (CMOS-00-ENG-001, N-2).

## Conformance notes

- **L1 (Contract):** `AIJobQueued`/`AIJobStarted`/`AIJobCompleted`/`AIJobFailed`
  envelopes and `data` validate against the frozen schemas; `AIJob` instances validate
  against the entity schema; `input_refs`/result carry only references.
- **L2 (Behavioural):** a driven scenario — `RecordingUploaded` → AIJob(`TRANSCRIPTION`)
  → `COMPLETED` with a transcript Object — produces the exact event set/order
  (CMOS-11-AI-021); re-delivery of the trigger creates no duplicate job
  (CMOS-11-AI-014); a cross-tenant `input_ref` is rejected (CMOS-11-AI-040); a
  consumer lacking `ai.objects.read` cannot resolve the recording (CMOS-11-AI-050);
  disabling all consumers leaves call flow unaffected (CMOS-11-AI-052).
- **L3 (Interoperable):** the same event/Object contract is consumed unmodified by two
  different external stacks (e.g. a hosted model via webhook and a self-hosted model
  via bus cursor), each returning results through the AIJob lifecycle.
- The harness (`conformance/run.py`) checks AIJob event catalog↔schema↔example
  consistency and that no substrate contract references a vendor.

## Open items
- REST surface detail for `POST /ai/jobs`, `GET /ai/jobs/{id}`, `:cancel`, and the
  claim/lease protocol (`POST /ai/jobs/{id}:claim`) — to be added in Volume 4.
- Streaming tail contract (read-only event/media stream) profile — reserved for v0.4.
- Redaction-hook configuration schema and tokenisation vault interface — reserved.
- Standard result-Object schemas per common `kind` (transcript, summary, score) as
  optional interop conveniences — reserved (kept vendor-neutral).

## Change log
- **0.3.0** — Initial implementation-grade draft: five-primitive integration surface,
  subscription/triggering rules, the QUEUED→RUNNING→COMPLETED/FAILED AIJob lifecycle
  with result Objects, capability catalogue (vendor-neutral), and PII/tenant-scoping/
  redaction/residency controls.
