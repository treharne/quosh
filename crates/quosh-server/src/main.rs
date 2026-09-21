use anyhow::{Context, Result};
use clap::Parser;
use quosh_server::cert::{self, CertChain};
use quosh_server::devices::DeviceStore;
use quosh_server::enroll::NonceStore;
use quosh_server::helper::{self, Daemon};
use quosh_server::transport::handle_incoming;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};
use wtransport::{Endpoint, ServerConfig};

#[derive(Parser, Debug)]
#[command(name = "quosh-server")]
struct Args {
    /// UDP bind address for WebTransport (HTTP/3).
    #[arg(long, default_value = "0.0.0.0:443")]
    bind: String,
    /// Unix socket the SSH helper talks to.
    #[arg(long, default_value = "/run/quosh/quosh.sock")]
    socket: PathBuf,
    /// State directory (TLS material).
    #[arg(long, default_value = "/var/lib/quosh")]
    data_dir: PathBuf,
    /// Number of certificates in the rotation window.
    #[arg(long, default_value_t = cert::DEFAULT_CHAIN)]
    cert_chain: usize,
    /// WebAuthn Relying Party ID.
    #[arg(long, default_value = "quosh.jtcs.dev")]
    rp_id: String,
    /// Expected WebAuthn origin.
    #[arg(long, default_value = "https://quosh.jtcs.dev")]
    origin: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let port = args
        .bind
        .rsplit(':')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(quosh_proto::DEFAULT_PORT);

    if let Some(parent) = args.socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::create_dir_all(&args.data_dir)?;
    let _ = std::fs::remove_file(&args.socket);

    let chain = CertChain::load(&args.data_dir.join("tls"), args.cert_chain)?;
    info!("cert sha256 {}", hex::encode(chain.current_hash()?));

    let bind: std::net::SocketAddr = args.bind.parse().context("bind addr")?;
    let config = ServerConfig::builder()
        .with_bind_address(bind)
        .with_custom_tls(chain.tls_config()?)
        .build();

    let endpoint = Endpoint::server(config).context("WebTransport endpoint")?;
    info!("WebTransport on {bind}");

    let listener = tokio::net::UnixListener::bind(&args.socket).context("unix bind")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&args.socket, std::fs::Permissions::from_mode(0o666))?;
    }
    info!("helper socket {}", args.socket.display());

    let daemon = Arc::new(Daemon {
        sessions: Arc::new(tokio::sync::Mutex::new(Default::default())),
        chain: chain.clone(),
        devices: DeviceStore::load(&args.data_dir.join("devices.json"))?,
        nonces: NonceStore::new(),
        port,
        rp_id: args.rp_id.clone(),
        origin: args.origin.clone(),
    });
    let d2 = daemon.clone();
    tokio::spawn(async move {
        d2.serve_unix(listener).await;
    });

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    loop {
        tokio::select! {
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
            incoming = endpoint.accept() => {
                let sessions = daemon.sessions.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_incoming(incoming, sessions).await {
                        warn!("session: {e:#}");
                    }
                });
            }
        }
    }
    info!("shutting down");
    let ids: Vec<_> = daemon.sessions.lock().await.keys().copied().collect();
    for id in ids {
        helper::destroy(&daemon.sessions, id).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    Ok(())
}
