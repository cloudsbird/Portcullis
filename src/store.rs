use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::crypt::{self, StoreKey};

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
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub deny: Vec<Entry>,
    #[serde(default)]
    pub allow: Vec<Entry>,
}

impl Store {
    /// Load the store, decrypting it with the key from `PORTCULLIS_STORE_KEY` or
    /// `PORTCULLIS_STORE_KEY_FILE` when the file is encrypted.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        Self::load_with(path, StoreKey::from_env()?.as_ref())
    }

    /// [`Store::load`] with an explicit key.
    ///
    /// A plaintext file loads whether or not a key is given (so turning encryption on
    /// is just setting the key; the next save encrypts it). An encrypted file without
    /// the key is an error, never an empty store — silently starting empty would
    /// overwrite the real one on the next teach.
    pub fn load_with(path: impl AsRef<Path>, key: Option<&StoreKey>) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Store::default());
        }
        let bytes = std::fs::read(path)?;
        if crypt::is_encrypted(&bytes) {
            let key = key.ok_or_else(|| {
                anyhow::anyhow!(
                    "{} is encrypted: set PORTCULLIS_STORE_KEY or PORTCULLIS_STORE_KEY_FILE",
                    path.display()
                )
            })?;
            let plain = crypt::open(key, &bytes)?;
            return Ok(serde_json::from_slice(&plain)?);
        }
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Is the store at `path` encrypted? (`false` if it does not exist yet.)
    pub fn is_encrypted(path: impl AsRef<Path>) -> bool {
        std::fs::read(path).is_ok_and(|b| crypt::is_encrypted(&b))
    }

    /// Persist the store, atomically.
    ///
    /// The JSON goes to a sibling temp file which is flushed to disk and then
    /// renamed over the target, so a crash mid-write leaves the previous store
    /// intact instead of a truncated one. On Unix the file is created `0600`: it
    /// is a map of everything you consider private, and there is no encryption at
    /// rest unless a key is configured — so at minimum it must not be world-readable.
    ///
    /// With `PORTCULLIS_STORE_KEY` / `PORTCULLIS_STORE_KEY_FILE` set the contents are
    /// encrypted (see [`crate::crypt`]).
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        self.save_with(path, StoreKey::from_env()?.as_ref())
    }

    /// [`Store::save`] with an explicit key (`None`: plaintext).
    pub fn save_with(&self, path: impl AsRef<Path>, key: Option<&StoreKey>) -> Result<()> {
        use std::io::Write;

        let path = path.as_ref();
        let plain = serde_json::to_vec_pretty(self)?;
        let raw = match key {
            Some(key) => crypt::seal(key, &plain)?,
            None => plain,
        };

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
            file.write_all(&raw)?;
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
    ///
    /// Idempotent by (term, scope): the same word taught for two scopes is two
    /// entries, so teaching it for one client never rewrites another client's.
    pub fn teach_with(&mut self, term: &str, label: &str, scope: &str, whole_word: bool) {
        if let Some(e) = self
            .deny
            .iter_mut()
            .find(|e| e.term.eq_ignore_ascii_case(term) && e.scope.eq_ignore_ascii_case(scope))
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

    /// Remove a taught term from every scope. Returns whether anything changed.
    pub fn unteach(&mut self, term: &str) -> bool {
        self.unteach_in(term, None)
    }

    /// Remove a taught term, from one scope or (`None`) from all of them.
    pub fn unteach_in(&mut self, term: &str, scope: Option<&str>) -> bool {
        let before = self.deny.len();
        self.deny.retain(|e| {
            !(e.term.eq_ignore_ascii_case(term)
                && scope.is_none_or(|s| e.scope.eq_ignore_ascii_case(s)))
        });
        before != self.deny.len()
    }

    pub fn is_allowed(&self, term: &str) -> bool {
        self.is_allowed_for(None, term)
    }

    /// Whether `term` is on the allow-list for `scope` (see [`applies`]).
    pub fn is_allowed_for(&self, scope: Option<&str>, term: &str) -> bool {
        self.allow
            .iter()
            .any(|e| applies(&e.scope, scope) && e.term.eq_ignore_ascii_case(term))
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
        self.hidden_entries_for(None)
    }

    /// The forms that apply to a request in `scope` (see [`applies`]).
    pub fn hidden_entries_for(&self, scope: Option<&str>) -> Vec<HiddenForm> {
        let mut v = Vec::new();
        for e in self.deny.iter().filter(|e| applies(&e.scope, scope)) {
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

/// The scope shared by every request.
pub const GLOBAL_SCOPE: &str = "global";

/// Does an entry taught under `entry_scope` apply to a request in `request_scope`?
///
/// * `None` — single-tenant mode: no scopes are configured, every entry applies.
/// * `Some(s)` — multi-tenant: `global` entries and entries for `s`, nothing else.
pub fn applies(entry_scope: &str, request_scope: Option<&str>) -> bool {
    match request_scope {
        None => true,
        Some(s) => {
            entry_scope.eq_ignore_ascii_case(GLOBAL_SCOPE) || entry_scope.eq_ignore_ascii_case(s)
        }
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
    fn the_same_term_taught_for_two_scopes_stays_two_entries() {
        let mut s = Store::default();
        s.teach("Acme", "ORG", "alpha");
        s.teach("Acme", "CLIENT", "beta");
        assert_eq!(s.deny.len(), 2);
        assert_eq!(s.hidden_entries_for(Some("alpha")).len(), 1);
        assert_eq!(s.hidden_entries_for(Some("alpha"))[0].label, "ORG");
        assert!(s.hidden_entries_for(Some("gamma")).is_empty());
        assert_eq!(
            s.hidden_entries_for(None).len(),
            2,
            "single-tenant: all apply"
        );

        assert!(s.unteach_in("acme", Some("alpha")));
        assert_eq!(s.deny.len(), 1);
        assert_eq!(s.deny[0].scope, "beta");
    }

    fn cheap_key(pass: &str) -> StoreKey {
        StoreKey::new(pass).with_params(crypt::KdfParams {
            m_kib: 8,
            t: 1,
            p: 1,
        })
    }

    #[test]
    fn an_encrypted_store_round_trips_and_never_holds_plaintext_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.json");
        let key = cheap_key("a long enough passphrase");

        let mut store = Store::default();
        store.teach("Cartalian", "ORG", "alpha");
        store.save_with(&path, Some(&key)).unwrap();

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(
            !on_disk.contains("Cartalian") && !on_disk.contains("alpha"),
            "{on_disk}"
        );
        assert!(Store::is_encrypted(&path));

        let back = Store::load_with(&path, Some(&key)).unwrap();
        assert_eq!(back.deny[0].term, "Cartalian");
        assert_eq!(back.deny[0].scope, "alpha");
    }

    #[test]
    fn an_encrypted_store_without_its_key_is_an_error_not_an_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.json");
        let mut store = Store::default();
        store.teach("Cartalian", "ORG", "global");
        store
            .save_with(&path, Some(&cheap_key("right key right key")))
            .unwrap();

        let err = Store::load_with(&path, None).unwrap_err().to_string();
        assert!(err.contains("encrypted"), "{err}");
        let err = Store::load_with(&path, Some(&cheap_key("wrong key wrong key")))
            .unwrap_err()
            .to_string();
        assert!(err.contains("wrong key"), "{err}");
    }

    #[test]
    fn a_plaintext_store_migrates_when_a_key_is_first_supplied() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.json");
        let mut store = Store::default();
        store.teach("Cartalian", "ORG", "global");
        store.save_with(&path, None).unwrap();
        assert!(!Store::is_encrypted(&path));

        let key = cheap_key("fresh key fresh key");
        let loaded = Store::load_with(&path, Some(&key)).unwrap();
        loaded.save_with(&path, Some(&key)).unwrap();
        assert!(Store::is_encrypted(&path));
        assert_eq!(Store::load_with(&path, Some(&key)).unwrap().deny.len(), 1);
    }

    #[test]
    fn old_store_files_without_whole_word_still_load() {
        let raw = r#"{"deny":[{"term":"Acme","label":"ORG"}]}"#;
        let store: Store = serde_json::from_str(raw).unwrap();
        assert!(!store.deny[0].whole_word);
    }
}
