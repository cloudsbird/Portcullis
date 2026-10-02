use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;

/// Any `<<...>>` token. Which ones are real placeholders is decided by lookup.
fn token_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"<<([^<>]*)>>").expect("placeholder token regex is valid"))
}

/// A vault: maps real values to stable, deterministic placeholders, and back.
///
/// **Scope one vault to one conversation (the proxy uses one per request).**
/// Placeholders such as `<<EMAIL_1>>` are only unique within a vault, so a vault
/// shared between clients would restore one client's values into another
/// client's response.
///
/// Determinism matters twice over:
///   * the model must see the *same* placeholder for the same value across turns, and
///   * a *stable* prompt prefix preserves the provider's prompt cache (cost/latency).
///
/// Both hold per request, because clients resend the whole history each turn and
/// placeholders are numbered in order of first appearance.
#[derive(Default)]
pub struct Vault {
    real_to_ph: HashMap<String, String>,
    /// Keyed by the placeholder's inner text, upper-cased: `EMAIL_1`.
    ph_to_real: HashMap<String, String>,
    counters: HashMap<String, usize>,
}

impl Vault {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the stable placeholder for `real`, minting one on first sight.
    pub fn placeholder_for(&mut self, label: &str, real: &str) -> String {
        if let Some(p) = self.real_to_ph.get(real) {
            return p.clone();
        }
        let label = label.to_ascii_uppercase();
        let n = self.counters.entry(label.clone()).or_insert(0);
        *n += 1;
        let inner = format!("{label}_{n}");
        let ph = format!("<<{inner}>>");
        self.real_to_ph.insert(real.to_string(), ph.clone());
        self.ph_to_real.insert(inner, real.to_string());
        ph
    }

    /// Restore placeholders, tolerating whitespace and case variation.
    ///
    /// The provider may reformat `<<EMAIL_1>>`; we match `<<  email_1  >>` too.
    /// One pass over the text, whatever the number of placeholders.
    pub fn restore(&self, text: &str) -> String {
        if self.ph_to_real.is_empty() {
            return text.to_string();
        }
        token_regex()
            .replace_all(text, |caps: &regex::Captures| {
                let inner = caps[1].trim().to_uppercase();
                match self.ph_to_real.get(&inner) {
                    Some(real) => real.clone(),
                    None => caps[0].to_string(),
                }
            })
            .into_owned()
    }

    pub fn is_empty(&self) -> bool {
        self.ph_to_real.is_empty()
    }

    pub fn len(&self) -> usize {
        self.ph_to_real.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restores_tolerantly_in_one_pass() {
        let mut v = Vault::new();
        let a = v.placeholder_for("email", "a@example.com");
        let b = v.placeholder_for("org", "Cartalian");
        assert_eq!(a, "<<EMAIL_1>>");
        assert_eq!(v.placeholder_for("email", "a@example.com"), a, "stable");

        let text = "Write to << email_1 >> at <<ORG_1>>; unknown <<ORG_9>> stays.";
        assert_eq!(
            v.restore(text),
            "Write to a@example.com at Cartalian; unknown <<ORG_9>> stays."
        );
        assert_eq!(b, "<<ORG_1>>");
    }

    #[test]
    fn restored_values_are_not_rescanned() {
        let mut v = Vault::new();
        v.placeholder_for("org", "<<EMAIL_1>>");
        v.placeholder_for("email", "x@example.com");
        // The real value of ORG_1 looks like a placeholder; it must come back verbatim.
        assert_eq!(v.restore("<<ORG_1>>"), "<<EMAIL_1>>");
    }

    #[test]
    fn separate_vaults_do_not_share_values() {
        let mut a = Vault::new();
        let b = Vault::new();
        let ph = a.placeholder_for("email", "alice@example.com");
        assert_eq!(
            b.restore(&ph),
            ph,
            "another session's vault must not restore it"
        );
    }
}
