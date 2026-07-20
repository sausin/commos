# Next steps

This package is a **living** specification. The complete target exceeds 1,000 pages
and is expanded volume-by-volume under version control (CONVENTIONS §5 governs
promotion from DRAFT → REVIEW → FROZEN).

## Where v0.3 stands
- **Spine at REVIEW with contracts + green harness:** Volumes 0, 2, 3, 4, 5.
- **Machine-readable contracts:** `contracts/json-schema` (envelope, 7 core entities,
  14 core events + examples) and `contracts/openapi/commos.openapi.yaml`.
- **Executable conformance:** `conformance/run.py` (schema-validity, consistency,
  examples-valid) wired into CI (`.github/workflows/conformance.yml`).
- **Breadth volumes drafted to implementation grade:** 1, 6–18. ADRs (19) at REVIEW.

## The path to v0.4 (freeze the spine, widen the contracts)
1. External review of the spine; resolve REVIEW → FROZEN per volume.
2. Add contracts for the next tier: `json-schema/cdr`, `provisioning`, and the
   remaining events currently marked `planned` in `spec/005-events/catalog.md`.
3. Land the typed control↔media interface (IDL) under `contracts/`.
4. Build the **L2 behavioural** conformance suite from the Volume 2 state machines
   (drive transitions; assert event set/order/idempotency).
5. Introduce the messaging/video/contact-centre workload entities (Volume 2 Open
   items) — proving "voice is one workload" with a second workload.
