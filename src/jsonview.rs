//! A JSON-unescaped view of text, with a map back to the original bytes.
//!
//! Agents send a lot of JSON *as text*: Hermes returns every tool result as
//! `json.dumps({"output": ...})`, and tool-call arguments are JSON strings. In that text
//! a newline is the two characters `\n`, a tab is `\t`, and a non-ASCII letter may be
//! `\uXXXX`. Matching on the raw text goes wrong in three ways:
//!
//! * the escape letter joins the next word, so `\nAnn` has no word boundary before
//!   `Ann` and a whole-word taught term at the start of a line is never found (a leak);
//! * an email at the start of a line is read as `njane@acme.io`, so the wrong value is
//!   stored and later restored, and redacting it leaves an invalid escape `\<<EMAIL_1>>`;
//! * an escaped letter (`Jos` + `\u` + `00e9`) never equals the taught `José`.
//!
//! Detection therefore also runs over a decoded view. Spans found there are mapped back
//! to raw byte ranges that cover whole escape sequences, so replacing them with a
//! placeholder (which needs no escaping) keeps the JSON valid.

use std::borrow::Cow;

/// How many layers of escaping are decoded. A tool that prints JSON (`cat
/// package.json`) inside a JSON tool result gives two; more is rare.
pub const MAX_DEPTH: u8 = 2;

/// Text with JSON string escapes decoded, and a map from its offsets to raw offsets.
pub struct View {
    pub text: String,
    /// `map[i]` is the raw offset of view offset `i`. `map.len() == text.len() + 1`.
    /// Bytes inside a multi-byte character map to the start of its source sequence.
    map: Vec<usize>,
    /// Layers of escaping decoded to get here (1 or 2).
    pub depth: u8,
}

impl View {
    /// The decoded view of `raw`, or `None` when it has no escape to decode (then the
    /// raw text is the view, and nothing needs mapping).
    pub fn of(raw: &str) -> Option<View> {
        let (mut text, mut map) = decode_layer(raw)?;
        let mut depth = 1;
        while depth < MAX_DEPTH {
            let Some((next, inner)) = decode_layer(&text) else {
                break;
            };
            // Compose: view offset -> previous view offset -> raw offset.
            map = inner.into_iter().map(|i| map[i]).collect();
            text = next;
            depth += 1;
        }
        Some(View { text, map, depth })
    }

    /// The raw byte range behind view range `start..end`. Both ends must be char
    /// boundaries of the view; the result then spans whole escape sequences.
    pub fn to_raw(&self, start: usize, end: usize) -> (usize, usize) {
        (self.map[start], self.map[end])
    }
}

/// Decode one layer of JSON escapes in `raw`. Invalid escapes and lone surrogates are
/// kept verbatim.
pub fn unescape(raw: &str) -> Cow<'_, str> {
    match decode_layer(raw) {
        Some((text, _)) => Cow::Owned(text),
        None => Cow::Borrowed(raw),
    }
}

/// [`unescape`] applied `depth` times.
pub fn unescape_n(raw: &str, depth: u8) -> Cow<'_, str> {
    let mut out = Cow::Borrowed(raw);
    for _ in 0..depth {
        match decode_layer(&out) {
            Some((text, _)) => out = Cow::Owned(text),
            None => break,
        }
    }
    out
}

/// Escape `text` for use inside a JSON string literal (without the quotes).
pub fn escape(text: &str) -> String {
    let quoted = serde_json::to_string(text).expect("a str always serializes");
    quoted[1..quoted.len() - 1].to_string()
}

fn hex4(b: &[u8]) -> Option<u32> {
    if b.len() < 4 {
        return None;
    }
    let s = std::str::from_utf8(&b[..4]).ok()?;
    if !s.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(s, 16).ok()
}

