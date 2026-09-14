use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use pgfault_proxy::{bind, frontend_acceptor, serve, upstream_connector, Config, TlsConfig};
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
#[derive(Args, Clone)]
struct TlsArgs {
    /// Terminate TLS from clients using this certificate (PEM). Requires --tls-key.
    #[arg(long, requires = "tls_key")]
    tls_cert: Option<PathBuf>,
    /// Private key (PEM) matching --tls-cert.
    #[arg(long, requires = "tls_cert")]
    tls_key: Option<PathBuf>,
    /// Negotiate TLS to the upstream PostgreSQL server; fails if it declines.
    #[arg(long)]
    upstream_tls: bool,
    /// Skip verifying the upstream server's TLS certificate (testing only).
    #[arg(long, requires = "upstream_tls")]
    upstream_tls_insecure: bool,
    /// Extra CA certificate (PEM) to trust when verifying the upstream server.
    #[arg(long, requires = "upstream_tls")]
    upstream_ca: Option<PathBuf>,
}
impl TlsArgs {
    fn into_config(self) -> Result<TlsConfig> {
        let frontend = match (self.tls_cert, self.tls_key) {
            (Some(cert), Some(key)) => Some(frontend_acceptor(&cert, &key)?),
            _ => None,
        };
        let upstream = self
            .upstream_tls
            .then(|| upstream_connector(self.upstream_tls_insecure, self.upstream_ca.as_deref()))
            .transpose()?;
        Ok(TlsConfig { frontend, upstream })
    }
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
        #[command(flatten)]
        tls: TlsArgs,
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
        #[command(flatten)]
        tls: TlsArgs,
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
    let (listen, upstream, scenarios, trace, tls) = match Cli::parse().command {
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
            tls,
        } => (
            listen,
            upstream,
            scenario
                .iter()
                .map(Scenario::load)
                .collect::<Result<Vec<_>>>()?,
            trace,
            tls,
        ),
        Command::Replay {
            trace,
            listen,
            upstream,
            output_trace,
            tls,
        } => (
            listen,
            upstream,
            pgfault_trace::replay(trace)?,
            output_trace,
            tls,
        ),
    };
    let tls = tls.into_config()?;
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
            tls,
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
