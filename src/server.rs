use crate::herdr::HerdrClient;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, Request, State,
    },
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Json, Response},
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
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tower_http::trace::TraceLayer;
use tracing::warn;

#[derive(Clone)]
pub struct AppState {
    pub herdr: Arc<HerdrClient>,
}

pub fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health_check))
        .route("/api/snapshot", get(get_snapshot))
        .route("/api/tab", post(post_tab_create))
        .route("/api/tab/{id}", delete(delete_tab))
        .route("/api/tab/{id}/rename", post(post_tab_rename))
        .route("/api/pane/{id}", delete(delete_pane))
        .route("/api/workspace", post(post_workspace_create))
        .route("/api/workspace/move", post(post_workspace_move))
        .route("/api/workspace/{id}", delete(delete_workspace))
        .route("/api/workspace/{id}/rename", post(post_workspace_rename))
        .route("/api/worktree", get(get_worktree_list).post(post_worktree_create))
        .route("/api/worktree/open", post(post_worktree_open))
        .route("/ws/session", get(ws_session_handler))
        .route("/ws/term/{id}", get(ws_term_handler))
        .layer(middleware::from_fn(reject_browsers))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Browsers let any web page open a WebSocket to any address, and send its `Origin` with it (and with
/// every cross-site or non-GET request); the app's Dart client sends none. So a request that carries one
/// comes from a web page, which could otherwise type into a terminal: refuse it.
async fn reject_browsers(req: Request, next: Next) -> Response {
    if req.headers().contains_key(header::ORIGIN) {
        warn!("Refused a browser request to {} (Origin {:?})", req.uri(), req.headers()[header::ORIGIN]);
        return (StatusCode::FORBIDDEN, "herdr-bridge only accepts the Herdr Mobile app").into_response();
    }
    next.run(req).await
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

type ApiResult = Result<Json<Value>, (StatusCode, String)>;

/// One herdr call; its error comes back as a 400 with herdr's message.
async fn rpc(state: &AppState, method: &str, params: Value) -> ApiResult {
    state.herdr.call(method, params).await.map(Json).map_err(|e| (StatusCode::BAD_REQUEST, e))
}

/// Body is passed straight through as `tab.create` params (`workspace_id`, `label`, `cwd`, `focus`).
async fn post_tab_create(State(state): State<AppState>, Json(params): Json<Value>) -> ApiResult {
    rpc(&state, "tab.create", params).await
}

async fn delete_tab(State(state): State<AppState>, Path(tab_id): Path<String>) -> ApiResult {
    rpc(&state, "tab.close", json!({ "tab_id": tab_id })).await
}

async fn delete_pane(State(state): State<AppState>, Path(pane_id): Path<String>) -> ApiResult {
    rpc(&state, "pane.close", json!({ "pane_id": pane_id })).await
}

/// Body is passed straight through as `workspace.create` params (`label`, `cwd`, `focus`).
async fn post_workspace_create(State(state): State<AppState>, Json(params): Json<Value>) -> ApiResult {
    rpc(&state, "workspace.create", params).await
}

/// Body is passed straight through as `workspace.move_block` params (`workspace_ids`,
/// `before_workspace_id`, null for the end).
async fn post_workspace_move(State(state): State<AppState>, Json(params): Json<Value>) -> ApiResult {
    rpc(&state, "workspace.move_block", params).await
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
) -> ApiResult {
    let method = if query.remove_worktree.unwrap_or(false) { "worktree.remove" } else { "workspace.close" };
    let mut params = json!({ "workspace_id": workspace_id });
    if let Some(force) = query.force {
        params["force"] = json!(force);
    }
    rpc(&state, method, params).await
}

#[derive(Deserialize)]
struct RenameBody {
    label: String,
}

async fn post_tab_rename(
    State(state): State<AppState>,
    Path(tab_id): Path<String>,
    Json(body): Json<RenameBody>,
) -> ApiResult {
    rpc(&state, "tab.rename", json!({ "tab_id": tab_id, "label": body.label })).await
}

async fn post_workspace_rename(
    State(state): State<AppState>,
    Path(workspace_id): Path<String>,
    Json(body): Json<RenameBody>,
) -> ApiResult {
    rpc(&state, "workspace.rename", json!({ "workspace_id": workspace_id, "label": body.label })).await
}

#[derive(Deserialize)]
struct WorktreeListQuery {
    workspace_id: Option<String>,
}

async fn get_worktree_list(State(state): State<AppState>, Query(query): Query<WorktreeListQuery>) -> ApiResult {
    let mut params = json!({});
    if let Some(ws_id) = query.workspace_id {
        params["workspace_id"] = json!(ws_id);
    }
    rpc(&state, "worktree.list", params).await
}

async fn post_worktree_create(State(state): State<AppState>, Json(params): Json<Value>) -> ApiResult {
    rpc(&state, "worktree.create", params).await
}

async fn post_worktree_open(State(state): State<AppState>, Json(params): Json<Value>) -> ApiResult {
    rpc(&state, "worktree.open", params).await
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
        // A herdr event pushes the new snapshot right away. Herdr's global event subscription carries no
        // agent status changes, though, so the snapshot is also polled and pushed whenever it changed:
        // that is how the app sees agents start waiting or finish (and raises its alerts). The first tick
        // sends the initial snapshot.
        let mut last = String::new();
        let mut tick = tokio::time::interval(Duration::from_millis(1500));
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                event = event_rx.recv() => match event {
                    // A burst of events makes one snapshot.
                    Ok(_) | Err(RecvError::Lagged(_)) => {
                        while matches!(event_rx.try_recv(), Ok(_) | Err(TryRecvError::Lagged(_))) {}
                    }
                    Err(RecvError::Closed) => break,
                },
            }
            let Ok(snapshot) = herdr.snapshot().await else { continue };
            let text = json!({ "type": "snapshot", "data": snapshot }).to_string();
            if text == last {
                continue;
            }
            last = text.clone();
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
    State(state): State<AppState>,
    Path(pane_id): Path<String>,
    Query(size): Query<TermSize>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        // Focusing is what marks a pane seen in herdr (a `done` agent turns `idle`); the takeover alone
        // doesn't. The desktop follows the phone to this pane.
        if let Err(e) = state.herdr.call("pane.focus", json!({ "pane_id": pane_id })).await {
            warn!("Could not focus pane {}: {}", pane_id, e);
        }
        if let Err(e) = handle_ws_term(socket, pane_id, size).await {
            warn!("Terminal session ended with error: {}", e);
        }
    })
}

