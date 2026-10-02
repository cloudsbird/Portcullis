# What the provider actually sees

A real, unedited run. Reproduce with [`scripts/make_example.sh`](../scripts/make_example.sh).

**Scenario:** you're drafting a client follow-up. Your provider is your normal LLM
endpoint. You have taught Portcullis three terms: `Cartalian`, `Daniel Pratt`,
`Northwind Logistics`.

---

## What you send

**System prompt**

```
You are the assistant for Northwind Logistics. The account manager for Cartalian is Daniel Pratt.
```

**Your message**

```
Draft a follow-up to Daniel Pratt <daniel.pratt@northwind-logistics.com> about the Cartalian renewal. My direct line is +1 415 555 0132.
```

---

## What actually leaves your machine

This is the payload the provider's servers receive. Your client sent the first block;
the second is what went over the wire.

**System prompt as received by the model**

```
You are the assistant for <<ORG_1>>. The account manager for <<ORG_2>> is <<PERSON_1>>.
```

**Your message as received by the model**

```
Draft a follow-up to <<PERSON_1>> <<<EMAIL_1>>> about the <<ORG_1>> renewal. My direct line is <<PHONE_1>>.
```

> The `<<<EMAIL_1>>>` triple brackets are correct: the input was `<daniel.pratt@…>`, so
> the literal `<` `>` around the address survive, with the placeholder in between.

**What your client displays back to you** (the response is rehydrated locally)

```
Draft a follow-up to Daniel Pratt <daniel.pratt@northwind-logistics.com> about the Cartalian renewal. My direct line is +1 415 555 0132.
```

Your provider has a coherent task. It has never seen a client name, a colleague's name,
an email address, or a phone number.

---

## The learning loop

Now you mention something you have **not** taught it yet:

**You send**

```
Also mention that Project Loki kicks off next month.
```

**The model receives**

```
Also mention that <<FULL_NAME_1>> kicks off next month.
```

Notice two things:

1. **The model layer caught it anyway** — the ML detector generalised from the other
   names, so `Project Loki` did not leak. That is the recall the model buys you.
2. **The label is wrong.** It guessed `FULL_NAME`. The model is best-effort recall with
   imperfect labels.

So you teach it, in-chat:

```bash
curl -sX POST http://127.0.0.1:8080/teach \
  -H "Authorization: Bearer $PORTCULLIS_ADMIN_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"term":"Project Loki","label":"ORG"}'
```

**Send the identical message again** — the model now receives

```
Also mention that <<ORG_1>> kicks off next month.
```

The label is exact, the redaction is guaranteed by the deterministic dictionary layer
rather than guessed, and it took effect on the **very next request** — including on
messages the provider had already seen.

---

## Why the placeholders are stable

`<<ORG_1>>` is a pure function of the value, not a counter over the conversation. The
same value always renders as the same placeholder, so:

- the model stays coherent across turns (it can refer back to `<<PERSON_1>>`), and
- the **provider's prompt cache is preserved**, because untouched segments are re-emitted
  byte-identically.

## The three layers, and what each is worth

| Layer | Catches | Guarantee |
|---|---|---|
| **Dictionary** (taught terms) | exactly what you taught | **exact** — this is the guarantee |
| **Regex** | email, phone, card-like, IP, API keys | exact for known shapes |
| **ONNX model** | names, orgs, addresses it was never told about | best-effort recall; labels can be wrong |

Teach the things that matter. The model is there for the rest.
