# Running commosd against PostgreSQL

CommOS boots with **zero external dependencies** on its in-process store — that is the
single-binary dev/edge posture. PostgreSQL is the intended **durable system of record** at
scale, and the only hard dependency the binary ever takes (CMOS-14-DEP-020). This guide
runs `commosd` against a local PostgreSQL.

Config-as-code never carries secrets (CMOS-14-DEP-083): `pbx.yaml` holds a *reference* to
the DSN, and the process resolves it from the environment at boot.

## 1. Start PostgreSQL

```bash
cd reference
docker compose -f deploy/docker-compose.yml up -d postgres
```

`up` blocks until the `pg_isready` healthcheck passes. The default dev password is
`commos-dev-password`; override it for anything real:

```bash
export POSTGRES_PASSWORD=$(openssl rand -hex 16)
docker compose -f deploy/docker-compose.yml up -d postgres
```

## 2. Point commosd at it

Export the DSN. The variable name must match what `pbx.yaml` references (`env://DATABASE_URL`):

```bash
export DATABASE_URL="postgres://commos:${POSTGRES_PASSWORD:-commos-dev-password}@localhost:5432/commos"
```

Then enable the `database_url` reference in your `pbx.yaml`. Copy `deploy/pbx.example.yaml`
and set:

```yaml
listen: "0.0.0.0:8080"

database_url:
  ref_uri: "env://DATABASE_URL"   # resolved from $DATABASE_URL at boot
```

Leaving `database_url` out entirely keeps commosd on the zero-dependency in-process store.
Never inline a DSN with credentials into the YAML — commosd rejects it (CMOS-14-DEP-083).

## 3. Run the binary

```bash
./target/x86_64-unknown-linux-gnu/release/commosd --config pbx.yaml
```

Schema migrations run **automatically at boot** — there is no separate migration step.
`commosd` gates `/readyz` on the store being reachable and migrated, so a green `/readyz`
means it is serving against PostgreSQL.

## 4. Smoke-test it

```bash
scripts/smoke.sh                       # defaults to http://localhost:8080
scripts/smoke.sh http://host:8080      # or an explicit base URL
```

The script drives the originate-a-call slice end to end: `/livez`, `/readyz`, `/info`,
create/get/list on `/v1/calls`, and asserts the `CallStarted` event appears in the outbox
(`/_introspect/events`). It exits non-zero on the first failure. The same script passes
against either backing store — the contract surface is identical.

## Data lifecycle

The `commos-pgdata` named volume persists across `docker compose down`. To wipe it:

```bash
docker compose -f deploy/docker-compose.yml down -v
```
