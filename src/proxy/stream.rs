//! Placeholder-level carry buffer shared by both SSE paths.

use super::*;

// ---------------------------------------------------------------------------
// Placeholder-level carry buffer for the SSE path
// ---------------------------------------------------------------------------

/// Upper bound on how much of a suspected partial placeholder we hold back.
/// Guarantees a malformed stream can never grow the buffer without bound.
pub(super) const MAX_PENDING: usize = 256;

/// Per-field carry buffers, so a partial placeholder in `content` and one in
/// tool-call `arguments` cannot corrupt each other.
#[derive(Default)]
pub(super) struct StreamCarry {
    pub(super) content: String,
    pub(super) args: String,
    /// Reasoning/thinking keeps its own buffer. Two text fields interleave in one
    /// stream, so stitching a split placeholder across them would corrupt both.
    pub(super) reasoning: String,
}

/// Split `combined` into (safe to emit, holds back a partial placeholder).
///
/// A placeholder looks like `<<LABEL_N>>`. If the text ends with a `<<` that
/// has no closing `>>` after it, everything from that `<<` onward is withheld
/// until a later event completes it.
pub(super) fn split_partial_placeholder(combined: &str) -> (&str, &str) {
    if let Some(open) = combined.rfind("<<") {
        if !combined[open + 2..].contains(">>") {
            return (&combined[..open], &combined[open..]);
        }
    }
    (combined, "")
}

/// Rehydrate `text`, prepending and updating the carry buffer so a placeholder
/// split across events is restored as a single unit.
pub(super) fn rehydrate_with_carry(text: &str, pending: &mut String, vault: &Vault) -> String {
    let combined = format!("{pending}{text}");
    let (emit, held) = split_partial_placeholder(&combined);
    let (emit, held) = if held.len() > MAX_PENDING {
        // Give up holding rather than buffer unboundedly.
        (combined.as_str(), "")
    } else {
        (emit, held)
    };
    let emit_owned = emit.to_string();
    *pending = held.to_string();
    vault.restore(&emit_owned)
}
