use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::broadcast;
use tracing::{error, info, warn};

#[derive(Clone, Debug)]
pub struct HerdrClient {
    socket_path: PathBuf,
    event_tx: broadcast::Sender<Value>,
}

impl HerdrClient {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        let (event_tx, _) = broadcast::channel(1024);
        Self {
            socket_path: socket_path.into(),
            event_tx,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.event_tx.subscribe()
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let mut stream = UnixStream::connect(&self.socket_path)
            .await
            .map_err(|e| format!("Failed to connect to herdr socket at {:?}: {}", self.socket_path, e))?;

        let req_id = format!("bridge-{}", uuid_or_timestamp());
        let payload = json!({
            "id": req_id,
            "method": method,
            "params": params,
        });

        let mut raw = serde_json::to_vec(&payload).map_err(|e| e.to_string())?;
        raw.push(b'\n');

        stream
            .write_all(&raw)
            .await
            .map_err(|e| format!("Failed to write to herdr socket: {}", e))?;

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .map_err(|e| format!("Failed to read response from herdr: {}", e))?;

        let res: Value = serde_json::from_str(&line)
            .map_err(|e| format!("Failed to parse herdr response {}: {}", line, e))?;

        if let Some(err) = res.get("error") {
            return Err(format!("Herdr error response: {}", err));
        }

        Ok(res.get("result").cloned().unwrap_or(Value::Null))
    }

    pub async fn snapshot(&self) -> Result<Value, String> {
        self.call("session.snapshot", json!({})).await
    }

    pub async fn read_pane(&self, pane_id: &str, lines: Option<u32>) -> Result<String, String> {
        let params = json!({
            "pane_id": pane_id,
            "source": "visible",
            "format": "ansi",
            "strip_ansi": false,
            "lines": lines,
        });

        let res = self.call("pane.read", params).await?;
        if let Some(text) = res.pointer("/read/text").and_then(|v| v.as_str()) {
            Ok(text.to_string())
        } else {
            Ok(String::new())
        }
    }

    pub async fn send_input(&self, pane_id: &str, text: Option<&str>, keys: Option<Vec<String>>) -> Result<Value, String> {
        let mut params = json!({
            "pane_id": pane_id
        });
        if let Some(t) = text {
            params["text"] = Value::String(t.to_string());
        }
        if let Some(k) = keys {
            params["keys"] = json!(k);
        }
        self.call("pane.send_input", params).await
    }

    pub async fn resize_pane(&self, pane_id: &str, cols: u32, rows: u32) -> Result<Value, String> {
        let params = json!({
            "pane_id": pane_id,
            "cols": cols,
            "rows": rows
        });
        self.call("pane.resize", params).await
    }

    pub fn start_event_listener(self: Arc<Self>) {
        tokio::spawn(async move {
            loop {
                info!("Connecting event subscriber to herdr socket at {:?}", self.socket_path);
                match UnixStream::connect(&self.socket_path).await {
                    Ok(mut stream) => {
                        let sub_payload = json!({
                            "id": "bridge-sub-all",
                            "method": "events.subscribe",
                            "params": {
                                "subscriptions": [
                                    { "type": "workspace.created" },
                                    { "type": "workspace.updated" },
                                    { "type": "workspace.focused" },
                                    { "type": "workspace.closed" },
                                    { "type": "tab.created" },
                                    { "type": "tab.focused" },
                                    { "type": "tab.closed" },
                                    { "type": "pane.created" },
                                    { "type": "pane.focused" },
                                    { "type": "pane.closed" },
                                    { "type": "pane.updated" },
                                    { "type": "pane.agent_detected" },
                                    { "type": "layout.updated" }
                                ]
                            }
                        });

                        let mut raw = match serde_json::to_vec(&sub_payload) {
                            Ok(r) => r,
                            Err(e) => {
                                error!("Failed to serialize subscribe request: {}", e);
                                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                                continue;
                            }
                        };
                        raw.push(b'\n');

                        if let Err(e) = stream.write_all(&raw).await {
                            error!("Failed to send subscribe request: {}", e);
                            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                            continue;
                        }

                        let (reader, writer) = stream.into_split();
                        let mut lines = BufReader::new(reader).lines();
                        let _keep_writer_alive = writer;

                        info!("Successfully subscribed to Herdr runtime events");
                        while let Ok(Some(line)) = lines.next_line().await {
                            if line.trim().is_empty() {
                                continue;
                            }
                            match serde_json::from_str::<Value>(&line) {
                                Ok(val) => {
                                    let _ = self.event_tx.send(val);
                                }
                                Err(e) => {
                                    warn!("Received unparsable JSON event: {} - {}", e, line);
                                }
                            }
                        }
                        warn!("Herdr event stream closed. Reconnecting in 2 seconds...");
                    }
                    Err(e) => {
                        error!("Failed to connect to herdr socket for events: {}. Retrying in 3s...", e);
                    }
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            }
        });
    }
}

fn uuid_or_timestamp() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
