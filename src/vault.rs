use std::collections::HashMap;

use regex::RegexBuilder;

/// A session vault: maps real values to stable, deterministic placeholders, and back.
///
/// Determinism matters twice over:
///   * the model must see the *same* placeholder for the same value across turns, and
///   * a *stable* prompt prefix preserves the provider's prompt cache (cost/latency).
#[derive(Default)]
pub struct Vault {
    real_to_ph: HashMap<String, String>,
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
        let ph = format!("<<{label}_{n}>>");
        self.real_to_ph.insert(real.to_string(), ph.clone());
        self.ph_to_real.insert(ph.clone(), real.to_string());
        ph
    }

    /// Restore placeholders, tolerating whitespace and case variation.
    ///
    /// The provider may reformat `<<EMAIL_1>>`; we match `<<  email_1  >>` too.
    pub fn restore(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (ph, real) in &self.ph_to_real {
            let inner = ph.trim_start_matches('<').trim_end_matches('>');
            let re = RegexBuilder::new(&format!(r"<<\s*{}\s*>>", regex::escape(inner)))
                .case_insensitive(true)
                .build()
                .expect("placeholder regex is valid");
            out = re.replace_all(&out, real.as_str()).into_owned();
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.ph_to_real.is_empty()
    }

    pub fn len(&self) -> usize {
        self.ph_to_real.len()
    }
}
