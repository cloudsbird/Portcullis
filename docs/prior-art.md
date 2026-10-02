# Prior art — and why Portcullis exists

The "redact before the cloud LLM sees it" idea is not new. Here is the honest landscape.

| Project | What it is | Teach-in-chat? |
|---|---|---|
| **Microsoft Presidio** | OSS detect/anonymize/deanonymize SDK; `PatternRecognizer(deny_list=[…])` | ❌ config |
| **Presidio + LiteLLM** | A redaction proxy — an off-the-shelf config (`guardrail: presidio`) | ❌ config |
| **LLM Guard** (Protect AI) | `hidden_names` / `allowed_names` + `Vault` + `Deanonymize` | ❌ config · unmaintained |
| **larsderidder/contextio** | Transparent proxy; redact presets; reversible; custom rules + allowlist (JSON) | ❌ config |
| **daslabhq/pii-proxy** | JS proxy; layered regex→Ollama detectors; mask/unmask | ❌ config |
| **OpenGradient/veil**, **blidormf/veil-ai**, **helloveil/veil-phantom** | Local privacy proxies | ❌ |
| **Portkey / Cloudflare AI Gateway / Bedrock Guardrails / Google DLP** | Cloud gateways / DLP | ❌ (or cloud-side) |

**Every one is config-driven.** None offers a *user-taught, in-chat, persistent* learning
loop. That is the gap Portcullis targets.

## What Portcullis does *not* try to be

- **Not a new detection model.** It wraps existing ones (deterministic dictionary first,
  GLiNER2-PII behind an ONNX boundary at M1).
- **Not a new proxy framework.** The proxy is a thin surface over the core.
- **Not a cloud service.** The whole point is that the detector runs locally; a cloud
  "DLP proxy" just *relocates* the exposure.

## The honest differentiator

Two things together:

1. **A learned, local deny/allow store** — instant, deterministic, reversible, auditable,
   and it never leaves the machine. A privacy *guarantee* has to live in a deterministic
   layer; a probabilistic model cannot provide one.
2. **The four invariants** enforced as tests — so "it's private" is a property you can
   *verify*, not a claim.
