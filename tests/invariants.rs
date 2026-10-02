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
