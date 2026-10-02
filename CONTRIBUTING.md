# Contributing

Thanks for considering it. This is a small, opinionated project, so this guide is short
and mostly about expectations rather than process.

## Ways to contribute

- **Bug reports.** Especially leak reports — see [SECURITY.md](SECURITY.md) for those. For
  ordinary bugs, an issue with a reproduction is worth more than a patch without one.
- **Detection accuracy.** Better label sets for a domain, or a well-argued change to the
  deterministic regex layer. Bring a measurement, not an opinion; `benchmarks/` shows how.
- **Client coverage.** A verified setup snippet for a client that is not in
  [docs/CLIENTS.md](docs/CLIENTS.md) is genuinely useful. Note that claims need to be
  reproduced, not assumed.
- **Documentation.** Corrections, clarifications, and anything that made you stop and
  re-read.

If you are planning anything substantial, **open an issue first** so the approach can be
agreed before you write code. It is much easier to change a plan than a patch.

## Development setup

```bash
git clone https://github.com/cloudsbird/Portcullis
cd Portcullis

# build-time only, needed to compile the ONNX bindings
sudo apt-get install -y pkg-config libssl-dev

cargo build                       # no ML layer
cargo build --features onnx       # with the detector
```

The detector model is 1.2 GB and is not in the repository. Export it with
[`lmoe/gliner2-onnx`](https://github.com/lmoe/gliner2-onnx):

```bash
git clone https://github.com/lmoe/gliner2-onnx && cd gliner2-onnx
make onnx-export MODEL=fastino/gliner2-privacy-filter-PII-multi
```

Then point `PORTCULLIS_MODEL_DIR` at `model_out/gliner2-privacy-filter-PII-multi`.
Most of the test suite does not need it.

## Running the tests

```bash
cargo test                                                  # 40 tests, no model required
cargo build --features onnx
PORTCULLIS_MODEL_DIR=./model cargo test --features onnx     # 41, includes the golden test
```

`cargo test` must be green in **both** configurations before a patch is mergeable. CI runs
both on every push and pull request, and additionally asserts that `ort` stays
feature-gated — the default build must not acquire ML dependencies.

The optional live end-to-end run against a real provider is described in
[docs/OPERATIONS.md](docs/OPERATIONS.md#live-end-to-end-test). It costs a fraction of a
cent and is run manually, never in CI.

### Documentation checks

```bash
python3 scripts/linkcheck.py
```

Verifies that every relative link resolves, every `#anchor` matches a real heading (using
GitHub's slug rules), and every external URL answers. Standard library only, so it adds
nothing to the toolchain. Run it if your change touches documentation.

## Conventions

Two of them are load-bearing:

**1. A behaviour change means a test change.**

[`tests/invariants.rs`](tests/invariants.rs) is the safety contract — never echo a raw
segment, no bypass, invalidate on policy change, fail closed. Those four are the reason
this project can be trusted at all. A patch that weakens them will not be merged, however
convenient it is. If you believe an invariant is wrong, open an issue and argue it; do not
edit around it.

**2. Claims need evidence.**

The documentation makes specific, falsifiable claims — a benchmark score, a memory figure,
a latency. Each one is backed by a script that reproduces it:

- `benchmarks/pii_masking/` — the PIIMB run and its committed raw output
- `docs/RESOURCES.md` with `scripts/measure.py`, `scripts/measure_server.sh`
- `docs/EXAMPLE.md` with `scripts/make_example.sh`
- `scripts/e2e_live.sh` — the live provider run

If you add a number, add the means to check it. If you cannot measure something, say so in
the prose rather than estimating it silently.

Beyond those: keep commits focused, write commit messages that explain *why* rather than
*what* (the diff already says what), and match the surrounding style. Run `cargo fmt` and
`cargo clippy` if you have them.

## Pull requests

- Branch from `main`, and keep the PR to one concern.
- State what you changed and how you verified it. "Tests pass" is fine if they do; say
  which ones you ran.
- If the change is user-visible, update the relevant document under `docs/` in the same
  PR. Documentation kept in a separate commit tends not to happen.
- Be prepared for review to focus on failure modes rather than the happy path. For a tool
  in the privacy path, "what happens when this is wrong" matters more than "does it work".

## Code of conduct

Be straightforward and kind. Critique the change, not the person. Assume good faith — most
disagreements here have been about tradeoffs rather than facts.

## License

By contributing, you agree that your contributions are licensed under the
[Apache-2.0 license](LICENSE) that covers the project.
