//! Native, in-process GLiNER2-PII detector using ONNX Runtime.
//!
//! Compiled only with the `onnx` cargo feature. [`OnnxDetector`] is synchronous
//! and implements [`Detector`], so it can be called directly from a `tokio` task
//! with no runtime spawn and no network.
//!
//! The decode mirrors `reference/gliner2_onnx_runtime.py` exactly:
//! schema prefix -> word offsets -> word-span grid -> span_rep -> einsum with
//! `count_embed`-transformed label embeddings -> sigmoid -> threshold -> NMS.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use ndarray::{Array2, Array3, Axis, Ix2, Ix3};
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;
use regex::Regex;
use serde::Deserialize;
use tokenizers::Tokenizer;

use crate::detect::{Detector, Span};

const DEFAULT_LABELS: &[&str] = &[
    "person",
    "full_name",
    "email",
    "phone_number",
    "account_number",
    "api_key",
    "city",
];

/// Label set resolution order: `PORTCULLIS_LABELS` (comma-separated) → the
/// built-in default. Benchmarks and specialised deployments pass their own set.
fn resolve_labels() -> Vec<String> {
    match std::env::var("PORTCULLIS_LABELS") {
        Ok(v) if !v.trim().is_empty() => v
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => DEFAULT_LABELS.iter().map(|s| s.to_string()).collect(),
    }
}

/// Confidence threshold resolution: `PORTCULLIS_THRESHOLD` → 0.5.
/// Recall-oriented deployments (and the PIIMB benchmark) use 0.3.
fn resolve_threshold() -> f32 {
    std::env::var("PORTCULLIS_THRESHOLD")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .filter(|t| (0.0..=1.0).contains(t))
        .unwrap_or(0.5)
}

const NER_TASK_NAME: &str = "entities";
const SCHEMA_OPEN: &str = "(";
const SCHEMA_CLOSE: &str = ")";
const TOKEN_P: &str = "[P]";
const TOKEN_E: &str = "[E]";
const TOKEN_SEP_TEXT: &str = "[SEP_TEXT]";

const ONNX_INPUT_IDS: &str = "input_ids";
const ONNX_ATTENTION_MASK: &str = "attention_mask";
const ONNX_HIDDEN_STATES: &str = "hidden_states";
const ONNX_SPAN_START_IDX: &str = "span_start_idx";
const ONNX_SPAN_END_IDX: &str = "span_end_idx";
const ONNX_LABEL_EMBEDDINGS: &str = "label_embeddings";

#[derive(Debug, Clone, Deserialize)]
struct Gliner2Config {
    max_width: usize,
    special_tokens: HashMap<String, u32>,
    onnx_files: HashMap<String, HashMap<String, String>>,
}

/// In-process ONNX GLiNER2 detector.
pub struct OnnxDetector {
    encoder: Mutex<Session>,
    span_rep: Mutex<Session>,
    count_embed: Mutex<Session>,
    tokenizer: Tokenizer,
    special_tokens: HashMap<String, u32>,
    max_width: usize,
    labels: Vec<String>,
    threshold: f32,
}

