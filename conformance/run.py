#!/usr/bin/env python3
"""CommOS executable conformance harness.

This is the arbiter of "does this conform" for the *contracts themselves*
(Volume 16). It runs three suites over contracts/ and spec/:

  1. schema-validity   — every JSON Schema is a valid 2020-12 schema and all
                         cross-file $refs resolve.
  2. consistency       — catalog.md (Vol 5) <-> event schemas <-> examples are in
                         1:1 agreement; every domain entity named in Volume 2 that
                         has a schema has an example; the OpenAPI parses.
  3. examples-valid    — every example instance validates against its schema.

Exit code 0 = all pass. Non-zero = at least one failure (suitable for CI).

Usage:
    python3 -m pip install jsonschema
    python3 conformance/run.py [--repo <path>]
"""
from __future__ import annotations
import json
import re
import sys
import argparse
from pathlib import Path

try:
    from jsonschema import Draft202012Validator
    from referencing import Registry, Resource
except ImportError:
    sys.stderr.write(
        "ERROR: missing dependency. Run:  python3 -m pip install jsonschema\n"
    )
    sys.exit(2)

# ------------------------------------------------------------------ paths -----
def repo_root(cli: str | None) -> Path:
    if cli:
        return Path(cli).resolve()
    # this file is <repo>/conformance/run.py
    return Path(__file__).resolve().parent.parent

# ------------------------------------------------------------------ result ----
class Report:
    def __init__(self) -> None:
        self.passed = 0
        self.failures: list[str] = []
        self.notes: list[str] = []

    def ok(self, msg: str) -> None:
        self.passed += 1
        print(f"  \033[32mPASS\033[0m {msg}")

    def fail(self, msg: str) -> None:
        self.failures.append(msg)
        print(f"  \033[31mFAIL\033[0m {msg}")

    def note(self, msg: str) -> None:
        self.notes.append(msg)
        print(f"  \033[33mNOTE\033[0m {msg}")


def load_json(p: Path):
    with p.open() as f:
        return json.load(f)


def build_registry(schema_files: list[Path], rep: Report) -> Registry:
    resources = []
    for p in schema_files:
        try:
            doc = load_json(p)
        except json.JSONDecodeError as e:
            rep.fail(f"{p.name}: invalid JSON — {e}")
            continue
        sid = doc.get("$id")
        if not sid:
            rep.fail(f"{p.name}: missing $id (required for the contract registry)")
            continue
        resources.append((sid, Resource.from_contents(doc)))
    return Registry().with_resources(resources)


# --------------------------------------------------------------- suite 1 ------
def suite_schema_validity(sch_dir: Path, schema_files: list[Path], registry: Registry, rep: Report):
    print("\n[1] schema-validity")
    for p in schema_files:
        try:
            doc = load_json(p)
        except json.JSONDecodeError:
            continue  # already reported
        try:
            Draft202012Validator.check_schema(doc)
        except Exception as e:  # noqa: BLE001
            rep.fail(f"{p.relative_to(sch_dir.parent)}: not a valid JSON Schema — {e}")
            continue
        # force $ref resolution by constructing a validator bound to the registry
        try:
            Draft202012Validator(doc, registry=registry)
        except Exception as e:  # noqa: BLE001
            rep.fail(f"{p.name}: validator construction failed — {e}")
            continue
        rep.ok(f"{p.relative_to(sch_dir.parent)} is a valid schema")


# --------------------------------------------------------------- suite 2 ------
CORE_LINK = re.compile(r"\[core\]\([^)]*events/([A-Za-z0-9]+)\.schema\.json\)")
ANY_EVENT_ROW = re.compile(r"^\|\s*`([A-Za-z0-9]+)`", re.M)


