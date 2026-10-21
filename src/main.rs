mod herdr;
mod monitors;
mod server;
mod token;
mod uploads;
mod usage;
pub mod vault;

use clap::{Parser, Subcommand};
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

    /// IP address to bind to. Defaults to 0.0.0.0 so the bridge answers on localhost and Tailscale alike.
    #[arg(long, env = "HERDR_BRIDGE_BIND", default_value = "0.0.0.0")]
    bind: String,

    /// Port to listen on
    #[arg(short, long, env = "HERDR_BRIDGE_PORT", default_value_t = 7788)]
    port: u16,

    /// The file holding the token the app must send, made on first start if missing.
    #[arg(long, env = "HERDR_BRIDGE_TOKEN_FILE", default_value_os_t = default_token_path())]
    token_file: PathBuf,

    /// Print the token (making it if needed) and exit, to type it into the app.
    #[arg(long)]
    print_token: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Things agents ask of the Obsidian app, through its Herdr plugin.
    Obsidian {
        #[command(subcommand)]
        action: ObsidianAction,
    },
}

#[derive(Subcommand, Debug)]
enum ObsidianAction {
    /// Show a note in Obsidian: `obsidian open notes/Plan.md#Goals --line 12`.
    Open {
        /// A path from the current directory, with an optional `#heading`.
        target: String,
        #[arg(long)]
        line: Option<u64>,
    },
}

/// Asks the running bridge to show a note, with a bare HTTP request: the one call isn't worth a client crate.
async fn obsidian_open(args: &Args, target: &str, line: Option<u64>) -> Result<(), Box<dyn std::error::Error>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (path, heading) = match target.split_once('#') {
        Some((p, h)) => (p, Some(h)),
        None => (target, None),
    };
    let file = std::env::current_dir()?.join(path);
    let body = serde_json::json!({ "file": file, "heading": heading, "line": line }).to_string();
    let token = token::load_or_create(&args.token_file)?;
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", args.port)).await.map_err(|e| format!("herdr-bridge is not running on port {}: {e}", args.port))?;
    let request = format!(
        "POST /s/herdr-obsidian/api/obsidian/open HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await?;
    let mut response = String::new();
    stream.read_to_string(&mut response).await?;
    let (head, reply) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
    if head.starts_with("HTTP/1.1 200") {
        return Ok(());
    }
    Err(reply.trim().to_string().into())
}

fn default_token_path() -> PathBuf {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    config.join("herdr-bridge/token")
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
    if let Some(Command::Obsidian { action: ObsidianAction::Open { target, line } }) = &args.command {
        return obsidian_open(&args, target, *line).await;
    }
    let token = token::load_or_create(&args.token_file)?;
    if args.print_token {
        println!("{token}");
        return Ok(());
    }

    info!("Starting Herdr Mobile Bridge v{}", env!("CARGO_PKG_VERSION"));
    info!("Targeting Herdr Unix Socket at {:?}", args.socket);
    info!("The app must send the token in {:?} (herdr-bridge --print-token shows it)", args.token_file);

    let herdr = Arc::new(HerdrClient::new(args.socket));

    // Start background event listener
    herdr.clone().start_event_listener();

    let state = AppState {
        herdr: herdr.clone(),
        snapshots: server::start_snapshot_poller(herdr.clone()),
        opens: tokio::sync::broadcast::channel(16).0,
    };

    let app = create_router(state, token);

    // An IPv6 bind needs no brackets this way.
    let addr = SocketAddr::new(args.bind.parse::<IpAddr>()?, args.port);
    info!("Herdr Mobile Bridge listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
