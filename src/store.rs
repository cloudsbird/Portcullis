use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

fn default_scope() -> String {
    "global".into()
}

/// One learned entity: a term to hide (deny) or to leave alone (allow).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub term: String,
    pub label: String,
    #[serde(default = "default_scope")]
    pub scope: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub source: String,
}

/// The learned entity store. Local-only. **Never synced.**
///
/// NOTE: persisted as **plaintext JSON** — there is no encryption at rest yet
/// (see docs/TEACHING.md, "What teaching does NOT do"). This file is the crown
/// jewels: a map of everything you consider private. It must never leave the
/// machine and must never be committed (see `.gitignore`).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub deny: Vec<Entry>,
    #[serde(default)]
    pub allow: Vec<Entry>,
}

impl Store {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Store::default());
        }
        Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
    }

    /// Persist the store.
    ///
    /// On Unix the file is created with `0600`. It is a map of everything you
    /// consider private, and there is no encryption at rest yet — so at minimum
    /// it must not be world-readable.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let raw = serde_json::to_string_pretty(self)?;

        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(path)?;
            file.write_all(raw.as_bytes())?;

            // `.create(true)` does not change the mode of a file that already
            // exists, so tighten a pre-existing store too.
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }

        #[cfg(not(unix))]
        std::fs::write(path, raw)?;

        Ok(())
    }

    /// Teach a term. Idempotent by (case-insensitive) term.
    pub fn teach(&mut self, term: &str, label: &str, scope: &str) {
        if let Some(e) = self
            .deny
            .iter_mut()
            .find(|e| e.term.eq_ignore_ascii_case(term))
        {
            e.label = label.to_string();
            return;
        }
        self.deny.push(Entry {
            term: term.to_string(),
            label: label.to_string(),
            scope: scope.to_string(),
            aliases: Vec::new(),
            source: "user".into(),
        });
    }

    /// Remove a taught term. Returns whether anything changed.
    pub fn unteach(&mut self, term: &str) -> bool {
        let before = self.deny.len();
        self.deny.retain(|e| !e.term.eq_ignore_ascii_case(term));
        before != self.deny.len()
    }

    pub fn is_allowed(&self, term: &str) -> bool {
        self.allow.iter().any(|e| e.term.eq_ignore_ascii_case(term))
    }

    /// Every surface form that must be hidden: terms plus their aliases.
    pub fn hidden_forms(&self) -> Vec<(String, String)> {
        let mut v = Vec::new();
        for e in &self.deny {
            v.push((e.term.clone(), e.label.clone()));
            for a in &e.aliases {
                v.push((a.clone(), e.label.clone()));
            }
        }
        v
    }
}
