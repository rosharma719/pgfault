use anyhow::{Context, Result};
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
    /// Run an external command against a proxy for one shot, then optionally
    /// check ground truth with a direct query -- the "does what the client
    /// saw match what's actually true" loop this tool exists for, automated.
    Probe {
        #[arg(long, default_value = "127.0.0.1:0")]
        listen: String,
        #[arg(long, default_value = "127.0.0.1:5432")]
        upstream: String,
        #[arg(long)]
        scenario: Vec<PathBuf>,
        #[arg(long)]
        trace: Option<PathBuf>,
        #[command(flatten)]
        tls: TlsArgs,
        /// SQL to run directly against the upstream (bypassing the proxy)
        /// after the command exits, to check what actually happened.
        #[arg(long, requires = "verify_dsn")]
        verify_sql: Option<String>,
        /// Direct, non-proxied connection string for --verify-sql.
        #[arg(long, requires = "verify_sql")]
        verify_dsn: Option<String>,
        /// Command to run. Any argument containing the literal text
        /// {{PGFAULT_PORT}} has it replaced with the proxy's listen port.
        /// Put this after `--`.
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
}
async fn verify_query(dsn: &str, sql: &str) -> Result<Vec<String>> {
    let (client, connection) = tokio_postgres::connect(dsn, tokio_postgres::NoTls).await?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            tracing::warn!(error=%e, "verify connection ended");
        }
    });
    let messages = client.simple_query(sql).await?;
    Ok(messages
        .into_iter()
        .filter_map(|m| match m {
            tokio_postgres::SimpleQueryMessage::Row(row) => Some(
                (0..row.len())
                    .map(|i| row.get(i).unwrap_or("NULL").to_string())
                    .collect::<Vec<_>>()
                    .join(" | "),
            ),
            _ => None,
        })
        .collect())
}
struct ProbeArgs {
    listen: String,
    upstream: String,
    scenario: Vec<PathBuf>,
    trace: Option<PathBuf>,
    tls: TlsArgs,
    verify_sql: Option<String>,
    verify_dsn: Option<String>,
    command: Vec<String>,
}
async fn probe(args: ProbeArgs) -> Result<i32> {
    let ProbeArgs {
        listen,
        upstream,
        scenario,
        trace,
        tls,
        verify_sql,
        verify_dsn,
        command,
    } = args;
    let scenarios = scenario
        .iter()
        .map(Scenario::load)
        .collect::<Result<Vec<_>>>()?;
    let tls = tls.into_config()?;
    let path = trace.unwrap_or_else(default_trace_path);
    let recorder = Recorder::create(&path)?;
    let listener = bind(&listen).await?;
    let port = listener.local_addr()?.port();
    tracing::info!(listen=%listener.local_addr()?, %upstream, trace=%path.display(), "pgfault probe listening");
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let config = Config {
        upstream,
        scenarios,
        trace: recorder,
        tls,
    };
    let server = tokio::spawn(async move {
        serve(listener, config, async {
            let _ = shutdown_rx.await;
        })
        .await
    });
    let substituted: Vec<String> = command
        .iter()
        .map(|a| a.replace("{{PGFAULT_PORT}}", &port.to_string()))
        .collect();
    let (program, args) = substituted.split_first().context("empty command")?;
    let output = tokio::process::Command::new(program)
        .args(args)
        .output()
        .await
        .with_context(|| format!("running {program}"))?;
    let _ = shutdown_tx.send(());
    server.await??;
    println!("$ {}", substituted.join(" "));
    println!(
        "exit code: {}",
        output
            .status
            .code()
            .map_or("signaled".to_string(), |c| c.to_string())
    );
    if !output.stdout.is_empty() {
        println!(
            "--- stdout ---\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
    if !output.stderr.is_empty() {
        println!(
            "--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    if let (Some(sql), Some(dsn)) = (verify_sql, verify_dsn) {
        println!("=== ground truth: {sql} ===");
        match verify_query(&dsn, &sql).await {
            Ok(rows) if rows.is_empty() => println!("(no rows)"),
            Ok(rows) => rows.iter().for_each(|r| println!("{r}")),
            Err(e) => println!("verify query failed: {e:#}"),
        }
    }
    Ok(output.status.code().unwrap_or(1))
}
fn default_trace_path() -> PathBuf {
    PathBuf::from(format!(
        ".pgfault/traces/{}-{}.jsonl",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        std::process::id()
    ))
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
        Command::Probe {
            listen,
            upstream,
            scenario,
            trace,
            tls,
            verify_sql,
            verify_dsn,
            command,
        } => {
            let code = probe(ProbeArgs {
                listen,
                upstream,
                scenario,
                trace,
                tls,
                verify_sql,
                verify_dsn,
                command,
            })
            .await?;
            std::process::exit(code);
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
    let path = trace.unwrap_or_else(default_trace_path);
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
