//! Portcullis — a local-first, teachable PII gateway.
//!
//! The cloud model should see the **task**, never the *identities*. Portcullis sits
//! between your agent and the cloud LLM: it strips private details out of the prompt
//! before it leaves, and restores them in the answer that comes home.
//!
//! Design invariants (enforced by `tests/invariants.rs`):
//!
//! 1. **Never echo a raw segment** — outbound bytes are either freshly redacted or the
//!    cached *redacted* form of a segment seen before. Raw text is never echoed.
//! 2. **No bypass** — every outbound segment was scanned: now, or its exact bytes earlier.
//! 3. **Invalidate on policy change** — `teach`/`unteach` clears the delta cache, so a
//!    term taught mid-session cannot be leaked by a stale cached redaction.
//! 4. **Fail closed** — the assembled payload is asserted clean before it leaves.

pub mod detect;
#[cfg(feature = "onnx")]
pub mod detect_onnx;
pub mod model;
pub mod pipeline;
pub mod proxy;
pub mod store;
pub mod vault;

pub use model::DetectorSettings;
pub use pipeline::Gateway;
pub use store::{Entry, Store};
pub use vault::Vault;
