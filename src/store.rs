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
    /// Match only at word boundaries, so teaching "Ann" leaves "Anna" alone.
    /// Off by default: for a privacy tool, over-redacting beats leaking.
    #[serde(default)]
    pub whole_word: bool,
}

/// One surface form that must be hidden, as the dictionary detector sees it.
#[derive(Debug, Clone)]
pub struct HiddenForm {
    pub form: String,
    pub label: String,
    pub whole_word: bool,
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

    /// Persist the store, atomically.
    ///
    /// The JSON goes to a sibling temp file which is flushed to disk and then
    /// renamed over the target, so a crash mid-write leaves the previous store
    /// intact instead of a truncated one. On Unix the file is created `0600`: it
    /// is a map of everything you consider private, and there is no encryption at
    /// rest yet — so at minimum it must not be world-readable.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        use std::io::Write;

        let path = path.as_ref();
        let raw = serde_json::to_string_pretty(self)?;

        let mut tmp_name = path
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_else(|| "store".into());
        tmp_name.push(format!(".tmp-{}", std::process::id()));
        let tmp = path.with_file_name(tmp_name);

        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }

        let write = || -> Result<()> {
            let mut file = options.open(&tmp)?;
            file.write_all(raw.as_bytes())?;
            file.sync_all()?;
            Ok(())
        };
        if let Err(e) = write().and_then(|()| Ok(std::fs::rename(&tmp, path)?)) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        Ok(())
    }

    /// Teach a term. Idempotent by (case-insensitive) term.
    pub fn teach(&mut self, term: &str, label: &str, scope: &str) {
        self.teach_with(term, label, scope, false);
    }

    /// Teach a term, choosing whether it only matches as a whole word.
    pub fn teach_with(&mut self, term: &str, label: &str, scope: &str, whole_word: bool) {
        if let Some(e) = self
            .deny
            .iter_mut()
            .find(|e| e.term.eq_ignore_ascii_case(term))
        {
            e.label = label.to_string();
            e.whole_word = whole_word;
            return;
        }
        self.deny.push(Entry {
            term: term.to_string(),
            label: label.to_string(),
            scope: scope.to_string(),
            aliases: Vec::new(),
            source: "user".into(),
            whole_word,
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
        self.hidden_entries()
            .into_iter()
            .map(|h| (h.form, h.label))
            .collect()
    }

    /// Like [`Store::hidden_forms`], with the matching mode of each form.
    pub fn hidden_entries(&self) -> Vec<HiddenForm> {
        let mut v = Vec::new();
        for e in &self.deny {
            for form in std::iter::once(&e.term).chain(&e.aliases) {
                v.push(HiddenForm {
                    form: form.clone(),
                    label: e.label.clone(),
                    whole_word: e.whole_word,
                });
            }
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_is_atomic_private_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.json");

        let mut store = Store::default();
        store.teach("Aerolith", "ORG", "global");
        store.save(&path).unwrap();
        store.teach("Cartalian", "ORG", "global");
        store.save(&path).unwrap();

        assert_eq!(Store::load(&path).unwrap().deny.len(), 2);
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "stray files: {names:?}");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn old_store_files_without_whole_word_still_load() {
        let raw = r#"{"deny":[{"term":"Acme","label":"ORG"}]}"#;
        let store: Store = serde_json::from_str(raw).unwrap();
        assert!(!store.deny[0].whole_word);
    }
}
