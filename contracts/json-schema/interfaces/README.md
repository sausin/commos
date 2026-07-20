# Subsystem Interface Contracts

Typed message contracts for the boundaries **between** CommOS subsystems. These make
the control/media split (ADR-0010, CMOS-03-ARCH-001..003) and the plugin ABIs
concrete, so an alternative media engine or provisioning plugin can be substituted
without breaking compatibility.

| Interface | Direction | Purpose | Spec |
|-----------|-----------|---------|------|
| `ControlMediaCommand` | control → media | Typed commands (originate, answer, hangup, transfer, record, conference, DTMF). | Vol 3 |
| `ControlMediaFact` | media → control | Typed facts (signalling state, media quality) — the source of Vol 5 media/call events. | Vol 3 |
| `ProvisionerBuildRequest` | host → plugin | Vendor-neutral desired device state handed to a Provisioner. | Vol 8 |
| `ProvisionerBuildResult` | plugin → host | Config Object reference + typed error taxonomy. | Vol 8 |
| `PolicyDecisionRequest` | caller → policy engine | Action + subject + context for evaluation. | Vol 9 |
| `PolicyDecision` | policy engine → caller | `ALLOW`/`DENY` + obligations (`REQUIRE_IDENTITY`/`REQUIRE_APPROVAL`). | Vol 9 |
| `RatingRequest` | billing → rating | Deterministic, reproducible rating input (profile version pinned). | Vol 10 |
| `RatingResult` | rating → billing | Cost feeding the CDR. | Vol 10 |

Each interface has a validated example under `examples/`. Media never reads
control-plane memory; it exchanges only these messages — which is what lets the media
plane be split into separate processes/nodes with no control-plane redesign
(CMOS-03-ARCH-002).