def suite_consistency(root: Path, sch_dir: Path, rep: Report):
    print("\n[2] consistency")
    catalog = (root / "spec/005-events/catalog.md").read_text()
    core_events = set(CORE_LINK.findall(catalog))
    event_schema_files = {p.stem.replace(".schema", "")
                          for p in (sch_dir / "events").glob("*.schema.json")}
    event_examples = {p.stem for p in (sch_dir / "events/examples").glob("*.json")}

    # every 'core' catalog event has a schema
    for name in sorted(core_events):
        if name in event_schema_files:
            rep.ok(f"catalog 'core' event {name} has a schema")
        else:
            rep.fail(f"catalog marks {name} as core but no schema exists")

    # every event schema is referenced as 'core' in the catalog
    for name in sorted(event_schema_files):
        if name not in core_events:
            rep.fail(f"event schema {name} exists but catalog does not mark it 'core'")
        else:
            rep.ok(f"event schema {name} is catalogued")

    # every event schema has an example
    for name in sorted(event_schema_files):
        if name in event_examples:
            rep.ok(f"event {name} has an example")
        else:
            rep.fail(f"event schema {name} has no example instance")

    # every domain entity schema has an example
    ent_dir = sch_dir / "entities"
    ent_schemas = {p.stem.replace(".schema", "") for p in ent_dir.glob("*.schema.json")}
    ent_examples = {p.stem for p in (ent_dir / "examples").glob("*.json")}
    for name in sorted(ent_schemas):
        if name in ent_examples:
            rep.ok(f"entity {name} has an example")
        else:
            rep.fail(f"entity schema {name} has no example instance")

    # OpenAPI parses (YAML if available, else structural sniff)
    oapi = root / "contracts/openapi/commos.openapi.yaml"
    try:
        import yaml  # type: ignore
        doc = yaml.safe_load(oapi.read_text())
        if isinstance(doc, dict) and doc.get("openapi", "").startswith("3."):
            rep.ok(f"OpenAPI parses and declares {doc['openapi']}")
        else:
            rep.fail("OpenAPI document missing a 3.x 'openapi' version")
    except ImportError:
        head = oapi.read_text(4096)
        if head.lstrip().startswith("openapi: 3."):
            rep.note("pyyaml not installed; OpenAPI checked structurally only "
                     "(pip install pyyaml for full parse)")
            rep.ok("OpenAPI declares a 3.x version (structural check)")
        else:
            rep.fail("OpenAPI does not start with 'openapi: 3.'")


# --------------------------------------------------------------- suite 3 ------
def _validate_instance(schema_path: Path, instance_path: Path, registry: Registry, rep: Report, label: str):
    schema = load_json(schema_path)
    instance = load_json(instance_path)
    validator = Draft202012Validator(schema, registry=registry)
    errors = sorted(validator.iter_errors(instance), key=lambda e: list(e.path))
    if not errors:
        rep.ok(f"{label} {instance_path.name} validates")
    else:
        for e in errors:
            loc = "/".join(str(x) for x in e.path) or "(root)"
            rep.fail(f"{label} {instance_path.name}: {loc}: {e.message}")


def suite_examples_valid(sch_dir: Path, registry: Registry, rep: Report):
    print("\n[3] examples-valid")
    # events
    ev_dir = sch_dir / "events"
    for ex in sorted((ev_dir / "examples").glob("*.json")):
        schema = ev_dir / f"{ex.stem}.schema.json"
        if schema.exists():
            _validate_instance(schema, ex, registry, rep, "event")
        else:
            rep.fail(f"event example {ex.name} has no matching schema")
    # entities
    ent_dir = sch_dir / "entities"
    for ex in sorted((ent_dir / "examples").glob("*.json")):
        schema = ent_dir / f"{ex.stem}.schema.json"
        if schema.exists():
            _validate_instance(schema, ex, registry, rep, "entity")
        else:
            rep.fail(f"entity example {ex.name} has no matching schema")


# ------------------------------------------------------------------ main ------
def main() -> int:
    ap = argparse.ArgumentParser(description="CommOS conformance harness")
    ap.add_argument("--repo", help="repo root (default: inferred)")
    args = ap.parse_args()

    root = repo_root(args.repo)
    sch_dir = root / "contracts/json-schema"
    if not sch_dir.exists():
        sys.stderr.write(f"ERROR: {sch_dir} not found (wrong --repo?)\n")
        return 2

    schema_files = sorted(sch_dir.rglob("*.schema.json"))
    rep = Report()

    print(f"CommOS conformance harness — {len(schema_files)} schema file(s) under {sch_dir.relative_to(root)}")
    registry = build_registry(schema_files, rep)

    suite_schema_validity(sch_dir, schema_files, registry, rep)
    suite_consistency(root, sch_dir, rep)
    suite_examples_valid(sch_dir, registry, rep)

    print("\n" + "=" * 60)
    print(f"PASS: {rep.passed}   FAIL: {len(rep.failures)}   NOTE: {len(rep.notes)}")
    if rep.failures:
        print("\nFailures:")
        for f in rep.failures:
            print(f"  - {f}")
        return 1
    print("\nAll conformance checks passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
