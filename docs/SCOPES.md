# Scopes: per-client isolation

One Portcullis can serve several clients — teams, projects, customers — without their
taught terms mixing. This is **opt-in**: with nothing configured it behaves exactly as
before (one tenant, every term applies to every request).

## How it works

Give each client a token:

```bash
export PORTCULLIS_SCOPE_TOKENS='alpha=tok-3f9c...,beta=tok-81ab...'
# or keep them out of the environment:
export PORTCULLIS_SCOPE_TOKENS_FILE=/etc/portcullis/scopes.json   # {"alpha": "tok-…", …}
```

Each client sends its token the way it already sends an API key — `Authorization: Bearer
<token>` (OpenAI-style) or `x-api-key: <token>` (Anthropic-style). Point the client's
"API key" setting at the scope token; Portcullis substitutes the real provider key
upstream, so **the client never holds your provider key**.

Once any token is configured:

- Every request must present a valid token. Missing or wrong → `401`, **before** the body is
  read or anything is forwarded.
- A request in scope `alpha` is redacted with the terms taught for `global` and for
  `alpha`. Terms taught for `beta` do not apply to it.
- The token *proves* the scope. A client cannot name another client's scope.

Teach into a scope with the `scope` field (admin token required):

```bash
curl -X POST http://127.0.0.1:8080/teach \
  -H "Authorization: Bearer $PORTCULLIS_ADMIN_TOKEN" -H 'content-type: application/json' \
  -d '{"term":"Acme","label":"ORG","scope":"alpha"}'
```

or from the CLI: `portcullis teach Acme --label ORG --scope alpha`. A scope that is neither
`global` nor configured is rejected (`400`), because it would protect nobody and never say so.
The same word can be taught for several scopes; each is its own entry. `unteach` takes an
optional `scope` and otherwise removes the term everywhere. `GET /suggestions?scope=alpha`
lists only what was seen in alpha's traffic.

Startup fails — rather than quietly weakening isolation — if a scope is named `global`,
listed twice, shares a token with another scope, reuses `PORTCULLIS_ADMIN_TOKEN`, or if
`PORTCULLIS_FORWARD_HEADERS` would pass the scope token on to the provider.

## What is isolated

| | Isolated per scope? |
|---|---|
| Which terms redact a request | ✅ |
| The delta cache (keyed by scope, so one client never gets another's cached redaction) | ✅ |
| Placeholders and rehydration (a vault per request) | ✅ |
| Auto-suggestions | ✅ tagged by scope, filterable |

## What is not

- **It is not a defence against the operator.** The admin token sees every scope's terms
  (`/terms`) and every suggestion. The store file holds all scopes in one plaintext file
  (see M6 for encryption).
- **Scoping selects which terms apply; it does not stop a client sending its own secrets.**
  Alpha's text containing a term only *beta* taught is forwarded as-is — that term was never
  alpha's to protect. Teach shared secrets as `global`.
- **Everything downstream is shared**: one provider key and quota, one detector model, one
  log stream (counts and timings only — never values).
- Detection still serialises behind one lock, so one noisy tenant slows the others.

Demonstrated end to end, with a mock provider, in `tests/scopes.rs`.
