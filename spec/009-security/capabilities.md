# Capability Catalogue

**Status:** DRAFT · **Version:** 0.3.0 · **Subsystem tag:** SEC

Companion to [`README.md`](README.md). This is the **canonical catalogue** of
`Capability` keys — the fine-grained, grantable permissions that are CommOS's *only*
authorization primitive (CMOS-00-ENG-009, CMOS-09-SEC-010). Every API endpoint
(Volume 4 [endpoints.md](../004-api/endpoints.md)) declares exactly one of these keys;
the keys here are consistent with those declarations.

Roles do not appear here: a "role" is only a UI-presented bundle of these keys and has
no enforcement meaning (CMOS-09-SEC-012).

Rules:
- **CMOS-09-SEC-080** A capability `key` matches
  `^[a-z][a-z0-9]*(\.[a-z][a-z0-9]*)+$` and is **permanent within a MAJOR line**;
  adding a key is a MINOR change, removing/renaming one is MAJOR (CMOS-CONV-002/003).
- **CMOS-09-SEC-081** This catalogue is authoritative for the *set* of keys and their
  meaning; where an endpoint in Volume 4 declares a key, that key MUST appear here with
  a compatible meaning. Where the (future) machine-readable catalogue under
  `contracts/` and this prose disagree about the *set*, the contract wins
  (CONVENTIONS §8); until it exists, this prose is authoritative.
- **CMOS-09-SEC-082** Capability check is necessary but not sufficient: an authorized
  action is still subject to Policy evaluation (`REQUIRE_IDENTITY`/`REQUIRE_APPROVAL`/
  `DENY`), ANDed with the capability (CMOS-09-SEC-013).

Legend: **API area** names the primary Volume 4 endpoints the key gates. "Introduced
here" marks keys this volume adds beyond those already declared in Volume 4.

---

## Tenancy, people & IAM
| Key | Description | API area |
|-----|-------------|----------|
| `org.manage` | Manage the Organisation, Departments, and tenant settings. | `/organisations`, `/departments` |
| `users.manage` | Create, update, deactivate Users. | `/users` |
| `iam.manage` | Grant/revoke Capabilities to Users; manage the capability model. | `/capabilities`, `/users/{id}:grant` `:revoke` |
| `policy.manage` | Create and edit Policies evaluated by the policy engine. | `/policies` |

## Devices & provisioning
| Key | Description | API area |
|-----|-------------|----------|
| `devices.view` | Read Devices and the discovery/approval inbox. | `GET /devices` |
| `provision.devices` | Approve, reject, provision, replace, retire Devices; drive firmware; the device-security actions of Volume 8. | `/devices/{id}:approve` `:reject` `:provision` `:replace` `:retire` |

## Numbering & carriers
| Key | Description | API area |
|-----|-------------|----------|
| `numbering.manage` | Manage Extensions and DIDs. | `/extensions`, `/dids` |
| `carriers.manage` | Manage Carriers, Gateways, and Trunks. | `/carriers`, `/gateways`, `/trunks` |

## Routing & call control
| Key | Description | API area |
|-----|-------------|----------|
| `routing.manage` | Manage Routes, Call Flows (incl. publish), IVRs, and Queues. | `/routes`, `/call-flows`, `/ivrs`, `/queues` |
| `calls.view` | Read Calls and call state. | `GET /calls` |
| `calls.originate` | Originate (place) a Call. | `POST /calls` |
| `calls.control` | Control a live Call: transfer, hold/resume, hangup. | `/calls/{id}:transfer` `:hold` `:resume` `:hangup` |
| `calls.dial.international` | **Introduced here.** Place calls to the international / elevated-cost class where policy gates it by capability rather than per-call approval (CMOS-09-SEC-022). | policy-gated `POST /calls` |
| `conf.manage` | Manage Conferences and invite parties. | `/conferences` |

> Note: the Emergency Override (CMOS-09-SEC-023) requires **no** capability by design —
> an emergency call is permitted even to a principal holding none of the above.

## Media, objects & billing
| Key | Description | API area |
|-----|-------------|----------|
| `recordings.view` | Read Recordings (Object references). | `/recordings` |
| `voicemail.view` | Read Voicemails. | `/voicemails` |
| `objects.rw` | Read/write Objects via presigned upload/download. | `/objects` |
| `billing.manage` | Manage Cost Centres and billing configuration. | `/cost-centres` |
| `billing.view` | Read CDRs. | `/cdrs` |
| `billing.export` | Create billing exports. | `/billing/exports` |

## Integration & extensibility
| Key | Description | API area |
|-----|-------------|----------|
| `integrations.manage` | Manage Webhooks and Automations. | `/webhooks`, `/automations` |
| `ai.jobs` | Submit and read AI Jobs (external processing surface). | `/ai/jobs` |
| `plugins.manage` | Install, enable/disable, and configure Plugins (incl. Provisioners). | `/plugins` |

## Security, audit, secrets & PKI
| Key | Description | API area |
|-----|-------------|----------|
| `audit.view` | Read the append-only audit log. **No key edits or deletes it** (CMOS-09-SEC-042). | `GET /audit` |
| `secrets.manage` | **Introduced here.** Manage secret backends and secret references/handles (CMOS-09-SEC-054). No capability returns plaintext secret values. | secrets admin surface (§8) |
| `certs.manage` | **Introduced here.** Manage the PKI: issue/renew/revoke certificates and trust anchors (CMOS-09-SEC-063). | PKI admin surface (§9) |

---

## Composition into UI roles (informative)

Roles are convenience bundles surfaced in the UI only; enforcement uses the keys, not
the bundle name (CMOS-09-SEC-012). Illustrative bundles:

- **Operator** — `devices.view`, `provision.devices`, `numbering.manage`,
  `routing.manage`, `carriers.manage`.
- **Security Admin** — `iam.manage`, `policy.manage`, `audit.view`, `secrets.manage`,
  `certs.manage`.
- **Billing Admin** — `billing.manage`, `billing.view`, `billing.export`.
- **Agent** — `calls.view`, `calls.control`, `voicemail.view`.

These bundles are non-normative examples; a deployment MAY define any bundle over the
canonical keys.

## Keys introduced by this volume (adopt across the suite)

The following keys are new in 0.3.0 and SHOULD be adopted by Volume 4 endpoint
declarations and any volume that gates the corresponding action:

- `secrets.manage` — secrets backend/handle administration.
- `certs.manage` — PKI/certificate administration.
- `calls.dial.international` — capability-gated international dialling class.

## Change log

- **0.3.0** — Initial canonical catalogue: all capability keys referenced by Volume 4
  endpoints enumerated with descriptions and API areas; introduced `secrets.manage`,
  `certs.manage`, and `calls.dial.international`; requirement IDs
  `CMOS-09-SEC-080…082`.
