//! The four safety invariants, as tests that fail if violated.
//!
//! These are the contract. If any of these go red, the gateway can leak.

use portcullis::{Entry, Gateway, Store};

fn gw_with(terms: &[(&str, &str)]) -> Gateway {
    let mut s = Store::default();
    for (t, l) in terms {
        s.teach(t, l, "global");
    }
    Gateway::new(s)
}

/// INVARIANT 1 + 2: no raw PII is ever echoed; every segment is scanned.
#[test]
fn outbound_never_contains_raw_pii() {
    let mut gw = gw_with(&[("Cartalian", "ORG")]);
    let segs = vec!["Email daniel.pratt@northwind-logistics.com about Cartalian.".to_string()];
    let out = gw.process(&segs);
    assert!(
        !out[0].contains("daniel.pratt@northwind-logistics.com"),
        "{}",
        out[0]
    );
    assert!(!out[0].contains("Cartalian"), "{}", out[0]);
    assert!(gw.assert_clean(&out).is_ok());
}

/// A segment re-sent on a later turn is re-emitted REDACTED from cache, never raw.
#[test]
fn repeat_segment_reemits_redacted_not_raw() {
    let mut gw = gw_with(&[("Cartalian", "ORG")]);
    let seg = "Cartalian is our client.".to_string();
    let _ = gw.process(std::slice::from_ref(&seg)); // turn 1: scanned
    let out = gw.process(std::slice::from_ref(&seg)); // turn 2: cache hit
    assert!(
        !out[0].contains("Cartalian"),
        "cache echoed raw: {}",
        out[0]
    );
}

/// INVARIANT 3: teaching mid-session invalidates the cache — no stale-redaction leak.
#[test]
fn teach_invalidates_cache() {
    let mut gw = gw_with(&[]);
    let seg = "Email daniel.pratt@northwind-logistics.com about Project Loki.".to_string();

    let t1 = gw.process(std::slice::from_ref(&seg));
    assert!(
        t1[0].contains("Project Loki"),
        "not taught yet, should be visible"
    );

    gw.teach("Project Loki", "ORG", "global"); // policy change

    let t2 = gw.process(std::slice::from_ref(&seg));
    assert!(
        !t2[0].contains("Project Loki"),
        "stale cache leaked the newly-taught term: {}",
        t2[0]
    );
}

/// INVARIANT 4: the outbound assertion is fail-closed.
#[test]
fn assertion_is_fail_closed() {
    let gw = gw_with(&[("Cartalian", "ORG")]);
    let leaky = vec!["Cartalian is our client.".to_string()];
    assert!(gw.assert_clean(&leaky).is_err());
}

/// Allow-list wins: an allowed term is neither redacted nor asserted against.
#[test]
fn allow_list_wins() {
    let mut s = Store::default();
    s.teach("Adit", "PERSON", "global");
    s.allow.push(Entry {
        term: "Adit".into(),
        label: "PERSON".into(),
        scope: "global".into(),
        aliases: vec![],
        source: "user".into(),
        whole_word: false,
    });
    let mut gw = Gateway::new(s);
    let out = gw.process(&["Adit pinged us.".to_string()]);
    assert!(
        out[0].contains("Adit"),
        "allow-listed term was redacted: {}",
        out[0]
    );
    assert!(
        gw.assert_clean(&out).is_ok(),
        "assertion flagged an allowed term"
    );
}

/// A model reply that echoes a placeholder is restored locally.
#[test]
fn rehydrate_round_trip() {
    let mut gw = gw_with(&[("Cartalian", "ORG")]);
    let out = gw.process(&["Cartalian is our client.".to_string()]);
    let restored = gw.rehydrate(&out[0]);
    assert_eq!(restored, "Cartalian is our client.");
}

/// Offsets must come from the original text. `İ` lowercases to a *longer* byte
/// sequence, so reusing offsets from a lowercased copy slices mid-character
/// (panic) or lands on the wrong bytes (leak).
#[test]
fn unicode_case_mapping_cannot_shift_spans() {
    let mut gw = gw_with(&[("Cartalian", "ORG")]);
    let text = "İİİİİ met Cartalian, then CARTALIAN again.".to_string();
    let out = gw.process(std::slice::from_ref(&text));
    assert!(!out[0].to_lowercase().contains("cartalian"), "{}", out[0]);
    assert!(out[0].starts_with("İİİİİ met "), "{}", out[0]);
    assert!(gw.assert_clean(&out).is_ok());
}

#[test]
fn substring_matching_is_the_safe_default() {
    let mut gw = gw_with(&[("Ann", "PERSON")]);
    let out = gw.process(&["Anna and Ann.".to_string()]);
    assert!(
        !out[0].contains("Ann"),
        "default must over-redact: {}",
        out[0]
    );
}

#[test]
fn whole_word_terms_leave_longer_words_alone() {
    let mut s = Store::default();
    s.teach_with("Ann", "PERSON", "global", true);
    let mut gw = Gateway::new(s);
    let out = gw.process(&["Anna wrote to Ann, not channel.".to_string()]);
    assert!(out[0].contains("Anna"), "{}", out[0]);
    assert!(out[0].contains("channel"), "{}", out[0]);
    assert!(!out[0].contains("to Ann,"), "{}", out[0]);
    // The outbound assertion honours the same boundaries (no false alarm on "Anna").
    assert!(gw.assert_clean(&out).is_ok());
    assert!(gw.assert_clean(&["Ann is here".to_string()]).is_err());
}

#[test]
fn longest_form_wins_at_the_same_position() {
    let mut gw = gw_with(&[("Project", "ORG"), ("Project Loki", "PROJECT")]);
    let out = gw.process(&["About Project Loki today.".to_string()]);
    assert!(out[0].contains("<<PROJECT_1>>"), "{}", out[0]);
}

/// The compiled-dictionary cache must never serve a stale list: `Gateway::store` is a
/// public field, and a term added by editing it directly has to take effect at once.
#[test]
fn directly_edited_store_is_never_served_from_a_stale_dictionary() {
    let mut gw = gw_with(&[("Cartalian", "ORG")]);
    let first = gw.process(&["Loki and Cartalian".to_string()]);
    assert!(first[0].contains("Loki"), "{}", first[0]);

    gw.store.teach("Loki", "ORG", "global"); // bypasses Gateway::teach on purpose
    let leaky = vec!["Loki is here".to_string()];
    assert!(
        gw.assert_clean(&leaky).is_err(),
        "assert_clean used a stale dictionary"
    );
    let mut fresh = gw_with(&[("Cartalian", "ORG"), ("Loki", "ORG")]);
    assert!(!fresh.process(&leaky)[0].contains("Loki"));
}
