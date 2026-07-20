# CommOS Machine-Readable Contracts

These artifacts are **normative** (CONVENTIONS §8). They are the source of truth for
the *shape* of every entity, event, and API message. The prose in `spec/` is the
source of *meaning*; where they disagree about shape, the contract wins.

```
contracts/
├── json-schema/
│   ├── common.schema.json          # shared $defs: Uuid, Timestamp, Money, EntityBase…
│   ├── envelope.schema.json        # the canonical event envelope (Volume 5 §2)
│   ├── entities/                   # one schema per domain entity (Volume 2)
│   │   ├── Organisation.schema.json  User.schema.json  Identity.schema.json
│   │   ├── Device.schema.json  Call.schema.json  Object.schema.json  CDR.schema.json
│   └── events/                     # one schema per canonical event (Volume 5)
│       ├── <EventName>.schema.json # allOf envelope + type const + typed `data`
│       └── examples/               # a valid example instance per event
└── openapi/
    └── commos.openapi.yaml         # the API surface (Volume 4)
```

## Conventions

- JSON Schema dialect: **2020-12**.
- `$id` base: `https://commos.dev/schemas/`. Cross-file `$ref`s use absolute `$id`
  URIs so the schema set is a self-contained registry.
- Types follow [CONVENTIONS §6](../spec/CONVENTIONS.md#6-identifiers-data-types-and-encoding):
  UUIDv7 strings, RFC 3339 UTC millisecond timestamps, money as
  `{currency, minor_units}`.
- Event schemas are `allOf [envelope]` with a `type` const and a typed `data`
  payload; each has a matching example under `events/examples/`.

## Freeze rule

A volume cannot be `FROZEN` (CONVENTIONS §5) unless the entities/events it defines
have schemas here and the conformance harness passes:

```bash
python3 -m pip install jsonschema
python3 ../conformance/run.py      # from contracts/, or: python3 conformance/run.py from repo root
```

The harness enforces catalog↔schema↔example consistency and validates all examples.
