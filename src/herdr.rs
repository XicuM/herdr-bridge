use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

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

    /// Opens a connection to herdr and writes one request to it.
    async fn send(&self, id: &str, method: &str, params: Value) -> Result<UnixStream, String> {
        let mut stream = UnixStream::connect(&self.socket_path)
            .await
            .map_err(|e| format!("Failed to connect to herdr socket at {:?}: {}", self.socket_path, e))?;
        let mut raw = serde_json::to_vec(&json!({ "id": id, "method": method, "params": params }))
            .map_err(|e| e.to_string())?;
        raw.push(b'\n');
        stream
            .write_all(&raw)
            .await
            .map_err(|e| format!("Failed to write to herdr socket: {}", e))?;
        Ok(stream)
    }

    /// One request and its response. A herdr that doesn't answer fails the call instead of hanging it.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let req_id = format!(
            "bridge-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis()
        );
        let line = tokio::time::timeout(Duration::from_secs(10), async {
            let stream = self.send(&req_id, method, params).await?;
            let mut line = String::new();
            BufReader::new(stream)
                .read_line(&mut line)
                .await
                .map_err(|e| format!("Failed to read response from herdr: {}", e))?;
            Ok::<_, String>(line)
        })
        .await
        .map_err(|_| format!("herdr did not answer {method} within 10 s"))??;

        let res: Value = serde_json::from_str(&line).map_err(|e| {
            debug!("Unparsable herdr response: {}", line);
            format!("Failed to parse herdr response: {}", e)
        })?;

        if let Some(err) = res.get("error") {
            return Err(format!("Herdr error response: {}", err));
        }

        Ok(res.get("result").cloned().unwrap_or(Value::Null))
    }

    /// `session.snapshot`, with a `git_branch` added to each workspace (Herdr's API doesn't expose it).
    pub async fn snapshot(&self) -> Result<Value, String> {
        let mut res = self.call("session.snapshot", json!({})).await?;
        let snap = if res.get("snapshot").is_some() { &mut res["snapshot"] } else { &mut res };
        let panes = snap["panes"].as_array().cloned().unwrap_or_default();
        let Some(workspaces) = snap["workspaces"].as_array_mut() else { return Ok(res) };

        // A workspace's directory: its worktree checkout, else the focused pane of its active tab.
        let dirs: Vec<Option<String>> = workspaces
            .iter()
            .map(|ws| {
                if let Some(path) = ws["worktree"]["checkout_path"].as_str() {
                    return Some(path.to_string());
                }
                let tab_panes: Vec<&Value> = panes.iter().filter(|p| p["tab_id"] == ws["active_tab_id"]).collect();
                let pane = tab_panes.iter().find(|p| p["focused"] == true).or(tab_panes.first())?;
                pane["foreground_cwd"].as_str().or(pane["cwd"].as_str()).map(String::from)
            })
            .collect();

        let branches = futures_util::future::join_all(dirs.into_iter().map(|dir| async move {
            let out = Command::new("git")
                .args(["-C", dir.as_deref()?, "branch", "--show-current"])
                .output()
                .await
                .ok()?;
            let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
            (out.status.success() && !branch.is_empty()).then_some(branch)
        }))
        .await;

        for (ws, branch) in workspaces.iter_mut().zip(branches) {
            if let Some(branch) = branch {
                ws["git_branch"] = Value::String(branch);
            }
        }
        Ok(res)
    }

    pub fn start_event_listener(self: Arc<Self>) {
        tokio::spawn(async move {
            loop {
                info!("Connecting event subscriber to herdr socket at {:?}", self.socket_path);
                let subscriptions: Vec<Value> = [
                    "workspace.created", "workspace.updated", "workspace.focused", "workspace.closed",
                    "tab.created", "tab.focused", "tab.closed",
                    "pane.created", "pane.focused", "pane.closed", "pane.updated", "pane.agent_detected",
                    "layout.updated",
                ]
                .iter()
                .map(|t| json!({ "type": t }))
                .collect();
                match self.send("bridge-sub-all", "events.subscribe", json!({ "subscriptions": subscriptions })).await {
                    Ok(stream) => {
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
                                    warn!("Received unparsable JSON event: {}", e);
                                    debug!("Unparsable event: {}", line);
                                }
                            }
                        }
                        warn!("Herdr event stream closed. Reconnecting in 3 seconds...");
                    }
                    Err(e) => {
                        error!("Failed to subscribe to herdr events: {}. Retrying in 3s...", e);
                    }
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        });
    }
}