/// One escape sequence at the start of `b` (which begins with a backslash): the
/// character it stands for and the number of raw bytes it uses.
fn escape_at(b: &[u8]) -> Option<(char, usize)> {
    let simple = match *b.get(1)? {
        b'"' => '"',
        b'\\' => '\\',
        b'/' => '/',
        b'b' => '\u{8}',
        b'f' => '\u{c}',
        b'n' => '\n',
        b'r' => '\r',
        b't' => '\t',
        b'u' => {
            let hi = hex4(&b[2..])?;
            if (0xD800..0xDC00).contains(&hi) {
                // A high surrogate only means something followed by a low one.
                if b.get(6) == Some(&b'\\') && b.get(7) == Some(&b'u') {
                    let lo = hex4(&b[8..])?;
                    if (0xDC00..0xE000).contains(&lo) {
                        let c = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                        return char::from_u32(c).map(|c| (c, 12));
                    }
                }
                return None;
            }
            return char::from_u32(hi).map(|c| (c, 6));
        }
        _ => return None,
    };
    Some((simple, 2))
}

/// Decode one layer, or `None` if nothing in `raw` decodes.
fn decode_layer(raw: &str) -> Option<(String, Vec<usize>)> {
    let bytes = raw.as_bytes();
    // Fast path: most text has no backslash at all.
    let first = bytes.iter().position(|&b| b == b'\\')?;
    let mut text = String::with_capacity(raw.len());
    let mut map = Vec::with_capacity(raw.len() + 1);
    text.push_str(&raw[..first]);
    map.extend(0..first);
    let mut i = first;
    let mut decoded_any = false;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            if let Some((c, used)) = escape_at(&bytes[i..]) {
                for _ in 0..c.len_utf8() {
                    map.push(i);
                }
                text.push(c);
                i += used;
                decoded_any = true;
                continue;
            }
        }
        // An ordinary character (or a backslash that starts no valid escape).
        let c = raw[i..].chars().next().expect("i is a char boundary");
        for k in 0..c.len_utf8() {
            map.push(i + k);
        }
        text.push(c);
        i += c.len_utf8();
    }
    map.push(raw.len());
    decoded_any.then_some((text, map))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BS: char = '\\';

    #[test]
    fn plain_text_has_no_view() {
        assert!(View::of("hello Ann").is_none());
        assert!(View::of(r"C:\Program Files\x").is_none(), "no valid escape");
    }

    #[test]
    fn spans_map_back_over_whole_escape_sequences() {
        let raw = format!("owner:{BS}nAnn{BS}tjane@acme.io");
        let v = View::of(&raw).unwrap();
        assert_eq!(v.text, "owner:\nAnn\tjane@acme.io");
        let at = v.text.find("Ann").unwrap();
        let (a, b) = v.to_raw(at, at + 3);
        assert_eq!(&raw[a..b], "Ann");
        let at = v.text.find("jane").unwrap();
        let (a, b) = v.to_raw(at, v.text.len());
        assert_eq!(&raw[a..b], "jane@acme.io");
    }

    #[test]
    fn unicode_escapes_and_surrogate_pairs_decode() {
        let raw = format!("Jos{BS}u00e9 {BS}ud83d{BS}ude00!");
        let v = View::of(&raw).unwrap();
        assert_eq!(v.text, "José 😀!");
        let (a, b) = v.to_raw(0, "José".len());
        assert_eq!(&raw[a..b], format!("Jos{BS}u00e9"));
        assert_eq!(unescape(&raw[a..b]), "José");
        // A lone surrogate cannot become a char; it is kept as written.
        let lone = format!("x{BS}ud83dy{BS}n");
        assert_eq!(View::of(&lone).unwrap().text, format!("x{BS}ud83dy\n"));
    }

    #[test]
    fn two_layers_decode_and_compose() {
        // JSON printed by a tool, inside a JSON tool result: `\\n` -> `\n` -> newline.
        let raw = format!("a{BS}{BS}nAnn");
        let v = View::of(&raw).unwrap();
        assert_eq!(v.depth, 2);
        assert_eq!(v.text, "a\nAnn");
        let (a, b) = v.to_raw(2, 5);
        assert_eq!(&raw[a..b], "Ann");
        assert_eq!(unescape_n(&format!("{BS}{BS}n"), 2), "\n");
    }

    #[test]
    fn escape_round_trips_through_json() {
        let s = "Dana \"DJ\" Smith\nC:\\x\t😀";
        let quoted = format!("\"{}\"", escape(s));
        assert_eq!(serde_json::from_str::<String>(&quoted).unwrap(), s);
    }
}
