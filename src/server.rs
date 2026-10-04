use crate::herdr::HerdrClient;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::StatusCode,
    response::{Html, IntoResponse, Json},
    routing::{get, post},
    Router,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::warn;

const INDEX_HTML: &str = include_str!("index.html");

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
        .route("/", get(index_handler))
        .route("/app", get(index_handler))
        .route("/health", get(health_check))
        .route("/api/snapshot", get(get_snapshot))
        .route("/api/pane/{id}/input", post(post_pane_input))
        .route("/api/pane/{id}/resize", post(post_pane_resize))
        .route("/ws/session", get(ws_session_handler))
        .route("/ws/pane/{id}", get(ws_pane_handler))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn index_handler() -> Html<&'static str> {
    Html(INDEX_HTML)
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

#[derive(Deserialize)]
struct PaneResizeBody {
    cols: u32,
    rows: u32,
}

async fn post_pane_resize(
    State(state): State<AppState>,
    Path(pane_id): Path<String>,
    Json(body): Json<PaneResizeBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    match state.herdr.resize_pane(&pane_id, body.cols, body.rows).await {
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

    // Send initial snapshot upon connection
    if let Ok(snapshot) = state.herdr.snapshot().await {
        let msg = json!({
            "type": "snapshot",
            "data": snapshot
        });
        if let Ok(text) = serde_json::to_string(&msg) {
            let _ = sender.send(Message::Text(text.into())).await;
        }
    }

    let mut send_task = tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    let msg = json!({
                        "type": "event",
                        "data": event
                    });
                    if let Ok(text) = serde_json::to_string(&msg) {
                        if sender.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!("Session WS client lagged by {} events", n);
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    break;
                }
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

async fn ws_pane_handler(
    ws: WebSocketUpgrade,
    Path(pane_id): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws_pane(socket, pane_id, state))
}

async fn handle_ws_pane(socket: WebSocket, pane_id: String, state: AppState) {
    let (mut sender, mut receiver) = socket.split();
    let herdr = state.herdr.clone();
    let target_pane = pane_id.clone();

    // Send initial visible screen content
    if let Ok(content) = herdr.read_pane(&target_pane, None).await {
        let _ = sender.send(Message::Text(content.into())).await;
    }

    // Stream updates to the client
    let target_pane_for_send = target_pane.clone();
    let herdr_for_send = herdr.clone();
    let mut event_rx = herdr.subscribe();

    let mut send_task = tokio::spawn(async move {
        let mut last_content = String::new();
        let mut interval = tokio::time::interval(Duration::from_millis(150));

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if let Ok(content) = herdr_for_send.read_pane(&target_pane_for_send, None).await {
                        if content != last_content && !content.is_empty() {
                            last_content = content.clone();
                            if sender.send(Message::Text(content.into())).await.is_err() {
                                break;
                            }
                        }
                    }
                }
                Ok(event) = event_rx.recv() => {
                    let ev_pane = event.pointer("/event/pane_id")
                        .or_else(|| event.get("pane_id"))
                        .and_then(|v| v.as_str());
                    if ev_pane == Some(&target_pane_for_send) {
                        if let Ok(content) = herdr_for_send.read_pane(&target_pane_for_send, None).await {
                            if content != last_content && !content.is_empty() {
                                last_content = content.clone();
                                if sender.send(Message::Text(content.into())).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
    });

    let target_pane_for_recv = target_pane.clone();
    let herdr_for_recv = herdr.clone();

    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            match msg {
                Message::Text(text) => {
                    // Try parsing as control JSON or treat as raw input
                    if let Ok(val) = serde_json::from_str::<Value>(&text) {
                        let action = val.get("type").and_then(|v| v.as_str());
                        match action {
                            Some("input") => {
                                let input_text = val.get("text").and_then(|v| v.as_str());
                                let keys = val.get("keys").and_then(|v| v.as_array()).map(|arr| {
                                    arr.iter().filter_map(|s| s.as_str().map(String::from)).collect()
                                });
                                let _ = herdr_for_recv.send_input(&target_pane_for_recv, input_text, keys).await;
                            }
                            Some("resize") => {
                                let cols = val.get("cols").and_then(|v| v.as_u64()).unwrap_or(80) as u32;
                                let rows = val.get("rows").and_then(|v| v.as_u64()).unwrap_or(24) as u32;
                                let _ = herdr_for_recv.resize_pane(&target_pane_for_recv, cols, rows).await;
                            }
                            _ => {
                                // Raw string input
                                let _ = herdr_for_recv.send_input(&target_pane_for_recv, Some(&text), None).await;
                            }
                        }
                    } else {
                        // Raw text input sent from terminal
                        let _ = herdr_for_recv.send_input(&target_pane_for_recv, Some(&text), None).await;
                    }
                }
                Message::Binary(bin) => {
                    if let Ok(text) = String::from_utf8(bin.to_vec()) {
                        let _ = herdr_for_recv.send_input(&target_pane_for_recv, Some(&text), None).await;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    tokio::select! {
        _ = (&mut send_task) => recv_task.abort(),
        _ = (&mut recv_task) => send_task.abort(),
    }
}
