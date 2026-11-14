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
    /// OpenRouter's `reasoning` delta. Separate from `reasoning_content` because a
    /// gateway may stream both side by side.
    pub(super) reasoning_alt: String,
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

/// Accumulates raw SSE bytes and yields whole events.
///
/// Bytes, not text: a network chunk can end in the middle of a multi-byte character,
/// and decoding each chunk on its own would turn both halves into U+FFFD. An event is
/// only decoded once its terminating blank line has arrived, so it is always whole.
#[derive(Default)]
pub(super) struct SseBuffer {
    bytes: Vec<u8>,
}

impl SseBuffer {
    pub(super) fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
    }

    /// The next complete event, including its blank-line terminator. Both `\n\n` and
    /// `\r\n\r\n` terminate an event.
    pub(super) fn next_event(&mut self) -> Option<String> {
        let find = |needle: &[u8]| self.bytes.windows(needle.len()).position(|w| w == needle);
        let end = match (
            find(b"\n\n").map(|i| i + 2),
            find(b"\r\n\r\n").map(|i| i + 4),
        ) {
            (Some(a), Some(b)) => a.min(b),
            (a, b) => a.or(b)?,
        };
        let rest = self.bytes.split_off(end);
        let event = std::mem::replace(&mut self.bytes, rest);
        Some(String::from_utf8_lossy(&event).into_owned())
    }

    /// Whatever is left at end of stream: an unterminated final event.
    pub(super) fn take_rest(&mut self) -> Option<String> {
        if self.bytes.is_empty() {
            return None;
        }
        Some(String::from_utf8_lossy(&std::mem::take(&mut self.bytes)).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_character_split_across_chunks_survives() {
        let event = "data: héllo 日本 😀\n\n".as_bytes();
        // Feed one byte at a time: every multi-byte character is split.
        let mut buf = SseBuffer::default();
        let mut got = None;
        for b in event {
            buf.push(&[*b]);
            if let Some(e) = buf.next_event() {
                got = Some(e);
            }
        }
        assert_eq!(got.as_deref(), Some("data: héllo 日本 😀\n\n"));
    }

    #[test]
    fn crlf_terminated_events_are_split() {
        let mut buf = SseBuffer::default();
        buf.push(b"data: a\r\n\r\ndata: b\n\ndata: c");
        assert_eq!(buf.next_event().as_deref(), Some("data: a\r\n\r\n"));
        assert_eq!(buf.next_event().as_deref(), Some("data: b\n\n"));
        assert_eq!(buf.next_event(), None);
        assert_eq!(buf.take_rest().as_deref(), Some("data: c"));
        assert_eq!(buf.take_rest(), None);
    }
}
