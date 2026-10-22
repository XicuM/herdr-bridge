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
use tracing::{info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser, Debug)]
#[command(name = "herdr-bridge", about = "Lets other devices and apps reach your herdr agents")]
struct Args {
    /// Path to Herdr API Unix domain socket
    #[arg(long, env = "HERDR_SOCKET", default_value_os_t = default_socket_path())]
    socket: PathBuf,

    /// IP address to bind to. Defaults to this machine only: `tailscale serve` brings the tailnet to it, and
    /// 0.0.0.0 opens it to every network the machine is on, where the token crosses in clear text.
    #[arg(long, env = "HERDR_BRIDGE_BIND", default_value = "127.0.0.1")]
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

    info!("Starting herdr-bridge v{}", env!("CARGO_PKG_VERSION"));
    info!("Targeting Herdr Unix Socket at {:?}", args.socket);
    info!("The app must send the token in {:?} (herdr-bridge --print-token shows it)", args.token_file);

    let herdr = Arc::new(HerdrClient::new(args.socket));

    // Installed as a herdr plugin, the bridge follows the plugin, since herdr runs nothing when it changes: it
    // pauses while the plugin is disabled, and removes itself once herdr no longer lists it. Only a list herdr
    // answered counts, never herdr being down.
    let paused = std::env::var_os("HERDR_BRIDGE_PAUSED").is_some();
    if let Ok(plugin) = std::env::var("HERDR_BRIDGE_PLUGIN") {
        let herdr = herdr.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                let Ok(list) = herdr.call("plugin.list", serde_json::json!({})).await else { continue };
                let Some(plugins) = list["plugins"].as_array() else { continue };
                if let Some(p) = plugins.iter().find(|p| p["plugin_id"] == plugin.as_str()) {
                    if (p["enabled"] != false) == paused {
                        // Started again in place: a paused bridge holds no port and no connection, and the
                        // service, which would only restart an exited one, never notices.
                        info!("The {plugin} plugin was {}", if paused { "enabled; serving again" } else { "disabled; pausing" });
                        let mut argv = std::env::args_os();
                        let mut cmd = std::process::Command::new(argv.next().unwrap_or_default());
                        cmd.args(argv);
                        if paused { cmd.env_remove("HERDR_BRIDGE_PAUSED") } else { cmd.env("HERDR_BRIDGE_PAUSED", "1") };
                        use std::os::unix::process::CommandExt;
                        warn!("Could not restart herdr-bridge: {}", cmd.exec());
                    }
                    continue;
                }
                info!("The {plugin} plugin was uninstalled; removing the bridge");
                // It stops this service last, ending the bridge with it.
                let script = std::env::var("HOME").unwrap_or_default() + "/.local/lib/herdr-bridge/install.sh";
                match tokio::process::Command::new("bash").arg(&script).arg("--uninstall").status().await {
                    Ok(status) if status.success() => std::process::exit(0),
                    // Exiting would only have the service start it again: it keeps serving, and tries again.
                    result => warn!("{script} --uninstall failed: {result:?}"),
                }
            }
        });
    }
    if paused {
        info!("Paused while the herdr plugin is disabled");
        std::future::pending::<()>().await;
    }

    // Start background event listener
    herdr.clone().start_event_listener();

    let state = AppState {
        herdr: herdr.clone(),
        snapshots: server::start_snapshot_poller(herdr.clone()),
        opens: tokio::sync::broadcast::channel(16).0,
        vault: vault::Watcher::new(),
    };

    let app = create_router(state, token);

    // An IPv6 bind needs no brackets this way.
    let addr = SocketAddr::new(args.bind.parse::<IpAddr>()?, args.port);
    info!("herdr-bridge listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
