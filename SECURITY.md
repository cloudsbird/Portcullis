# Security Policy

Portcullis is a privacy gateway: it sits in the path of your prompts and holds a list of
everything you consider private. Security reports are taken seriously.

## Reporting a vulnerability

**Please do not open a public issue for a security problem.**

Use GitHub's private vulnerability reporting: go to the
[Security tab](https://github.com/cloudsbird/Portcullis/security) and choose
**Report a vulnerability**. That opens a private advisory visible only to the maintainer.

If you cannot use that, email
[leunardus.vederis714@gmail.com](mailto:leunardus.vederis714@gmail.com) with `SECURITY` in
the subject.

Please include, as far as you can:

- what you did, and what you expected instead
- a minimal reproduction — a config, a request, and the observation
- the version or commit you tested
- whether the issue is a **leak** (protected data reaching the provider) or a
  **denial of service / availability** problem

**Leak reports take priority over everything else.** If you believe raw data reached the
provider, say so first — that is the class of bug this project exists to prevent.

You can expect an acknowledgement within a few days. As a single-maintainer project there
is no formal SLA, but a confirmed leak will be treated as the top priority.

## Supported versions

The project is pre-1.0 and moves fast. Only the **latest commit on `main`** is supported:
fixes land there rather than being backported to tags.

## In scope

Reports that are genuinely useful, roughly in priority order:

1. **A protected value reaching the provider in raw form.** Any path where a string that
   should have been redacted is forwarded, including via streaming, tool calls, reasoning
   fields, multimodal parts, or a field the walkers do not cover.
2. **A placeholder leaking to the client un-rehydrated**, or a half-placeholder appearing
   when a value is split across stream events.
3. **Bypassing the fail-closed assertion** — any way to make a payload leave that the
   outbound check should have stopped.
4. **A stale-cache leak** — a redaction decision surviving a policy change it should not
   have (teaching, unteaching).
5. **Crashing or hanging the process** from a request: a panic in a request path, an
   unbounded resource, a stall that blocks other requests.
6. **Weaknesses in the store file handling** — permissions, path handling, symlink
   behaviour.

## Out of scope

These are known, documented limitations rather than vulnerabilities — see
[docs/threat-model.md](docs/threat-model.md) and
[docs/OPERATIONS.md](docs/OPERATIONS.md#before-this-guards-someone-elses-data):

- **Values the detector does not recognise.** Detection is best-effort; the guarantee
  comes from what you teach it, not from the model. A missed detection in novel text is
  expected behaviour, and `teach` is the remedy.
- **The store is not encrypted at rest.** Documented, and on the roadmap.
- **`scope` is not enforced.** Documented; every term currently applies globally.
- **The proxy endpoints are unauthenticated by design**, for loopback use. Exposing them
  without fronting authentication is a deployment decision, not a defect.
- **Metadata** — timing, request volume, and the fact that redaction is happening.
- Anything requiring an already-compromised host, or an attacker who can read the
  process environment or the store file directly.

## Hardening guidance

If you are deploying this, the [deployment guide](docs/DEPLOYMENT.md) and the
[operations guide](docs/OPERATIONS.md) cover the decisions that matter: keep it on
loopback, set `PORTCULLIS_ADMIN_TOKEN`, keep the store outside version control, and read
the readiness gaps before pointing anyone else's traffic at it.
