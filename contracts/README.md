# CommOS Machine-Readable Contracts

These artifacts are **normative** (CONVENTIONS §8). They are the source of truth for
the *shape* of every entity, event, and API message. The prose in `spec/` is the
source of *meaning*; where they disagree about shape, the contract wins.

As of v0.4 the contract set is **complete**: every domain entity (Volume 2) and every
catalogued event (Volume 5) has a schema and a validated example, the API surface
(Volume 4) is fully covered, and the subsystem interfaces are typed.

```
contracts/
├── json-schema/
│   ├── common.schema.json          # shared $defs: Uuid, Timestamp, Money, EntityBase…
│   ├── envelope.schema.json        # the canonical event envelope (Volume 5 §2)
│   ├── entities/                   # 36 schemas — every domain entity (Volume 2)
│   │   └── examples/               #   a valid example instance per entity
│   ├── events/                     # 74 schemas — every canonical event (Volume 5)
│   │   └── examples/               #   a valid example instance per event
│   └── interfaces/                 # 8 typed subsystem interfaces (control↔media,
│       └── examples/               #   Provisioner ABI, Policy, Rating) + examples
└── openapi/
    └── commos.openapi.yaml         # 91 paths — the full API surface (Volume 4)
```

Counts are asserted by the conformance harness, not hand-maintained; run it to verify.

## Conventions

- JSON Schema dialect: **2020-12**.
- `$id` base: `https://commos.dev/schemas/`. Cross-file `$ref`s use absolute `$id`
  URIs so the schema set is a self-contained registry.
- Types follow [CONVENTIONS §6](../spec/CONVENTIONS.md#6-identifiers-data-types-and-encoding):
  UUIDv7 strings, RFC 3339 UTC millisecond timestamps, money as
  `{currency, minor_units}`.
- Event schemas are `allOf [envelope]` with a `type` const and a typed `data`
  payload; each has a matching example under `events/examples/`.
- Any contract directory with an `examples/` subfolder has every example validated
  against its sibling schema by the harness (entities, events, interfaces alike).

## Freeze rule

A volume cannot be `FROZEN` (CONVENTIONS §5) unless the entities/events it defines
have schemas here and the conformance harness passes:

```bash
python3 -m pip install jsonschema
python3 ../conformance/run.py      # from contracts/, or: python3 conformance/run.py from repo root
```

The harness enforces catalog↔schema↔example consistency and validates all examples.
