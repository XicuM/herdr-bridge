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

/// Where the shared SSH connections' sockets go: a directory only this user can enter. ssh joins whatever
/// socket is at that path, so one in a shared directory like `/tmp` (as `%C` is predictable) could be
/// planted by another user to see and answer every command, keystrokes included. `XDG_RUNTIME_DIR` is
/// such a directory where it is set (systemd); elsewhere (launchd) `~/.ssh`, made 0700 if missing.
fn control_dir() -> PathBuf {
    static DIR: std::sync::LazyLock<PathBuf> = std::sync::LazyLock::new(|| {
        if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
            return PathBuf::from(dir);
        }
        let dir = PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".ssh");
        use std::os::unix::fs::DirBuilderExt;
        let _ = std::fs::DirBuilder::new().mode(0o700).create(&dir);
        dir
    });
    DIR.clone()
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
            return error;
        };
        // herdr prints `Error: Custom { kind: Other, error: "<message>" }`.
        let text = String::from_utf8_lossy(&out.stderr);
        let text = text.trim();
        if text.is_empty() {
            return error;
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
        let dir = control_dir();
        // The remote shell parses the command line: every argument is quoted. A non-interactive login
        // may lack `~/.local/bin`, where herdr installs itself.
        let quoted: Vec<String> = [program].iter().chain(args).map(|a| format!("'{}'", a.replace('\'', r"'\''"))).collect();
        let mut cmd = Command::new("ssh");
        cmd.args(["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=8", "-o", "ServerAliveInterval=15"])
            .args(["-o", "ControlMaster=auto", "-o", "ControlPersist=600"])
            .arg("-o")
            .arg(format!("ControlPath={}/herdr-bridge-%C", dir.display()))
            .arg(target)
            .arg(format!("PATH=\"$HOME/.local/bin:$PATH\" exec {}", quoted.join(" ")));
        cmd
    }

    /// For the logs: the SSH target it reaches, or "this machine".
    pub fn name(&self) -> &str {
        self.ssh.as_deref().unwrap_or("this machine")
    }

    /// Whether this machine is reached over SSH rather than being this one.
    pub fn is_remote(&self) -> bool {
        self.ssh.is_some()
    }

    /// Saves an uploaded image where this machine's agents run and answers its absolute path: here
    /// in the uploads dir, or there over SSH (which streams the bytes into the same dir). `name`
    /// comes from [crate::uploads::file_name], safe characters only, and is checked again.
    pub async fn save_upload(&self, name: &str, bytes: &[u8]) -> Result<String, String> {
        if name.is_empty()
            || name.contains("..")
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return Err("Invalid upload name".into());
        }
        if self.ssh.is_none() {
            let dir = crate::uploads::dir();
            let path = dir.join(name);
            tokio::fs::write(&path, bytes).await.map_err(|e| format!("Could not save the image: {e}"))?;
            use std::os::unix::fs::PermissionsExt;
            let _ = tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await;
            crate::uploads::prune(dir);
            return Ok(path.to_string_lossy().into_owned());
        }
        // The remote shell parses the command line, and `command` quotes every argument; the names
        // this bridge makes carry no newlines, so `ls` lines are file names. The script prints the
        // path it saved, since the remote `$HOME` is only known there.
        let script = format!(
            "d=\"$HOME/.cache/herdr-bridge/uploads\"; mkdir -p \"$d\" && cat > \"$d/{name}\" && chmod 600 \"$d/{name}\" && cd \"$d\" && ls -t | tail -n +101 | xargs -r rm -f && echo \"$d/{name}\""
        );
        let mut child = self
            .command("sh", &["-c", &script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("Failed to reach {} over SSH: {e}", self.name()))?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let wrote = tokio::time::timeout(Duration::from_secs(60), async {
            stdin.write_all(bytes).await?;
            drop(stdin);
            let out = child.wait_with_output().await?;
            if !out.status.success() {
                return Err::<Vec<u8>, std::io::Error>(std::io::Error::other(
                    String::from_utf8_lossy(&out.stderr).trim().lines().last().unwrap_or("ssh failed").to_string(),
                ));
            }
            Ok(out.stdout)
        })
        .await
        .map_err(|_| "Timed out saving the image over SSH".to_string())?
        .map_err(|e| e.to_string())?;
        let path = String::from_utf8_lossy(&wrote);
        let path = path.trim();
        if path.is_empty() {
            return Err(format!("{} saved nothing", self.name()));
        }
        Ok(path.into())
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.event_tx.subscribe()
    }

    /// Opens a connection to herdr and writes one request to it.
    async fn send(&self, id: &str, method: &str, params: Value) -> Result<Conn, String> {
        let (reader, mut writer, ssh): Conn = if self.ssh.is_none() {
            let (reader, writer) = UnixStream::connect(&self.socket_path)
                .await
                .map_err(|e| match e.kind() {
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                        format!("herdr isn't running on this machine (nothing at {:?}): start it", self.socket_path)
                    }
                    _ => format!("Failed to connect to herdr socket at {:?}: {}", self.socket_path, e),
                })?
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
        // The usage files are this machine's: a remote one's agents get none rather than ours.
        // So are the background monitors, read from this machine's `/proc`.
        if self.ssh.is_none() {
            if let Some(agents) = snap["agents"].as_array_mut() {
                let names: Vec<String> =
                    agents.iter().map(|a| a["agent"].as_str().or(a["name"].as_str()).unwrap_or("").to_string()).collect();
                let usages = tokio::task::spawn_blocking(move || names.iter().map(|n| crate::usage::usage_of(n)).collect::<Vec<_>>())
                    .await
                    .unwrap_or_default();
                for (agent, usage) in agents.iter_mut().zip(usages) {
                    if let Some(usage) = usage {
                        agent["usage"] = usage;
                    }
                }
            }
            crate::monitors::mark(self, snap).await;
        }
        let panes = snap["panes"].as_array().cloned().unwrap_or_default();
        let Some(workspaces) = snap["workspaces"].as_array_mut() else { return Ok(res) };

        // A workspace's directory: the focused pane of its active tab, else its worktree checkout.
        let dirs: Vec<Option<String>> = workspaces
            .iter()
            .map(|ws| {
                let tab_panes: Vec<&Value> = panes.iter().filter(|p| p["tab_id"] == ws["active_tab_id"]).collect();
                let pane = tab_panes.iter().find(|p| p["focused"] == true).or(tab_panes.first());
                pane.and_then(|p| p["foreground_cwd"].as_str().or(p["cwd"].as_str()))
                    .map(String::from)
                    .or_else(|| ws["worktree"]["checkout_path"].as_str().map(String::from))
            })
            .collect();

        // If a non-linked workspace inherited a stale worktree whose repo does not match the
        // pane's directory, drop it so the workspace is not tied to an unrelated repository.
        for (ws, dir) in workspaces.iter_mut().zip(&dirs) {
            if let Some(dir) = dir
                && let Some(wt) = ws.get_mut("worktree")
                && wt["is_linked_worktree"] != true
            {
                let root = wt["repo_root"].as_str().or(wt["checkout_path"].as_str()).unwrap_or("");
                if !root.is_empty() && dir != root && !dir.starts_with(&format!("{root}/")) {
                    ws.as_object_mut().map(|obj| obj.remove("worktree"));
                }
            }
        }

        // One command for every workspace, one line each (empty without a branch): on a remote machine it
        // is one SSH round trip.
        const SCRIPT: &str = r#"for d; do b=; [ -n "$d" ] && b=$(git -C "$d" branch --show-current 2>/dev/null); printf '%s\n' "$b"; done"#;
        let mut args = vec!["-c", SCRIPT, "sh"];
        args.extend(dirs.iter().map(|d| d.as_deref().unwrap_or("")));
        let mut cmd = self.command("sh", &args);
        cmd.kill_on_drop(true);
        let Ok(Ok(out)) = tokio::time::timeout(Duration::from_secs(10), cmd.output()).await else { return Ok(res) };
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
