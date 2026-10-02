use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use portcullis::model::{DetectorSettings, ModelRegistry, DEFAULT_REGISTRY_FILE};
use portcullis::{Gateway, Store};

#[derive(Parser)]
#[command(
    name = "portcullis",
    about = "Local-first, teachable PII gateway",
    version
)]
struct Cli {
    /// Path to the entity store (the crown jewels). Keep it out of the repo.
    #[arg(long, global = true, default_value = "store.json")]
    store: PathBuf,

    /// Name of a detector from the registry (see the `models` subcommand).
    ///
    /// The preset supplies the model directory, its label set and its threshold
    /// together, so they cannot drift apart.
    #[arg(long, global = true)]
    model: Option<String>,

    /// Path to the model registry file (default: portcullis.models.json).
    #[arg(long, global = true)]
    models: Option<PathBuf>,

    /// Model directory, when not selecting a named registry entry.
    #[arg(long, global = true)]
    model_dir: Option<PathBuf>,

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
        /// Match only at word boundaries (so "Ann" leaves "Anna" alone).
        #[arg(long)]
        whole_word: bool,
        /// Who the term applies to: `global` (everyone) or a configured scope name.
        #[arg(long, default_value = "global")]
        scope: String,
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
    /// List the detectors available in the model registry.
    Models,
    /// Run the ONNX detector over JSONL on stdin, emitting JSONL on stdout.
    ///
    /// Input, one per line:  {"id": "..", "text": ".."}
    /// Output, one per line: {"id": "..", "entities": [{start, end, label, score, source}]}
    ///
    /// The model is loaded once and reused for every line, which is what makes a
    /// full benchmark run tractable (a cold load costs seconds).
    #[cfg(feature = "onnx")]
    Detect {
        /// Comma-separated label set (overrides the preset and PORTCULLIS_LABELS).
        #[arg(long)]
        labels: Option<String>,
        /// Confidence threshold (overrides the preset and PORTCULLIS_THRESHOLD).
        #[arg(long)]
        threshold: Option<f32>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing();
    let command = cli.command;

    // `models` only needs the registry, not a loaded detector.
    if matches!(command, Command::Models) {
        return list_models(cli.models.clone());
    }

    let settings = DetectorSettings::resolve(
        cli.model.clone(),
        cli.model_dir.clone(),
        None,
        None,
        cli.models.clone(),
    )?;

    // Each arm builds only what it needs, so `detect` does not pay for a Gateway
    // (which would load the ONNX model a second time).
    match command {
        Command::Teach {
            term,
            label,
            whole_word,
            scope,
        } => {
            let mut gw = Gateway::with_settings(Store::load(&cli.store)?, &settings);
            gw.teach_with(&term, &label, &scope, whole_word);
            gw.store.save(&cli.store)?;
            println!("taught: {term} ({label})");
        }
        Command::Unteach { term } => {
            let mut gw = Gateway::with_settings(Store::load(&cli.store)?, &settings);
            if gw.unteach(&term) {
                gw.store.save(&cli.store)?;
                println!("unteached: {term}");
            } else {
                println!("not found: {term}");
            }
        }
        Command::Scan { text } => {
            let mut gw = Gateway::with_settings(Store::load(&cli.store)?, &settings);
            let out = gw.process(std::slice::from_ref(&text));
            gw.assert_clean(&out).map_err(|e| anyhow::anyhow!(e))?;
            println!("-- what the cloud model sees --");
            println!("{}", out[0]);
            println!("-- rehydrated (what you see) --");
            println!("{}", gw.rehydrate(&out[0]));
        }
        Command::Serve { bind } => {
            let gw = Gateway::with_settings(Store::load(&cli.store)?, &settings);
            portcullis::proxy::serve(gw, &bind).await?;
        }
        Command::Models => {}
        #[cfg(feature = "onnx")]
        Command::Detect { labels, threshold } => {
            run_detect(&settings, labels, threshold)?;
        }
    }
    Ok(())
}

/// Initialise logging.
///
/// `PORTCULLIS_LOG` sets the filter (default `info`); `PORTCULLIS_LOG_FORMAT=json`
/// switches to JSON lines for log shippers. Values never appear in logs — only
/// counts, statuses and timings.
fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};

    // `ort` logs every graph-optimisation step at INFO — hundreds of kilobytes per
    // model load, which buries the request log. Silenced unless asked for.
    let filter = EnvFilter::try_from_env("PORTCULLIS_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,ort=warn"));
    let want_json = std::env::var("PORTCULLIS_LOG_FORMAT")
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false);

    let builder = fmt().with_env_filter(filter);
    // Already-initialised (e.g. across tests) is not an error.
    if want_json {
        let _ = builder.json().try_init();
    } else {
        let _ = builder.try_init();
    }
}

/// Print the detectors available in the registry, and how to select one.
fn list_models(registry_path: Option<PathBuf>) -> Result<()> {
    let path = registry_path
        .or_else(|| std::env::var("PORTCULLIS_MODELS").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_REGISTRY_FILE));

    if !path.exists() {
        println!("No model registry at {}.", path.display());
        println!();
        println!("Create one (see docs/MODELS.md), then run `portcullis models` again.");
        println!("Or select a model directory directly, without a registry:");
        println!();
        println!("  portcullis --model-dir ./model scan \"...\"");
        println!("  PORTCULLIS_MODEL_DIR=./model portcullis scan \"...\"");
        return Ok(());
    }

    let registry = ModelRegistry::load(&path)?;
    println!("registry: {}", path.display());
    if registry.models.is_empty() {
        println!("  (no models defined)");
        return Ok(());
    }
    println!();
    for (name, entry) in &registry.models {
        let is_default = registry.default.as_deref() == Some(name.as_str());
        println!("  {}{}", name, if is_default { "  (default)" } else { "" });
        println!("      dir:       {}", entry.dir.display());
        match &entry.labels {
            Some(labels) => println!("      labels:    {} ({})", labels.len(), labels.join(", ")),
            None => println!("      labels:    <detector default>"),
        }
        println!(
            "      threshold: {}",
            entry
                .threshold
                .map(|t| t.to_string())
                .unwrap_or_else(|| "<detector default>".into())
        );
        if let Some(description) = &entry.description {
            println!("      {description}");
        }
        println!();
    }
    println!("Select one with --model <name>, or PORTCULLIS_MODEL=<name>.");
    Ok(())
}

/// Batch detection over JSONL: the model is loaded once and reused for every line.
#[cfg(feature = "onnx")]
fn run_detect(
    settings: &DetectorSettings,
    labels: Option<String>,
    threshold: Option<f32>,
) -> Result<()> {
    use std::io::{BufRead, Write};

    use portcullis::detect::Detector;
    use portcullis::detect_onnx::OnnxDetector;
    use serde_json::json;

    let mut detector = OnnxDetector::from_dir(&settings.dir)?;
    // The preset supplies the dir, labels and threshold together...
    if let Some(labels) = &settings.labels {
        detector = detector.with_labels(labels.clone());
    }
    if let Some(threshold) = settings.threshold {
        detector = detector.with_threshold(threshold);
    }
    // ...and an explicit CLI flag still wins.
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
