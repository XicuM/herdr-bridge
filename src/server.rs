use crate::herdr::HerdrClient;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{delete, get, post},
    Router,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::warn;

#[derive(Clone)]
pub struct AppState {
    pub herdr: Arc<HerdrClient>,
}

pub fn create_router(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .route("/health", get(health_check))
        .route("/api/snapshot", get(get_snapshot))
        .route("/api/pane/{id}/input", post(post_pane_input))
        .route("/api/tab", post(post_tab_create))
        .route("/api/workspace", post(post_workspace_create))
        .route("/api/workspace/{id}", delete(delete_workspace))
        .route("/api/workspace/{id}/rename", post(post_workspace_rename))
        .route("/api/worktree", get(get_worktree_list).post(post_worktree_create))
        .route("/api/worktree/open", post(post_worktree_open))
        .route("/ws/session", get(ws_session_handler))
        .route("/ws/term/{id}", get(ws_term_handler))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn health_check() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "service": "herdr-bridge",
        "version": env!("CARGO_PKG_VERSION")
    }))
}

async fn get_snapshot(State(state): State<AppState>) -> Result<Json<Value>, (StatusCode, String)> {
    match state.herdr.snapshot().await {
        Ok(snapshot) => Ok(Json(snapshot)),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

#[derive(Deserialize)]
struct PaneInputBody {
    text: Option<String>,
    keys: Option<Vec<String>>,
}

async fn post_pane_input(
    State(state): State<AppState>,
    Path(pane_id): Path<String>,
    Json(body): Json<PaneInputBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    match state
        .herdr
        .send_input(&pane_id, body.text.as_deref(), body.keys)
        .await
    {
        Ok(res) => Ok(Json(res)),
        Err(e) => Err((StatusCode::BAD_REQUEST, e)),
    }
}

/// Body is passed straight through as `tab.create` params (`workspace_id`, `label`, `cwd`, `focus`).
async fn post_tab_create(
    State(state): State<AppState>,
    Json(params): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    match state.herdr.call("tab.create", params).await {
        Ok(res) => Ok(Json(res)),
        Err(e) => Err((StatusCode::BAD_REQUEST, e)),
    }
}

/// Body is passed straight through as `workspace.create` params (`label`, `cwd`, `focus`).
async fn post_workspace_create(
    State(state): State<AppState>,
    Json(params): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    match state.herdr.call("workspace.create", params).await {
        Ok(res) => Ok(Json(res)),
        Err(e) => Err((StatusCode::BAD_REQUEST, e)),
    }
}

#[derive(Deserialize)]
struct WorkspaceDeleteQuery {
    remove_worktree: Option<bool>,
    force: Option<bool>,
}

async fn delete_workspace(
    State(state): State<AppState>,
    Path(workspace_id): Path<String>,
    Query(query): Query<WorkspaceDeleteQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let method = if query.remove_worktree.unwrap_or(false) {
        "worktree.remove"
    } else {
        "workspace.close"
    };
    let mut params = json!({ "workspace_id": workspace_id });
    if let Some(force) = query.force {
        params["force"] = json!(force);
    }
    match state.herdr.call(method, params).await {
        Ok(res) => Ok(Json(res)),
        Err(e) => Err((StatusCode::BAD_REQUEST, e)),
    }
}

#[derive(Deserialize)]
struct WorkspaceRenameBody {
    label: String,
}

async fn post_workspace_rename(
    State(state): State<AppState>,
    Path(workspace_id): Path<String>,
    Json(body): Json<WorkspaceRenameBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let params = json!({
        "workspace_id": workspace_id,
        "label": body.label,
    });
    match state.herdr.call("workspace.rename", params).await {
        Ok(res) => Ok(Json(res)),
        Err(e) => Err((StatusCode::BAD_REQUEST, e)),
    }
}

#[derive(Deserialize)]
struct WorktreeListQuery {
    workspace_id: Option<String>,
}

async fn get_worktree_list(
    State(state): State<AppState>,
    Query(query): Query<WorktreeListQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut params = json!({});
    if let Some(ws_id) = query.workspace_id {
        params["workspace_id"] = json!(ws_id);
    }
    match state.herdr.call("worktree.list", params).await {
        Ok(res) => Ok(Json(res)),
        Err(e) => Err((StatusCode::BAD_REQUEST, e)),
    }
}

async fn post_worktree_create(
    State(state): State<AppState>,
    Json(params): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    match state.herdr.call("worktree.create", params).await {
        Ok(res) => Ok(Json(res)),
        Err(e) => Err((StatusCode::BAD_REQUEST, e)),
    }
}

async fn post_worktree_open(
    State(state): State<AppState>,
    Json(params): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    match state.herdr.call("worktree.open", params).await {
        Ok(res) => Ok(Json(res)),
        Err(e) => Err((StatusCode::BAD_REQUEST, e)),
    }
}

async fn ws_session_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_ws_session(socket, state))
}

