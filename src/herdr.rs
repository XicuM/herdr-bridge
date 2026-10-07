use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

#[derive(Clone, Debug)]
pub struct HerdrClient {
    socket_path: PathBuf,
    /// The SSH target of a machine saved in herdr (`herdr machine list`), reached through
    /// `herdr remote-api-bridge` there, as herdr's own `--machine` does; none for this machine.
    ssh: Option<String>,
    /// Its id in herdr, to ask herdr why it can't be reached.
    machine: Option<String>,
    /// The herdr session there, when not the default one.
    session: Option<String>,
    event_tx: broadcast::Sender<Value>,
}

/// One connection to herdr's API: its reader, its writer, and the `ssh` carrying it, if any.
type Conn = (Box<dyn AsyncRead + Unpin + Send>, Box<dyn AsyncWrite + Unpin + Send>, Option<Child>);

impl HerdrClient {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        let (event_tx, _) = broadcast::channel(1024);
        Self {
            socket_path: socket_path.into(),
            ssh: None,
            machine: None,
            session: None,
            event_tx,
        }
    }

    pub fn remote(id: String, ssh: String, session: String) -> Self {
        let session = (session != "default").then_some(session);
        Self { ssh: Some(ssh), machine: Some(id), session, ..Self::new("") }
    }

    /// Why herdr can't reach this machine either, as `herdr --machine` puts it (e.g. "remote Herdr does
    /// not support machine API forwarding; update Herdr on this machine"), or just "Error" when herdr
    /// can. This machine's own failure is the bridge's [error], since herdr has nothing to add.
    pub async fn machine_error(&self, error: String) -> String {
        let Some(id) = &self.machine else { return error };
        let Ok(out) = Command::new("herdr").args(["--machine", id, "api", "snapshot"]).output().await else {
            return "Error".into();
        };
        // herdr prints `Error: Custom { kind: Other, error: "<message>" }`.
        let text = String::from_utf8_lossy(&out.stderr);
        let text = text.trim();
        if text.is_empty() {
            return "Error".into();
        }
        let message = text.split_once("error: \"").and_then(|(_, m)| m.rsplit_once('"')).map_or(text, |(m, _)| m);
        // herdr's message leads with "machine '<label>'", maybe with " (session <name>)", and ": ".
        match message.strip_prefix("machine '").and_then(|m| m.split_once(": ")) {
            Some((_, m)) => m.to_string(),
            None => message.to_string(),
        }
    }

    /// [program] run on herdr's machine: here, or there over SSH. The SSH connection is shared between
    /// commands, so after the first each one costs little; BatchMode, since nobody is there to type a
    /// password (`herdr machine reconnect` in a terminal authenticates a machine that needs one).
    pub fn command(&self, program: &str, args: &[&str]) -> Command {
        // herdr's own commands go to its session there.
        let session = match &self.session {
            Some(session) if program == "herdr" => vec!["--session", session.as_str()],
            _ => vec![],
        };
        let args: Vec<&str> = session.into_iter().chain(args.iter().copied()).collect();
        let args = &args[..];
        let Some(target) = &self.ssh else {
            let mut cmd = Command::new(program);
            cmd.args(args);
            return cmd;
        };
        let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
        // The remote shell parses the command line: every argument is quoted. A non-interactive login
        // may lack `~/.local/bin`, where herdr installs itself.
        let quoted: Vec<String> = [program].iter().chain(args).map(|a| format!("'{}'", a.replace('\'', r"'\''"))).collect();
        let mut cmd = Command::new("ssh");
        cmd.args(["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=8", "-o", "ServerAliveInterval=15"])
            .args(["-o", "ControlMaster=auto", "-o", "ControlPersist=600"])
            .arg("-o")
            .arg(format!("ControlPath={dir}/herdr-bridge-%C"))
            .arg(target)
            .arg(format!("PATH=\"$HOME/.local/bin:$PATH\" exec {}", quoted.join(" ")));
        cmd
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.event_tx.subscribe()
    }

    /// Opens a connection to herdr and writes one request to it.
    async fn send(&self, id: &str, method: &str, params: Value) -> Result<Conn, String> {
        let (reader, mut writer, ssh): Conn = if self.ssh.is_none() {
            let (reader, writer) = UnixStream::connect(&self.socket_path)
                .await
                .map_err(|e| format!("Failed to connect to herdr socket at {:?}: {}", self.socket_path, e))?
                .into_split();
            (Box::new(reader), Box::new(writer), None)
        } else {
            let mut child = self
                .command("herdr", &["remote-api-bridge"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .map_err(|e| format!("Failed to run ssh: {e}"))?;
            (Box::new(child.stdout.take().expect("piped stdout")), Box::new(child.stdin.take().expect("piped stdin")), Some(child))
        };
        let mut raw = serde_json::to_vec(&json!({ "id": id, "method": method, "params": params }))
            .map_err(|e| e.to_string())?;
        raw.push(b'\n');
        writer.write_all(&raw).await.map_err(|e| format!("Failed to write to herdr socket: {}", e))?;
        Ok((reader, writer, ssh))
    }

    /// Why [ssh] ended without an answer, e.g. "ssh: Could not resolve hostname …".
    async fn ssh_error(ssh: Option<Child>) -> String {
        let mut text = String::new();
        if let Some(mut stderr) = ssh.and_then(|mut c| c.stderr.take()) {
            let _ = stderr.read_to_string(&mut text).await;
        }
        let text = text.trim();
        if text.is_empty() { "herdr closed the connection".into() } else { text.lines().last().unwrap_or(text).to_string() }
    }

    /// One request and its response. A herdr that doesn't answer fails the call instead of hanging it.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let req_id = format!(
            "bridge-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis()
        );
        let line = tokio::time::timeout(Duration::from_secs(10), async {
            let (reader, _writer, ssh) = self.send(&req_id, method, params).await?;
            let mut line = String::new();
            BufReader::new(reader)
                .read_line(&mut line)
                .await
                .map_err(|e| format!("Failed to read response from herdr: {}", e))?;
            if line.is_empty() {
                return Err(Self::ssh_error(ssh).await);
            }
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
        // The app reads neither the layouts, whose scroll offsets move as panes scroll, nor a raw title
        // that has a stripped one, whose spinner turns every second or so while an agent works. Without
        // them the snapshot changes, and is pushed to the phones, only when something they show does.
        if let Some(snap) = snap.as_object_mut() {
            snap.remove("layouts");
        }
        for list in ["panes", "agents"] {
            for item in snap[list].as_array_mut().into_iter().flatten() {
                if let Some(item) = item.as_object_mut().filter(|i| i.contains_key("terminal_title_stripped")) {
                    item.remove("terminal_title");
                }
            }
        }
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

        // One command for every workspace, one line each (empty without a branch): on a remote machine it
        // is one SSH round trip.
        const SCRIPT: &str = r#"for d; do b=; [ -n "$d" ] && b=$(git -C "$d" branch --show-current 2>/dev/null); printf '%s\n' "$b"; done"#;
        let mut args = vec!["-c", SCRIPT, "sh"];
        args.extend(dirs.iter().map(|d| d.as_deref().unwrap_or("")));
        let mut cmd = self.command("sh", &args);
        cmd.kill_on_drop(true);
        let Ok(Ok(out)) = tokio::time::timeout(Duration::from_secs(10), cmd.output()).await else { return Ok(res) };
        if let Some(agents) = snap["agents"].as_array_mut() {
            for agent in agents {
                let name = agent["agent"].as_str().or(agent["name"].as_str()).unwrap_or("");
                if let Some(usage) = get_agent_usage(name) {
                    agent["usage"] = usage;
                }
            }
        }
        for (ws, branch) in workspaces.iter_mut().zip(String::from_utf8_lossy(&out.stdout).lines()) {
            if !branch.trim().is_empty() {
                ws["git_branch"] = Value::String(branch.trim().to_string());
            }
        }
        Ok(res)
    }

    pub fn start_event_listener(self: Arc<Self>) {
        tokio::spawn(async move {
            // An unreachable machine is retried ever less often, up to once a minute.
            let mut wait = 3;
            loop {
                match &self.ssh {
                    Some(target) => info!("Connecting event subscriber to herdr on {target}"),
                    None => info!("Connecting event subscriber to herdr socket at {:?}", self.socket_path),
                }
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
                    Ok((reader, writer, ssh)) => {
                        let mut lines = BufReader::new(reader).lines();
                        let _keep_alive = (writer, ssh);

                        info!("Successfully subscribed to Herdr runtime events");
                        while let Ok(Some(line)) = lines.next_line().await {
                            if line.trim().is_empty() {
                                continue;
                            }
                            wait = 3;
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
                        warn!("Herdr event stream closed. Reconnecting in {wait} s...");
                    }
                    Err(e) => {
                        error!("Failed to subscribe to herdr events: {}. Retrying in {wait} s...", e);
                    }
                }
                tokio::time::sleep(Duration::from_secs(wait)).await;
                wait = (wait * 2).min(60);
            }
        });
    }
}

/// Usage and quota metrics for an agent, read from local state or config files.
fn get_agent_usage(agent_name: &str) -> Option<Value> {
    let agent_id = match agent_name {
        "agy" => "gemini",
        other => other,
    };
    let state_dir = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|h| PathBuf::from(h).join(".local/state"))
                .unwrap_or_else(|_| PathBuf::from("/tmp"))
        });
    let usage_file = state_dir.join("omarchy/agents/usage").join(format!("{agent_id}.json"));
    if let Ok(content) = std::fs::read_to_string(&usage_file) {
        if let Ok(json) = serde_json::from_str::<Value>(&content) {
            return Some(json!({
                "tier_label": json.get("tierLabel"),
                "limits": json.get("limits").unwrap_or(&Value::Array(vec![])),
                "today_tokens": json.get("todayTotalTokens"),
                "today_prompts": json.get("todayPrompts"),
            }));
        }
    }

    if agent_id == "claude" {
        if let Ok(home) = std::env::var("HOME") {
            let cred_path = PathBuf::from(home).join(".claude/.credentials.json");
            if let Ok(content) = std::fs::read_to_string(&cred_path) {
                if let Ok(json) = serde_json::from_str::<Value>(&content) {
                    let mut tier = json
                        .get("claudeAiOauth")
                        .and_then(|o| o.get("subscriptionType"))
                        .and_then(|s| s.as_str())
                        .map(|s| {
                            let mut c = s.chars();
                            match c.next() {
                                None => String::new(),
                                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                            }
                        });
                    if let Some(rate_tier) = json.get("claudeAiOauth").and_then(|o| o.get("rateLimitTier")).and_then(|s| s.as_str()) {
                        if rate_tier.contains("max_") {
                            tier = Some(format!("Max {}", &rate_tier[4..]));
                        }
                    }
                    return Some(json!({
                        "tier_label": tier,
                        "limits": [],
                    }));
                }
            }
        }
    }
    None
}