/// Streams one pane through `herdr terminal session control`, which sizes the pane to the viewer
/// and emits server-rendered ANSI frames. Binary frames carry terminal output/input; text frames
/// carry typed control commands (e.g. `terminal.scroll`), or else `{"cols","rows"}` resizes.
async fn handle_ws_term(socket: WebSocket, pane_id: String, size: TermSize) -> std::io::Result<()> {
    let mut child = Command::new("herdr")
        .args(["terminal", "session", "control", &pane_id, "--takeover"])
        .args(["--cols", &size.cols.to_string(), "--rows", &size.rows.to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut frames = BufReader::new(child.stdout.take().expect("piped stdout")).lines();

    let (mut sender, mut receiver) = socket.split();
    loop {
        tokio::select! {
            line = frames.next_line() => {
                // On a read error too, fall through to the release below.
                let Ok(Some(line)) = line else { break };
                let Ok(frame) = serde_json::from_str::<Value>(&line) else { continue };
                if frame["type"] == "terminal.closed" {
                    break;
                }
                if let Some(bytes) = frame["bytes"].as_str().and_then(|b| BASE64.decode(b).ok())
                    && sender.send(Message::Binary(bytes.into())).await.is_err()
                {
                    break;
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
                if stdin.write_all(format!("{cmd}\n").as_bytes()).await.is_err() {
                    break;
                }
            }
        }
    }

    // Hand the pane's size back to the desktop layout.
    let _ = stdin.write_all(b"{\"type\":\"terminal.release\"}\n").await;
    drop(stdin);
    if tokio::time::timeout(Duration::from_secs(2), child.wait()).await.is_err() {
        let _ = child.kill().await;
    }
    Ok(())
}
