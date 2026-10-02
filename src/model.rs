//! Model selection: a small registry of named detector presets.
//!
//! A preset bundles everything that has to agree for a detector to work well:
//! the model directory, **its label set**, and its confidence threshold. That
//! matters because a model trained on natural label names (`first_name`) scores
//! very differently on Presidio-style names (`GIVENNAME`) — so labels belong with
//! the model, not with the caller.
//!
//! ```json
//! {
//!   "default": "general",
//!   "models": {
//!     "general":  { "dir": "./model", "labels": ["person", "email"], "threshold": 0.5 },
//!     "clinical": { "dir": "./models/clinical", "threshold": 0.3 }
//!   }
//! }
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Default registry filename, looked for in the working directory.
pub const DEFAULT_REGISTRY_FILE: &str = "portcullis.models.json";

fn default_family() -> String {
    "gliner2".into()
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// One selectable detector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    /// Directory holding the ONNX graphs and tokenizer.
    pub dir: PathBuf,

    /// Label set this model should be asked for. Omitted → the detector default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<String>>,

    /// Confidence threshold. Omitted → the detector default (0.5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<f32>,

    /// Free-text note shown by `portcullis models`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Detector family. Only `gliner2` is implemented today; recorded so a
    /// future adapter (GLiNER v1, token-classification) can be selected by name
    /// without changing the registry format.
    #[serde(default = "default_family")]
    pub family: String,
}

/// A named collection of detectors.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelRegistry {
    /// Used when no model is named explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,

    #[serde(default)]
    pub models: BTreeMap<String, ModelEntry>,
}

impl ModelRegistry {
    /// Load a registry from disk. A missing file is an error only if a model was
    /// explicitly requested — see [`DetectorSettings::resolve`].
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read model registry {}", path.display()))?;
        let registry: ModelRegistry = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse model registry {}", path.display()))?;
        for (name, entry) in &registry.models {
            if entry.family != "gliner2" {
                bail!(
                    "model '{name}' declares family '{}', but only 'gliner2' is supported",
                    entry.family
                );
            }
        }
        Ok(registry)
    }

    pub fn names(&self) -> Vec<String> {
        self.models.keys().cloned().collect()
    }

    /// Resolve a name (or the registry default) to an entry.
    pub fn resolve(&self, name: Option<&str>) -> Result<(String, &ModelEntry)> {
        let name = match name.map(str::to_string).or_else(|| self.default.clone()) {
            Some(n) => n,
            None => bail!(
                "no model name given and the registry has no 'default' \
                 (available: {})",
                self.names().join(", ")
            ),
        };
        let entry = self.models.get(&name).with_context(|| {
            format!(
                "unknown model '{name}' (available: {})",
                self.names().join(", ")
            )
        })?;
        Ok((name, entry))
    }
}

/// Fully-resolved detector settings, ready to build an `OnnxDetector` from.
#[derive(Debug, Clone, PartialEq)]
pub struct DetectorSettings {
    pub dir: PathBuf,
    pub labels: Option<Vec<String>>,
    pub threshold: Option<f32>,
    /// Set when the settings came from a named registry entry.
    pub model_name: Option<String>,
}

impl DetectorSettings {
    /// The plain, un-named default: `./model` with detector defaults.
    pub fn default_dir() -> Self {
        Self { dir: PathBuf::from("./model"), labels: None, threshold: None, model_name: None }
    }

    /// Resolve settings from CLI values (which override environment variables).
    ///
    /// * A named model (`--model` / `PORTCULLIS_MODEL`) is looked up in the
    ///   registry (`--models` / `PORTCULLIS_MODELS` / `portcullis.models.json`).
    /// * With no name, `--model-dir` / `PORTCULLIS_MODEL_DIR` (default `./model`)
    ///   is used directly, with `--labels` / `PORTCULLIS_LABELS` and
    ///   `--threshold` / `PORTCULLIS_THRESHOLD`.
    pub fn resolve(
        model: Option<String>,
        model_dir: Option<PathBuf>,
        labels: Option<String>,
        threshold: Option<f32>,
        registry_path: Option<PathBuf>,
    ) -> Result<Self> {
        let model = model.or_else(|| env_nonempty("PORTCULLIS_MODEL"));

        if let Some(name) = model {
            let path = registry_path
                .or_else(|| env_nonempty("PORTCULLIS_MODELS").map(PathBuf::from))
                .unwrap_or_else(|| PathBuf::from(DEFAULT_REGISTRY_FILE));
            let registry = ModelRegistry::load(&path).with_context(|| {
                format!("model '{name}' was requested, but no registry was readable at {}", path.display())
            })?;
            let (name, entry) = registry.resolve(Some(&name))?;
            return Ok(Self {
                dir: entry.dir.clone(),
                labels: entry.labels.clone(),
                threshold: entry.threshold,
                model_name: Some(name),
            });
        }

        let dir = model_dir
            .or_else(|| env_nonempty("PORTCULLIS_MODEL_DIR").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("./model"));

        let labels = labels
            .or_else(|| env_nonempty("PORTCULLIS_LABELS"))
            .map(|s| split_labels(&s))
            .filter(|v| !v.is_empty());

        let threshold = threshold
            .or_else(|| env_nonempty("PORTCULLIS_THRESHOLD").and_then(|v| v.parse::<f32>().ok()))
            .filter(|t| (0.0..=1.0).contains(t));

        Ok(Self { dir, labels, threshold, model_name: None })
    }
}

