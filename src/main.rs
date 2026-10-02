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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let store = Store::load(&cli.store)?;
    let gw = Gateway::with_default_detectors(store);

    match cli.command {
        Command::Teach { term, label } => {
            let mut gw = gw;
            gw.teach(&term, &label, "global");
            gw.store.save(&cli.store)?;
            println!("taught: {term} ({label})");
        }
        Command::Unteach { term } => {
            let mut gw = gw;
            if gw.unteach(&term) {
                gw.store.save(&cli.store)?;
                println!("unteached: {term}");
            } else {
                println!("not found: {term}");
            }
        }
        Command::Scan { text } => {
            let mut gw = gw;
            let out = gw.process(std::slice::from_ref(&text));
            gw.assert_clean(&out).map_err(|e| anyhow::anyhow!(e))?;
            println!("-- what the cloud model sees --");
            println!("{}", out[0]);
            println!("-- rehydrated (what you see) --");
            println!("{}", gw.rehydrate(&out[0]));
        }
        Command::Serve { bind } => {
            portcullis::proxy::serve(gw, &bind).await?;
        }
    }
    Ok(())
}
