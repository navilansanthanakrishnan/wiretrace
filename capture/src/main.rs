//! wiretrace capture layer.
//!
//! Two ways to observe an application's HTTP traffic, one output format.
//! Orchestration, inference and the agent-facing surface live in Python; this
//! binary only turns encrypted traffic into JSON lines on stdout.

mod browser;
mod ca;
mod event;
mod proxy;

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "wiretrace-capture", about = "Emit one JSON line per HTTP exchange")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Intercept HTTPS through a local proxy. Used for native apps.
    Proxy {
        #[arg(long, default_value = "127.0.0.1:8787")]
        listen: SocketAddr,
        #[arg(long)]
        cert_dir: PathBuf,
        /// Only intercept hosts containing one of these substrings.
        #[arg(long = "host")]
        hosts: Vec<String>,
    },
    /// Drive a managed Chrome and attribute requests to UI actions.
    Browser {
        #[arg(long)]
        open: String,
        #[arg(long)]
        profile: PathBuf,
        #[arg(long)]
        chrome: Option<PathBuf>,
        #[arg(long, default_value_t = 9222)]
        port: u16,
        /// Run Chrome without a window, for unattended capture.
        #[arg(long)]
        headless: bool,
    },
    /// Print the CA certificate path, creating the CA if needed.
    Ca {
        #[arg(long)]
        cert_dir: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Proxy { listen, cert_dir, hosts } => proxy::run(listen, &cert_dir, hosts).await,
        Command::Browser { open, profile, chrome, port, headless } => {
            browser::run(&open, &profile, chrome, port, headless).await
        }
        Command::Ca { cert_dir } => {
            println!("{}", ca::Ca::load_or_create(&cert_dir)?.cert_path.display());
            Ok(())
        }
    }
}
