# Encrypting the store

The learned store is a map of everything you consider private. By default it is plaintext
JSON, written `0600`. Set a key and it is encrypted on disk instead.

## Turn it on

```bash
# a long random passphrase, in a file only you can read
openssl rand -base64 32 > /etc/portcullis.key && chmod 600 /etc/portcullis.key
export PORTCULLIS_STORE_KEY_FILE=/etc/portcullis.key     # or PORTCULLIS_STORE_KEY=…

portcullis --store store.json encrypt-store              # encrypts the existing file in place
portcullis --store store.json serve                      # now reads and writes it encrypted
```

Setting the key alone is enough for a new store, and for an old one the next save encrypts
it; `encrypt-store` just does it now. `PORTCULLIS_STORE_KEY` and `PORTCULLIS_STORE_KEY_FILE`
are mutually exclusive. Prefer the file: an environment variable is visible to anyone who
can read the process's environment, and ends up in `docker inspect` and shell history.

For systemd put `PORTCULLIS_STORE_KEY_FILE=…` in `/etc/portcullis.env`; for Docker mount the
file as a secret and point the variable at it.

## What the file looks like

```json
{"portcullis_store":1,"kdf":"argon2id","m_kib":65536,"t":3,"p":1,
 "salt":"…","nonce":"…","ciphertext":"…"}
```

**Argon2id** (64 MiB, 3 passes) stretches the passphrase into a 256-bit key; **AES-256-GCM**
encrypts and authenticates the store. A fresh random salt and nonce are drawn on every save.
Everything except the ciphertext — format version, KDF and its cost, salt, nonce — is bound
as associated data, so editing the file to weaken the key derivation fails authentication
rather than succeeding quietly. The KDF cost recorded in a file is capped on load, so a
tampered file cannot demand gigabytes of memory.

## Failure behaviour

| Situation | What happens |
|---|---|
| Encrypted store, no key | startup fails: *"is encrypted: set PORTCULLIS_STORE_KEY…"* |
| Wrong key, or a damaged file | startup fails: *"wrong key, or the file is corrupted"* (the two are indistinguishable by design) |
| Plaintext store, key set | loads fine; the next save encrypts it |

It never starts with an empty store in place of one it cannot read — the next `teach` would
overwrite the real one.

## Rotating or removing the key

```bash
PORTCULLIS_STORE_KEY_FILE=old.key portcullis --store store.json decrypt-store   # plaintext, 0600
PORTCULLIS_STORE_KEY_FILE=new.key portcullis --store store.json encrypt-store
```

`decrypt-store` writes **plaintext**; do the second step immediately, or leave it plaintext
to turn encryption off. There is no password recovery: lose the key and the store is gone.

## What this does and does not protect

Protects: the file at rest — a stolen disk or laptop, a backup, a stray `git add`, another
local user who can read the file but not your environment.

Does not protect:

- **A running process.** The decrypted store lives in memory while the proxy runs.
- **Anyone who can read the key**: the process environment, the key file, or root.
- **A weak passphrase.** Argon2 slows guessing; it cannot rescue `hunter2`. Anything under 16
  characters logs a warning.
- **The learned terms in transit to you** — `/terms` and `/suggestions` return them in clear
  over whatever transport you expose them on.
- **Passphrase memory hygiene.** The passphrase is not zeroed after use.
