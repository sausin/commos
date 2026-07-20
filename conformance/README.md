# CommOS Conformance

The conformance harness is the **arbiter of "does this conform"** (Volume 16). In a
specification-first project the tests are derived from the specification and are the
executable form of it: an implementation is conformant when it passes the suite for
its declared profile and level (see
[CONVENTIONS §3](../spec/CONVENTIONS.md#3-conformance-profiles-and-levels)).

## What runs today (`run.py`)

`run.py` validates the **contracts themselves** — the foundation every implementation
builds against — in four suites (500+ checks):

1. **schema-validity** — every file under `contracts/json-schema/` is a valid
   JSON Schema (2020-12) and all cross-file `$ref`s resolve through the contract
   registry (keyed by `$id`).
2. **consistency** — the Volume 5 event catalogue, the event schemas, and the event
   examples are in 1:1 agreement; every domain entity schema has an example; the
   OpenAPI document parses and all its `../json-schema/…` `$ref`s resolve to files.
3. **examples-valid** — every example instance validates against its schema, for
   every contract directory with an `examples/` subfolder (entities, events,
   interfaces).
4. **behavioural-scenarios** — every L2 scenario under `scenarios/` validates against
   `scenarios/scenario.schema.json` and each `expect_event` names a real, catalogued
   event.

```bash
python3 -m pip install jsonschema pyyaml   # jsonschema required; pyyaml for full OpenAPI parse
python3 conformance/run.py                 # exit 0 = pass, 1 = failure (CI-ready)
```

This is the gate for promoting a spine volume from `REVIEW` to `FROZEN`
(CONVENTIONS §5): the shapes it defines must exist here and this must be green.

## What comes next (roadmap)

The same harness grows outward along the conformance levels:

- **L1 Contract** (today): schema/shape validation of emitted entities, events, and
  API bodies. `run.py` covers the contract self-consistency; an adapter that points
  the same validators at a running implementation's `/v1/openapi.json`,
  `/v1/events/schemas`, and captured event stream is the next addition.
- **L2 Behavioural** (definitions landed): scenario tests derived from the Volume 2
  state machines — drive a Call/Device/Identity through legal and illegal transitions
  and assert (a) illegal transitions are rejected and (b) exactly the specified events
  are emitted, in `sequence` order, with a shared `correlation_id`; re-delivery is a
  no-op on an idempotent consumer (Volume 5 §3–§4). The scenarios exist under
  `scenarios/` and are validated structurally today; executing them against an
  implementation needs the adapter below.
- **L3 Interoperable** (planned): real SIP endpoints, real vendor devices, and
  cross-implementation event exchange (Volume 7/8/16).

## Layout

```
conformance/
├── run.py                    # the harness (suites 1–4)
├── scenarios/                # L2 behavioural scenarios (state-machine driven)
│   ├── scenario.schema.json  #   the scenario contract
│   └── *.json                #   one scenario per flow; each cites requirement IDs
└── README.md                 # this file
```

Each scenario cites the requirement IDs (`CMOS-<VOL>-<SUBSYS>-NNN`) it exercises. The
implementation `adapter/` (points the validators + scenario runner at a live
implementation's `/v1/openapi.json`, `/v1/events/schemas`, and event stream) is the
next addition to turn L2 definitions into executed L2/L3 conformance.
