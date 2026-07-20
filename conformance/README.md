# CommOS Conformance

The conformance harness is the **arbiter of "does this conform"** (Volume 16). In a
specification-first project the tests are derived from the specification and are the
executable form of it: an implementation is conformant when it passes the suite for
its declared profile and level (see
[CONVENTIONS §3](../spec/CONVENTIONS.md#3-conformance-profiles-and-levels)).

## What runs today (`run.py`)

`run.py` validates the **contracts themselves** — the foundation every implementation
builds against — in three suites:

1. **schema-validity** — every file under `contracts/json-schema/` is a valid
   JSON Schema (2020-12) and all cross-file `$ref`s resolve through the contract
   registry (keyed by `$id`).
2. **consistency** — the Volume 5 event catalogue, the event schemas, and the event
   examples are in 1:1 agreement; every domain entity schema has an example; the
   OpenAPI document parses.
3. **examples-valid** — every example instance validates against its schema.

```bash
python3 -m pip install jsonschema     # required; pip install pyyaml for full OpenAPI parse
python3 conformance/run.py            # exit 0 = pass, 1 = failure (CI-ready)
```

This is the gate for promoting a spine volume from `REVIEW` to `FROZEN`
(CONVENTIONS §5): the shapes it defines must exist here and this must be green.

## What comes next (roadmap)

The same harness grows outward along the conformance levels:

- **L1 Contract** (today): schema/shape validation of emitted entities, events, and
  API bodies. `run.py` covers the contract self-consistency; an adapter that points
  the same validators at a running implementation's `/v1/openapi.json`,
  `/v1/events/schemas`, and captured event stream is the next addition.
- **L2 Behavioural** (planned): scenario tests derived from the Volume 2 state
  machines — drive a Call/Device/Identity through legal and illegal transitions and
  assert (a) illegal transitions are rejected and (b) exactly the specified events
  are emitted, in `sequence` order, with a shared `correlation_id`; re-delivery is a
  no-op on an idempotent consumer (Volume 5 §3–§4).
- **L3 Interoperable** (planned): real SIP endpoints, real vendor devices, and
  cross-implementation event exchange (Volume 7/8/16).

## Layout

```
conformance/
├── run.py          # the harness (suites 1–3 above)
└── README.md       # this file
```

Behavioural fixtures (`scenarios/`) and the implementation adapter (`adapter/`) are
added as L2/L3 land. Each scenario cites the requirement IDs
(`CMOS-<VOL>-<SUBSYS>-NNN`) it exercises.
