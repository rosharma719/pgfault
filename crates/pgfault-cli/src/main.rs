use anyhow::Result;
use clap::{Parser, Subcommand};
use pgfault_proxy::{bind, serve, Config};
use pgfault_scenario::Scenario;
use pgfault_trace::Recorder;
use std::path::PathBuf;
#[derive(Parser)]
#[command(
    name = "pgfault",
    version,
    about = "Deterministic PostgreSQL fault injection at semantic boundaries"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Relay PostgreSQL unchanged, optionally injecting semantic faults.
    Run {
        #[arg(long, default_value = "127.0.0.1:15432")]
        listen: String,
        #[arg(long, default_value = "127.0.0.1:5432")]
        upstream: String,
        #[arg(long)]
        scenario: Vec<PathBuf>,
        #[arg(long)]
        trace: Option<PathBuf>,
    },
    /// Recreate fired faults at their recorded semantic coordinates.
    Replay {
        trace: PathBuf,
        #[arg(long, default_value = "127.0.0.1:15432")]
        listen: String,
        #[arg(long, default_value = "127.0.0.1:5432")]
        upstream: String,
        #[arg(long)]
        output_trace: Option<PathBuf>,
    },
    /// Parse and validate a scenario without opening any sockets.
    Validate { scenario: PathBuf },
}
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let (listen, upstream, scenarios, trace) = match Cli::parse().command {
        Command::Validate { scenario } => {
            let s = Scenario::load(scenario)?;
            println!("valid: {}", s.name);
            return Ok(());
        }
        Command::Run {
            listen,
            upstream,
            scenario,
            trace,
        } => (
            listen,
            upstream,
            scenario
                .iter()
                .map(Scenario::load)
                .collect::<Result<Vec<_>>>()?,
            trace,
        ),
        Command::Replay {
            trace,
            listen,
            upstream,
            output_trace,
        } => (
            listen,
            upstream,
            pgfault_trace::replay(trace)?,
            output_trace,
        ),
    };
    let path = trace.unwrap_or_else(|| {
        PathBuf::from(format!(
            ".pgfault/traces/{}-{}.jsonl",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            std::process::id()
        ))
    });
    let recorder = Recorder::create(&path)?;
    let listener = bind(&listen).await?;
    tracing::info!(listen=%listener.local_addr()?,%upstream,trace=%path.display(),"pgfault listening");
    serve(
        listener,
        Config {
            upstream,
            scenarios,
            trace: recorder,
        },
        async {
            #[cfg(unix)]
            {
                let mut term =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("SIGTERM handler");
                tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
            }
        },
    )
    .await
}