impl OnnxDetector {
    /// Load a detector from `PORTCULLIS_MODEL_DIR`, falling back to `./model`.
    pub fn from_env() -> Result<Self> {
        let dir = std::env::var("PORTCULLIS_MODEL_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("./model"));
        Self::from_dir(dir)
    }

    /// Load a detector from a local model directory.
    pub fn from_dir<P: AsRef<Path>>(dir: P) -> Result<Self> {
        let dir = dir.as_ref();
        let config: Gliner2Config = read_json(dir.join("gliner2_config.json"))
            .context("failed to load gliner2_config.json")?;

        let special_tokens = config.special_tokens;
        for token in [TOKEN_P, TOKEN_E, TOKEN_SEP_TEXT] {
            if !special_tokens.contains_key(token) {
                anyhow::bail!("missing special token {token}");
            }
        }

        let onnx_files = config
            .onnx_files
            .get("fp32")
            .context("missing fp32 ONNX file entries")?;

        let encoder = load_session(dir, onnx_files.get("encoder").context("missing encoder")?)?;
        let span_rep = load_session(dir, onnx_files.get("span_rep").context("missing span_rep")?)?;
        let count_embed = load_session(
            dir,
            onnx_files
                .get("count_embed")
                .context("missing count_embed")?,
        )?;

        let tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(|e| anyhow::anyhow!("failed to load tokenizer: {e}"))?;

        Ok(Self {
            encoder: Mutex::new(encoder),
            span_rep: Mutex::new(span_rep),
            count_embed: Mutex::new(count_embed),
            tokenizer,
            special_tokens,
            max_width: config.max_width,
            labels: resolve_labels(),
            threshold: resolve_threshold(),
        })
    }

    /// Return the label set used by the detector.
    pub fn labels(&self) -> &[String] {
        &self.labels
    }

    /// Override the label set (builder).
    pub fn with_labels(mut self, labels: Vec<String>) -> Self {
        self.labels = labels;
        self
    }

    /// Override the confidence threshold (builder).
    pub fn with_threshold(mut self, threshold: f32) -> Self {
        self.threshold = threshold;
        self
    }

    /// Return the confidence threshold in use.
    pub fn threshold(&self) -> f32 {
        self.threshold
    }
}

fn load_session<P: AsRef<Path>>(dir: &Path, rel: P) -> Result<Session> {
    let path = dir.join(rel);
    // NOTE: `ort::Error<SessionBuilder>` is not `Sync`, so anyhow's `.context()`
    // cannot be used on the builder chain — `map_err` instead.
    let builder = Session::builder()
        .map_err(|e| anyhow::anyhow!("failed to create ONNX session builder: {e}"))?;
    let mut builder = builder
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(|e| anyhow::anyhow!("failed to set optimization level: {e}"))?;
    builder
        .commit_from_file(&path)
        .map_err(|e| anyhow::anyhow!("failed to load ONNX model {}: {e}", path.display()))
}

fn read_json<T: serde::de::DeserializeOwned, P: AsRef<Path>>(path: P) -> Result<T> {
    let path = path.as_ref();
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&contents).with_context(|| format!("failed to parse {}", path.display()))
}

impl Detector for OnnxDetector {
    fn detect(&self, text: &str) -> Vec<Span> {
        self.detect_with_threshold(text, self.threshold)
    }
}