async fn handle_ws_session(socket: WebSocket, state: AppState) {
    let (mut sender, mut receiver) = socket.split();
    let mut event_rx = state.herdr.subscribe();
    let herdr = state.herdr.clone();

    let mut send_task = tokio::spawn(async move {
        // Herdr's global event subscription carries no agent status changes, so the snapshot is also
        // polled and pushed whenever it changed: that is how the app sees agents start waiting or
        // finish (and raises its alerts). The first tick sends the initial snapshot.
        let mut last = String::new();
        let mut tick = tokio::time::interval(Duration::from_millis(1500));
        loop {
            let text = tokio::select! {
                _ = tick.tick() => {
                    let Ok(snapshot) = herdr.snapshot().await else { continue };
                    let text = json!({ "type": "snapshot", "data": snapshot }).to_string();
                    if text == last {
                        continue;
                    }
                    last = text.clone();
                    text
                }
                event = event_rx.recv() => match event {
                    Ok(event) => json!({ "type": "event", "data": event }).to_string(),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!("Session WS client lagged by {} events", n);
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
            };
            if sender.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            match msg {
                Message::Close(_) => break,
                Message::Ping(_) => {}
                _ => {}
            }
        }
    });

    tokio::select! {
        _ = (&mut send_task) => recv_task.abort(),
        _ = (&mut recv_task) => send_task.abort(),
    }
}

#[derive(Deserialize)]
struct TermSize {
    cols: u16,
    rows: u16,
}

async fn ws_term_handler(
    ws: WebSocketUpgrade,
    Path(pane_id): Path<String>,
    Query(size): Query<TermSize>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        if let Err(e) = handle_ws_term(socket, pane_id, size).await {
            warn!("Terminal session ended with error: {}", e);
        }
    })
}

/// Streams one pane through `herdr terminal session control`, which sizes the pane to the viewer
/// and emits server-rendered ANSI frames. Binary frames carry terminal output/input; text frames
/// carry `{"cols","rows"}` resizes.
async fn handle_ws_term(socket: WebSocket, pane_id: String, size: TermSize) -> std::io::Result<()> {
    let mut child = Command::new("herdr")
        .args(["terminal", "session", "control", &pane_id, "--takeover"])
        .args(["--cols", &size.cols.to_string(), "--rows", &size.rows.to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut frames = BufReader::new(child.stdout.take().expect("piped stdout")).lines();

    let (mut sender, mut receiver) = socket.split();
    loop {
        tokio::select! {
            line = frames.next_line() => {
                let Some(line) = line? else { break };
                let Ok(frame) = serde_json::from_str::<Value>(&line) else { continue };
                if frame["type"] == "terminal.closed" {
                    break;
                }
                if let Some(bytes) = frame["bytes"].as_str().and_then(|b| BASE64.decode(b).ok()) {
                    if sender.send(Message::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                }
            }
            msg = receiver.next() => {
                let cmd = match msg {
                    Some(Ok(Message::Binary(bytes))) => json!({ "type": "terminal.input", "bytes": BASE64.encode(&bytes) }),
                    Some(Ok(Message::Text(text))) => match serde_json::from_str::<Value>(&text) {
                        // Typed control commands (e.g. `terminal.scroll`) go to herdr as-is.
                        Ok(cmd) if cmd.get("type").is_some() => cmd,
                        Ok(v) => match serde_json::from_value::<TermSize>(v) {
                            Ok(s) => json!({ "type": "terminal.resize", "cols": s.cols, "rows": s.rows }),
                            Err(_) => continue,
                        },
                        Err(_) => continue,
                    },
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    _ => continue,
                };
                stdin.write_all(format!("{cmd}\n").as_bytes()).await?;
            }
        }
    }

    // Hand the pane's size back to the desktop layout.
    let _ = stdin.write_all(b"{\"type\":\"terminal.release\"}\n").await;
    drop(stdin);
    if tokio::time::timeout(Duration::from_secs(2), child.wait()).await.is_err() {
        child.kill().await?;
    }
    Ok(())
}
