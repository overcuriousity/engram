//! The contained core, run on a desktop: what the phone runs, reachable by
//! anything that wants to talk to it. Prints the origin and the token and
//! serves until interrupted.
//!
//!     cargo run --features contained --example contained -- /tmp/engram-contained [models-dir]

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // To stderr, so stdout stays the two lines a script reads.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "engram=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let mut args = std::env::args().skip(1);
    let dir = std::path::PathBuf::from(args.next().expect("a data directory"));
    let models = args.next().map(std::path::PathBuf::from);
    let pick = |name: &str| {
        models
            .as_ref()
            .map(|d| d.join(name))
            .filter(|p| p.is_file())
    };
    let models = engram::infer::local::LocalModels {
        embed: pick("embed.gguf"),
        rerank: pick("rerank.gguf"),
        ask: pick("ask.gguf"),
        speech: pick("speech.bin"),
    };
    let (started, running) = engram::contained::start(&dir, models).await?;
    println!("origin=http://127.0.0.1:{}", started.port);
    println!("token={}", started.token);
    tokio::signal::ctrl_c().await?;
    running.stop().await;
    Ok(())
}