fn split_labels(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_registry(body: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.json");
        std::fs::write(&path, body).unwrap();
        (dir, path)
    }

    const REGISTRY: &str = r#"{
      "default": "general",
      "models": {
        "general":  { "dir": "./model", "labels": ["person", "email"], "threshold": 0.5 },
        "clinical": { "dir": "./models/clinical", "threshold": 0.3 }
      }
    }"#;

    #[test]
    fn resolves_a_named_model_with_its_labels_and_threshold() {
        let (_d, path) = write_registry(REGISTRY);
        let s = DetectorSettings::resolve(Some("clinical".into()), None, None, None, Some(path))
            .unwrap();
        assert_eq!(s.model_name.as_deref(), Some("clinical"));
        assert_eq!(s.dir, PathBuf::from("./models/clinical"));
        assert_eq!(s.labels, None, "clinical declared no labels");
        assert_eq!(s.threshold, Some(0.3));
    }

    #[test]
    fn falls_back_to_the_registry_default() {
        let (_d, path) = write_registry(REGISTRY);
        let s = DetectorSettings::resolve(
            Some("__use_default__".into()).filter(|_| false), // force no explicit name
            None, None, None, Some(path.clone()),
        )
        .unwrap();
        // No named model and no PORTCULLIS_MODEL -> raw dir path, not the registry.
        assert!(s.model_name.is_none());

        let registry = ModelRegistry::load(&path).unwrap();
        let (name, entry) = registry.resolve(None).unwrap();
        assert_eq!(name, "general");
        assert_eq!(entry.dir, PathBuf::from("./model"));
        assert_eq!(entry.labels.as_deref().map(|l| l.len()), Some(2));
    }

    #[test]
    fn unknown_model_name_is_an_error_that_lists_alternatives() {
        let (_d, path) = write_registry(REGISTRY);
        let err = DetectorSettings::resolve(Some("nope".into()), None, None, None, Some(path))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown model 'nope'"), "{err}");
        assert!(err.contains("general"), "{err}");
        assert!(err.contains("clinical"), "{err}");
    }

    #[test]
    fn naming_a_model_without_a_registry_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.json");
        let err = DetectorSettings::resolve(Some("general".into()), None, None, None, Some(missing))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no registry was readable"), "{err}");
    }

    #[test]
    fn unsupported_family_is_rejected_at_load() {
        let (_d, path) = write_registry(
            r#"{ "models": { "x": { "dir": "./x", "family": "token-classification" } } }"#,
        );
        let err = ModelRegistry::load(&path).unwrap_err().to_string();
        assert!(err.contains("only 'gliner2' is supported"), "{err}");
    }

    #[test]
    fn plain_dir_resolution_splits_labels() {
        let s = DetectorSettings::resolve(
            None,
            Some(PathBuf::from("./m")),
            Some("a, b ,,c".into()),
            Some(0.3),
            None,
        )
        .unwrap();
        assert_eq!(s.dir, PathBuf::from("./m"));
        assert_eq!(s.labels, Some(vec!["a".into(), "b".into(), "c".into()]));
        assert_eq!(s.threshold, Some(0.3));
        assert!(s.model_name.is_none());
    }

    #[test]
    fn out_of_range_threshold_is_ignored() {
        let s = DetectorSettings::resolve(None, Some(PathBuf::from("./m")), None, Some(9.0), None)
            .unwrap();
        assert_eq!(s.threshold, None);
    }
}
