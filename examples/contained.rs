//! The contained core, run on a desktop: what the phone runs, reachable by
//! anything that wants to talk to it. Prints the origin and the token and
//! serves until interrupted.
//!
//!     cargo run --features contained --example contained -- /tmp/engram-contained [models-dir]
//!
//! `ENGRAM_ASK_URL` and `ENGRAM_ASK_MODEL` name an endpoint, as Settings does
//! on the phone, and the gate on generation is opened as a charging phone's is.

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
    let ask = std::env::var("ENGRAM_ASK_URL")
        .ok()
        .map(|base_url| engram::contained::Endpoint {
            base_url,
            model: std::env::var("ENGRAM_ASK_MODEL").unwrap_or_default(),
            api_key: None,
        });
    let (started, running) =
        engram::contained::start_with(&dir, engram::contained::Setup { models, ask }).await?;
    running.allow_generation(true);
    println!("origin=http://127.0.0.1:{}", started.port);
    println!("token={}", started.token);
    tokio::signal::ctrl_c().await?;
    running.stop().await;
    Ok(())
}
