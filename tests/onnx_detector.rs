//! Golden test: the native Rust ONNX detector must reproduce the outputs of the
//! verified Python runtime (`reference/gliner2_onnx_runtime.py`), captured in
//! `tests/fixtures/detect_expected.json`.
//!
//! Skips gracefully when no model is present (so CI without the 1.2 GB model
//! still passes).

#![cfg(feature = "onnx")]

use std::collections::BTreeSet;
use std::path::Path;

use portcullis::detect::Detector;
use portcullis::detect_onnx::OnnxDetector;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Case {
    text: String,
    #[allow(dead_code)]
    labels: Vec<String>,
    entities: Vec<Ent>,
}

#[derive(Debug, Deserialize)]
struct Ent {
    label: String,
    start: usize,
    end: usize,
    score: f32,
}

/// Score tolerance — the Rust and Python runtimes must agree closely.
const SCORE_TOL: f32 = 1e-3;

#[test]
fn onnx_detector_matches_python_golden() {
    let model_dir = std::env::var("PORTCULLIS_MODEL_DIR").unwrap_or_else(|_| "./model".into());
    if !Path::new(&model_dir).join("gliner2_config.json").exists() {
        eprintln!("SKIP: no model at {model_dir} (set PORTCULLIS_MODEL_DIR)");
        return;
    }

    let raw = std::fs::read_to_string("tests/fixtures/detect_expected.json")
        .expect("read tests/fixtures/detect_expected.json");
    let cases: Vec<Case> = serde_json::from_str(&raw).expect("parse fixture");

    let detector = OnnxDetector::from_dir(&model_dir).expect("load OnnxDetector");

    for (i, case) in cases.iter().enumerate() {
        let got = detector.detect(&case.text);

        let expected_keys: BTreeSet<(String, usize, usize)> = case
            .entities
            .iter()
            .map(|e| (e.label.clone(), e.start, e.end))
            .collect();
        let got_keys: BTreeSet<(String, usize, usize)> = got
            .iter()
            .map(|s| (s.label.clone(), s.start, s.end))
            .collect();

        assert_eq!(
            got_keys, expected_keys,
            "case {i} entity set mismatch\n  text: {:?}\n  expected: {:?}\n  got:      {:?}",
            case.text, expected_keys, got_keys
        );

        // Scores must agree within tolerance for each matched entity.
        for exp in &case.entities {
            let found = got
                .iter()
                .find(|s| s.label == exp.label && s.start == exp.start && s.end == exp.end)
                .unwrap_or_else(|| panic!("case {i}: missing {:?}", exp));
            assert!(
                (found.score - exp.score).abs() <= SCORE_TOL,
                "case {i}: score drift for {:?} — expected {:.4}, got {:.4}",
                exp,
                exp.score,
                found.score
            );
        }
    }
}
