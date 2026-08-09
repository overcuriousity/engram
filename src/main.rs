mod config;
mod error;
mod store;

use axum::{Router, routing::get};
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "pkdb")]
struct Args {
    #[arg(long)]
    config: Option<PathBuf>,
    /// Print the effective config with secrets redacted, then exit.
    #[arg(long)]
    print_config: bool,
    /// Permit local auth mode on a non-loopback bind address. Do not use.
    #[arg(long)]
    i_know_this_is_insecure: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "pkdb=info,tower_http=info".into()),
        )
        .init();

    let args = Args::parse();
    let cfg = config::Config::load(args.config.as_deref())?;

    if args.print_config {
        println!("{}", cfg.redacted());
        return Ok(());
    }

    let app = Router::new().route("/healthz", get(|| async { "ok" }));
    let listener = tokio::net::TcpListener::bind(&cfg.server.bind).await?;
    tracing::info!(bind = %cfg.server.bind, "pkdb listening");
    axum::serve(listener, app).await?;
    Ok(())
}
