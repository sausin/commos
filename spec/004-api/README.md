# Volume 4 — API

**Status:** REVIEW · **Version:** 0.3.0 · **Subsystem tag:** API

Everything the platform can do is available through this API — the web UI, CLI,
mobile app, and Terraform provider are all clients of the *same* API, with no
privileged back door (CMOS-00-ENG-003). Commands mutate entities (Volume 2) and
cause events (Volume 5).

Machine-readable form:
[`contracts/openapi/commos.openapi.yaml`](../../contracts/openapi/commos.openapi.yaml).
Where this prose and the OpenAPI document disagree about *shape*, the OpenAPI wins
(CONVENTIONS §8). Endpoint catalogue: [`endpoints.md`](endpoints.md).

---

## 1. Style & transport (normative)

- **CMOS-04-API-001** The API is **resource-oriented HTTP/JSON** over TLS 1.3+.
  Resources are plural nouns (`/users`, `/devices`, `/calls`). Long-running or
  non-CRUD actions use the `:verb` sub-resource form
  (`POST /devices/{id}:approve`) and MUST emit the corresponding event.
- **CMOS-04-API-002** Verbs: `GET` (safe, cacheable), `POST` (create/action), `PUT`
  (full replace), `PATCH` (partial, JSON Merge Patch RFC 7386), `DELETE` (transition
  to a terminal state, not hard delete — CMOS-02-DOM-003).
- **CMOS-04-API-003** All bodies are `application/json`; field naming per
  CONVENTIONS §6 (`snake_case`, RFC 3339 UTC time, money objects, UUIDv7 ids).
- **CMOS-04-API-004** Real-time streams are exposed over **WebSocket** (`/stream`)
  and/or **Server-Sent Events** (`/events`), carrying the Volume 5 envelope
  verbatim. Webhooks (Volume 5 §3) are the push transport for external consumers.

## 2. Versioning (normative)

- **CMOS-04-API-010** The API is versioned by URL major: `/v1/...`. Within a major,
  changes are additive per CONVENTIONS §4 (new endpoints, new optional fields, new
  enum values behind capability negotiation).
- **CMOS-04-API-011** Clients MUST ignore unknown response fields (tolerant reader).
- **CMOS-04-API-012** Removing an endpoint or field, or tightening a constraint, is a
  new API major and requires an ADR.

## 3. Authentication & authorization (normative)

- **CMOS-04-API-020** Every request is authenticated. Supported credentials: OAuth2
  bearer / OIDC access tokens (users), and scoped API keys (machines). mTLS MAY be
  required for provisioning and admin planes (Volume 8/9).
- **CMOS-04-API-021** Authorization is **capability-based** (CMOS-00-ENG-009). Each
  endpoint declares the Capability it requires (e.g. `provision.devices`,
  `billing.export`). A missing capability yields `403` with error `forbidden`.
- **CMOS-04-API-022** Every request is **tenant-scoped** from the credential; a
  request MUST NOT be able to name another tenant's resources (CMOS-00-ENG-008).
- **CMOS-04-API-023** Mutating requests SHOULD carry an `Idempotency-Key` header;
  the server MUST apply the operation at most once per (endpoint, key, tenant) and
  return the original result on retry.

## 4. Pagination, filtering, sorting (normative)

- **CMOS-04-API-030** Collections are **cursor-paginated**:
  `?limit=<n>&cursor=<opaque>`; responses carry `{ "items": [...], "next_cursor":
  <opaque|null> }`. Offset pagination is not offered (unstable under writes).
- **CMOS-04-API-031** Filtering uses explicit query params documented per endpoint;
  sorting uses `?sort=<field>&order=asc|desc`. Default order is `created_at desc`.
- **CMOS-04-API-032** List endpoints default to `limit=50`, max `200`.

## 5. Errors (normative)

- **CMOS-04-API-040** Errors use **RFC 9457 Problem Details** (`application/problem+json`):
  `{ "type": <uri>, "title", "status", "detail", "instance", "code",
  "correlation_id", "errors": [ {"field","message"} ] }`.
- **CMOS-04-API-041** `code` is a stable machine string (`forbidden`,
  `validation_failed`, `conflict`, `not_found`, `rate_limited`,
  `precondition_failed`, `policy_denied`, …). Clients branch on `code`, never on
  `title`/`detail`.
- **CMOS-04-API-042** Every error response carries the request's `correlation_id`
  (also returned in the `X-Correlation-Id` header on all responses) for tracing.

## 6. Concurrency & consistency (normative)

- **CMOS-04-API-050** Entities expose their Digital-Twin `version`. Conditional
  updates use `If-Match: <version>`; a stale write returns `412 precondition_failed`.
- **CMOS-04-API-051** Reads are read-your-writes consistent within a tenant for the
  authenticated principal.

## 7. Rate limiting & quotas
- **CMOS-04-API-060** Limits are per-credential and per-tenant; responses carry
  `RateLimit-*` headers (draft IETF). Exceeding yields `429 rate_limited` with
  `Retry-After`.

## 8. Discoverability
- **CMOS-04-API-070** The server publishes its OpenAPI at `GET /v1/openapi.json` and
  the event schema registry at `GET /v1/events/schemas`. These MUST match the frozen
  contracts for the declared version.

## 9. Endpoint catalogue (summary)

Full list with request/response shapes in [`endpoints.md`](endpoints.md) and the
OpenAPI. Core resources:

```
/v1/organisations        /v1/users            /v1/identities:authenticate
/v1/departments          /v1/cost-centres     /v1/capabilities  /v1/policies
/v1/devices  /v1/devices/{id}:approve  :reject  :provision  :replace  :retire
/v1/extensions  /v1/dids  /v1/carriers  /v1/gateways  /v1/trunks
/v1/routes  /v1/call-flows  /v1/call-flows/{id}:publish  /v1/ivrs  /v1/queues
/v1/calls  /v1/calls/{id}:transfer  :hold  :hangup   /v1/conferences
/v1/recordings  /v1/voicemails  /v1/objects
/v1/cdrs  /v1/billing/exports
/v1/webhooks  /v1/automations  /v1/ai/jobs  /v1/plugins  /v1/audit
/v1/secrets  /v1/certificates
/v1/stream (WS)  /v1/events (SSE)  /v1/openapi.json  /v1/events/schemas
```

## 10. Conformance notes
- L1: request/response bodies validate against the OpenAPI schemas; error bodies are
  valid Problem Details; pagination/idempotency headers honoured.
- L2: documented mutations emit the correct Volume 5 events with a shared
  `correlation_id`; `If-Match`/idempotency semantics hold under retry/conflict.

## 11. Open items
- Bulk/batch endpoints and SCIM provisioning surface — Volume 9.
- GraphQL read model — explicitly deferred (see Volume 19 ADR candidate).

## Change log
- **0.3.0** — API conventions (auth, pagination, errors, concurrency, versioning)
  and core endpoint catalogue defined; OpenAPI skeleton added.
