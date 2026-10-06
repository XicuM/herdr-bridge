mod herdr;
mod server;

use clap::Parser;
use herdr::HerdrClient;
use server::{create_router, AppState};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser, Debug)]
#[command(name = "herdr-bridge", about = "Mobile companion bridge daemon for Herdr")]
struct Args {
    /// Path to Herdr API Unix domain socket
    #[arg(long, env = "HERDR_SOCKET", default_value_os_t = default_socket_path())]
    socket: PathBuf,

    /// IP address to bind to: your Tailscale IP (start-bridge.sh finds it). There is no auth, so the
    /// default is this machine only; 0.0.0.0 would expose every pane to the whole network.
    #[arg(long, env = "HERDR_BRIDGE_BIND", default_value = "127.0.0.1")]
    bind: String,

    /// Port to listen on
    #[arg(short, long, env = "HERDR_BRIDGE_PORT", default_value_t = 7788)]
    port: u16,
}

fn default_socket_path() -> PathBuf {
    if let Ok(config_dir) = std::env::var("HERDR_CONFIG_DIR") {
        PathBuf::from(config_dir).join("herdr.sock")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".config/herdr/herdr.sock")
    } else {
        PathBuf::from(".config/herdr/herdr.sock")
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "herdr_bridge=info,tower_http=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let args = Args::parse();

    info!("Starting Herdr Mobile Bridge v{}", env!("CARGO_PKG_VERSION"));
    info!("Targeting Herdr Unix Socket at {:?}", args.socket);

    let herdr = Arc::new(HerdrClient::new(args.socket));

    // Start background event listener
    herdr.clone().start_event_listener();

    let state = AppState {
        herdr: herdr.clone(),
        snapshots: server::start_snapshot_poller(herdr.clone()),
    };

    let app = create_router(state);

    // An IPv6 bind needs no brackets this way.
    let addr = SocketAddr::new(args.bind.parse::<IpAddr>()?, args.port);
    info!("Herdr Mobile Bridge listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
