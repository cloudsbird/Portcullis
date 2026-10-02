//! Property tests for the redaction round trip.
//!
//! The example-based tests pin known cases; these hunt for the ones nobody thought
//! of — odd Unicode, adjacent terms, mixed case, regex-shaped noise.

use portcullis::{Gateway, Store, Vault};
use proptest::prelude::*;

const TERMS: &[&str] = &["Cartalian", "Project Loki", "Zoë Müller", "straße-corp"];

/// Text built from taught terms (in random case), Unicode that changes length when
/// lowercased, regex-shaped noise, and filler. Never contains `<<`, which would be
/// indistinguishable from a placeholder on the way back.
fn arb_text() -> impl Strategy<Value = String> {
    let piece = prop_oneof![
        Just("Cartalian".to_string()),
        Just("CARTALIAN".to_string()),
        Just("cartalian".to_string()),
        Just("Project Loki".to_string()),
        Just("project loki".to_string()),
        Just("Zoë Müller".to_string()),
        Just("ZOË MÜLLER".to_string()),
        Just("STRASSE-CORP".to_string()),
        Just("straße-corp".to_string()),
        Just("İİİ".to_string()),
        Just("ǅ Σ ς".to_string()),
        Just("alice@example.com".to_string()),
        Just("+1 (415) 555-0132".to_string()),
        Just("4111 1111 1111 1111".to_string()),
        Just("192.168.10.4".to_string()),
        Just("2026-10-02".to_string()),
        "[ -;=?-~\u{a0}-\u{24f}\n]{0,8}",
    ];
    prop::collection::vec(piece, 0..12).prop_map(|v| v.join(" "))
}

fn gateway() -> Gateway {
    let mut store = Store::default();
    for t in TERMS {
        store.teach(t, "ORG", "global");
    }
    Gateway::new(store)
}

proptest! {
    /// No taught term (in any case) survives redaction, and `assert_clean` agrees.
    #[test]
    fn taught_terms_never_survive(text in arb_text()) {
        let mut gw = gateway();
        let out = gw.process(std::slice::from_ref(&text));
        let lower = out[0].to_lowercase();
        for t in TERMS {
            prop_assert!(!lower.contains(&t.to_lowercase()), "{t} survived in {:?}", out[0]);
        }
        prop_assert!(gw.assert_clean(&out).is_ok());
    }

    /// Restoring the redacted text gives back exactly what the client sent.
    #[test]
    fn redact_then_restore_is_identity(text in arb_text()) {
        let mut gw = gateway();
        let mut vault = Vault::new();
        let out = gw.process_with(&mut vault, std::slice::from_ref(&text));
        prop_assert_eq!(vault.restore(&out[0]), text);
    }

    /// The delta cache never changes the answer: a second pass over the same text,
    /// served from cache, is byte-identical to the first.
    #[test]
    fn cache_hit_equals_cache_miss(text in arb_text()) {
        let mut gw = gateway();
        let first = gw.process_with(&mut Vault::new(), std::slice::from_ref(&text));
        let second = gw.process_with(&mut Vault::new(), std::slice::from_ref(&text));
        prop_assert_eq!(first, second);
    }

    /// Redaction never panics on arbitrary text, whatever it contains.
    #[test]
    fn arbitrary_text_never_panics(text in "\\PC{0,200}") {
        let mut gw = gateway();
        let _ = gw.process(std::slice::from_ref(&text));
    }
}