impl OnnxDetector {
    /// Run detection with a configurable threshold.
    pub fn detect_with_threshold(&self, text: &str, threshold: f32) -> Vec<Span> {
        if text.trim().is_empty() {
            return Vec::new();
        }

        let (
            input_ids,
            attention_mask,
            e_positions,
            word_offsets,
            text_start_idx,
            first_token_positions,
        ) = match self.build_ner_input(text) {
            Ok(x) => x,
            Err(_) => return Vec::new(),
        };

        let num_words = word_offsets.len();
        if num_words == 0 {
            return Vec::new();
        }

        // hidden: [1, seq_len, hidden]
        let hidden = match run_encoder(&self.encoder, input_ids, attention_mask) {
            Ok(h) => h,
            Err(_) => return Vec::new(),
        };

        let dim = hidden.shape()[2];
        let seq_len = hidden.shape()[1];

        // label_embeddings: [num_labels, hidden] — gather rows at e_positions.
        let mut label_embeddings = Array2::<f32>::zeros((e_positions.len(), dim));
        for (i, &p) in e_positions.iter().enumerate() {
            for j in 0..dim {
                label_embeddings[[i, j]] = hidden[[0, p, j]];
            }
        }

        // text_hidden: [seq_len - text_start_idx, hidden]
        let text_len = seq_len.saturating_sub(text_start_idx);
        let mut text_hidden = Array2::<f32>::zeros((text_len, dim));
        for i in 0..text_len {
            for j in 0..dim {
                text_hidden[[i, j]] = hidden[[0, text_start_idx + i, j]];
            }
        }

        let (word_span_start, word_span_end) = generate_spans(num_words, self.max_width);
        let token_span_start: Vec<i64> = word_span_start
            .iter()
            .map(|&i| first_token_positions[i] as i64)
            .collect();
        let token_span_end: Vec<i64> = word_span_end
            .iter()
            .map(|&i| first_token_positions[i] as i64)
            .collect();

        let span_rep = match run_span_rep(
            &self.span_rep,
            &text_hidden,
            &token_span_start,
            &token_span_end,
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let scores = match compute_scores(&self.count_embed, &span_rep, &label_embeddings) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let mut entities: Vec<RawEntity> = Vec::new();
        for (span_idx, (&start_word, &end_word)) in
            word_span_start.iter().zip(word_span_end.iter()).enumerate()
        {
            for (label_idx, label) in self.labels.iter().enumerate() {
                let score = scores[[span_idx, label_idx]];
                if score >= threshold {
                    let char_start = word_offsets[start_word].0;
                    let char_end = word_offsets[end_word].1;
                    entities.push(RawEntity {
                        label: label.clone(),
                        start: char_start,
                        end: char_end,
                        score,
                    });
                }
            }
        }

        deduplicate_entities(entities)
            .into_iter()
            .map(|e| Span {
                start: e.start,
                end: e.end,
                label: e.label,
                source: "onnx",
                score: e.score,
            })
            .collect()
    }

    /// Build the NER token sequence and the word -> first-token map.
    fn build_ner_input(
        &self,
        text: &str,
    ) -> Result<(
        Array2<i64>,
        Array2<i64>,
        Vec<usize>,
        Vec<(usize, usize)>,
        usize,
        Vec<usize>,
    )> {
        let (mut tokens, e_positions) =
            self.build_schema_prefix(NER_TASK_NAME, &self.labels, TOKEN_E)?;
        let text_start_idx = tokens.len();

        let lower = text.to_lowercase();
        let word_re = word_pattern();

        let mut word_offsets: Vec<(usize, usize)> = Vec::new();
        let mut first_token_positions: Vec<usize> = Vec::new();
        let mut token_idx = 0usize;

        for m in word_re.find_iter(&lower) {
            word_offsets.push((m.start(), m.end()));
            first_token_positions.push(token_idx);

            let ids = encode_token(&self.tokenizer, m.as_str())?;
            for &id in &ids {
                tokens.push(id as i64);
            }
            token_idx += ids.len();
        }

        let seq_len = tokens.len();
        let input_ids = Array2::from_shape_vec((1, seq_len), tokens)?;
        let attention_mask = Array2::<i64>::ones((1, seq_len));

        Ok((
            input_ids,
            attention_mask,
            e_positions,
            word_offsets,
            text_start_idx,
            first_token_positions,
        ))
    }

    /// Build `( [P] task ( [E] label1 [E] label2 ... ) ) [SEP_TEXT]`.
    fn build_schema_prefix(
        &self,
        task_name: &str,
        labels: &[String],
        label_token: &str,
    ) -> Result<(Vec<i64>, Vec<usize>)> {
        let p_id = self
            .special_tokens
            .get(TOKEN_P)
            .context("missing [P] token")?;
        let label_token_id = self
            .special_tokens
            .get(label_token)
            .context("missing label token")?;
        let sep_text_id = self
            .special_tokens
            .get(TOKEN_SEP_TEXT)
            .context("missing [SEP_TEXT] token")?;

        let mut tokens: Vec<i64> = Vec::new();
        tokens.extend(
            encode_token(&self.tokenizer, SCHEMA_OPEN)?
                .iter()
                .map(|&id| id as i64),
        );
        tokens.push(*p_id as i64);
        tokens.extend(
            encode_token(&self.tokenizer, task_name)?
                .iter()
                .map(|&id| id as i64),
        );
        tokens.extend(
            encode_token(&self.tokenizer, SCHEMA_OPEN)?
                .iter()
                .map(|&id| id as i64),
        );

        let mut label_positions = Vec::with_capacity(labels.len());
        for label in labels {
            label_positions.push(tokens.len());
            tokens.push(*label_token_id as i64);
            tokens.extend(
                encode_token(&self.tokenizer, label)?
                    .iter()
                    .map(|&id| id as i64),
            );
        }

        tokens.extend(
            encode_token(&self.tokenizer, SCHEMA_CLOSE)?
                .iter()
                .map(|&id| id as i64),
        );
        tokens.extend(
            encode_token(&self.tokenizer, SCHEMA_CLOSE)?
                .iter()
                .map(|&id| id as i64),
        );
        tokens.push(*sep_text_id as i64);

        Ok((tokens, label_positions))
    }
}

fn encode_token(tokenizer: &Tokenizer, text: &str) -> Result<Vec<u32>> {
    tokenizer
        .encode(text, false)
        .map_err(|e| anyhow::anyhow!("tokenization failed for {text:?}: {e}"))
        .map(|enc| enc.get_ids().to_vec())
}

fn word_pattern() -> Regex {
    Regex::new(
        r"(?ix)
        (?:https?://[^\s]+|www\.[^\s]+)
        | [a-z0-9._%+-]+@[a-z0-9.-]+\.[a-z]{2,}
        | @[a-z0-9_]+
        | \w+(?:[-_]\w+)*
        | \S",
    )
    .expect("word regex is valid")
}

fn generate_spans(num_words: usize, max_width: usize) -> (Vec<usize>, Vec<usize>) {
    let mut start_indices = Vec::new();
    let mut end_indices = Vec::new();
    for i in 0..num_words {
        let limit = max_width.min(num_words - i);
        for j in 0..limit {
            start_indices.push(i);
            end_indices.push(i + j);
        }
    }
    (start_indices, end_indices)
}

#[derive(Debug, Clone)]
struct RawEntity {
    label: String,
    start: usize,
    end: usize,
    score: f32,
}

fn deduplicate_entities(mut entities: Vec<RawEntity>) -> Vec<RawEntity> {
    entities.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut kept: Vec<RawEntity> = Vec::new();
    for e in entities {
        let overlaps = kept
            .iter()
            .any(|k| k.label == e.label && e.start < k.end && e.end > k.start);
        if !overlaps {
            kept.push(e);
        }
    }
    kept
}

fn run_encoder(
    encoder: &Mutex<Session>,
    input_ids: Array2<i64>,
    attention_mask: Array2<i64>,
) -> Result<Array3<f32>> {
    let ids = Tensor::from_array(input_ids)?;
    let mask = Tensor::from_array(attention_mask)?;
    let mut session = encoder
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let outputs = session.run(ort::inputs![
        ONNX_INPUT_IDS => ids,
        ONNX_ATTENTION_MASK => mask
    ])?;
    let hidden = outputs[0].try_extract_array::<f32>()?.to_owned();
    Ok(hidden.into_dimensionality::<Ix3>()?)
}

fn run_span_rep(
    span_rep: &Mutex<Session>,
    text_hidden: &Array2<f32>,
    token_span_start: &[i64],
    token_span_end: &[i64],
) -> Result<Array2<f32>> {
    let num_spans = token_span_start.len();
    let hidden = Tensor::from_array(text_hidden.clone().insert_axis(Axis(0)))?;
    let span_start = Tensor::from_array(Array2::from_shape_vec(
        (1, num_spans),
        token_span_start.to_vec(),
    )?)?;
    let span_end = Tensor::from_array(Array2::from_shape_vec(
        (1, num_spans),
        token_span_end.to_vec(),
    )?)?;

    let mut session = span_rep
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let outputs = session.run(ort::inputs![
        ONNX_HIDDEN_STATES => hidden,
        ONNX_SPAN_START_IDX => span_start,
        ONNX_SPAN_END_IDX => span_end
    ])?;

    // [1, num_spans, hidden] -> [num_spans, hidden]
    let reps = outputs[0].try_extract_array::<f32>()?.to_owned();
    let reps = reps.into_dimensionality::<ndarray::Ix3>()?;
    Ok(reps.index_axis(Axis(0), 0).to_owned())
}

fn compute_scores(
    count_embed: &Mutex<Session>,
    span_rep: &Array2<f32>,
    label_embeddings: &Array2<f32>,
) -> Result<Array2<f32>> {
    let labels_in = Tensor::from_array(label_embeddings.clone())?;
    let mut session = count_embed
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let outputs = session.run(ort::inputs![ONNX_LABEL_EMBEDDINGS => labels_in])?;
    let transformed = outputs[0].try_extract_array::<f32>()?.to_owned();
    let labels_2d = transformed.into_dimensionality::<Ix2>()?;

    // numpy: einsum("sh,lh->sl", span_rep, transformed_labels)
    let logits = span_rep.dot(&labels_2d.t());
    Ok(sigmoid(logits))
}

fn sigmoid(mut x: Array2<f32>) -> Array2<f32> {
    x.mapv_inplace(|v| {
        if v >= 0.0 {
            1.0 / (1.0 + (-v).exp())
        } else {
            let e = v.exp();
            e / (1.0 + e)
        }
    });
    x
}
