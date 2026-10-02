use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use portcullis::{Gateway, Store};

#[derive(Parser)]
#[command(name = "portcullis", about = "Local-first, teachable PII gateway", version)]
struct Cli {
    /// Path to the entity store (the crown jewels). Keep it out of the repo.
    #[arg(long, global = true, default_value = "store.json")]
    store: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Add a term that must always be redacted.
    Teach {
        term: String,
        #[arg(long, default_value = "ORG")]
        label: String,
    },
    /// Remove a taught term.
    Unteach { term: String },
    /// Redact a message and show exactly what the cloud model would see.
    Scan { text: String },
    /// Run the OpenAI-compatible proxy server.
    Serve {
        /// Address to bind on.
        #[arg(long, default_value = "127.0.0.1:8080")]
        bind: String,
    },
    /// Run the ONNX detector over JSONL on stdin, emitting JSONL on stdout.
    ///
    /// Input, one per line:  {"id": "..", "text": ".."}
    /// Output, one per line: {"id": "..", "entities": [{start, end, label, score, source}]}
    ///
    /// The model is loaded once and reused for every line, which is what makes a
    /// full benchmark run tractable (a cold load costs seconds).
    #[cfg(feature = "onnx")]
    Detect {
        /// Comma-separated label set (overrides PORTCULLIS_LABELS / the default).
        #[arg(long)]
        labels: Option<String>,
        /// Confidence threshold (overrides PORTCULLIS_THRESHOLD / 0.5).
        #[arg(long)]
        threshold: Option<f32>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Each arm builds only what it needs, so `detect` does not pay for a Gateway
    // (which would load the ONNX model a second time).
    match cli.command {
        Command::Teach { term, label } => {
            let mut gw = Gateway::with_default_detectors(Store::load(&cli.store)?);
            gw.teach(&term, &label, "global");
            gw.store.save(&cli.store)?;
            println!("taught: {term} ({label})");
        }
        Command::Unteach { term } => {
            let mut gw = Gateway::with_default_detectors(Store::load(&cli.store)?);
            if gw.unteach(&term) {
                gw.store.save(&cli.store)?;
                println!("unteached: {term}");
            } else {
                println!("not found: {term}");
            }
        }
        Command::Scan { text } => {
            let mut gw = Gateway::with_default_detectors(Store::load(&cli.store)?);
            let out = gw.process(std::slice::from_ref(&text));
            gw.assert_clean(&out).map_err(|e| anyhow::anyhow!(e))?;
            println!("-- what the cloud model sees --");
            println!("{}", out[0]);
            println!("-- rehydrated (what you see) --");
            println!("{}", gw.rehydrate(&out[0]));
        }
        Command::Serve { bind } => {
            let gw = Gateway::with_default_detectors(Store::load(&cli.store)?);
            portcullis::proxy::serve(gw, &bind).await?;
        }
        #[cfg(feature = "onnx")]
        Command::Detect { labels, threshold } => {
            run_detect(labels, threshold)?;
        }
    }
    Ok(())
}

/// Batch detection over JSONL: the model is loaded once and reused for every line.
#[cfg(feature = "onnx")]
fn run_detect(labels: Option<String>, threshold: Option<f32>) -> Result<()> {
    use std::io::{BufRead, Write};

    use portcullis::detect::Detector;
    use portcullis::detect_onnx::OnnxDetector;
    use serde_json::json;

    let mut detector = OnnxDetector::from_env()?;
    if let Some(labels) = labels {
        detector = detector.with_labels(
            labels
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
        );
    }
    if let Some(threshold) = threshold {
        detector = detector.with_threshold(threshold);
    }

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                writeln!(out, "{}", json!({ "error": e.to_string() }))?;
                continue;
            }
        };
        let id = value.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let text = value.get("text").and_then(|t| t.as_str()).unwrap_or("");
        let entities: Vec<serde_json::Value> = detector
            .detect(text)
            .iter()
            .map(|s| {
                json!({
                    "start": s.start,
                    "end": s.end,
                    "label": s.label,
                    "score": s.score,
                    "source": s.source,
                })
            })
            .collect();
        writeln!(out, "{}", json!({ "id": id, "entities": entities }))?;
    }
    out.flush()?;
    Ok(())
}
