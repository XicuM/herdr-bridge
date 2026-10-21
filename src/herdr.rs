use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

/// (ssh_target, dir) to its `branch<TAB>ahead<TAB>behind` line and when it was queried. The snapshot
/// polls every 1.5 s, but git branches change rarely; cached for 10 s so remote machines avoid SSH
/// subprocess storms.
static BRANCHES: LazyLock<Mutex<HashMap<(Option<String>, String), (Instant, String)>>> =
    LazyLock::new(Default::default);
const BRANCHES_TTL: Duration = Duration::from_secs(10);

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

    pub fn session(name: String) -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        let socket_path = home.join(format!(".config/herdr/sessions/{name}/herdr.sock"));
        Self {
            socket_path,
            session: Some(name),
            ..Self::new("")
        }
    }

    /// Why herdr can't reach this machine either, as `herdr machine status` puts it (e.g. "remote Herdr
    /// server is stopped or incompatible; run …"), or [error] when herdr can. This machine's own failure
    /// is the bridge's [error], since herdr has nothing to add.
    pub async fn machine_error(&self, error: String) -> String {
        let Some(id) = &self.machine else { return error };
        let Ok(out) = Command::new("herdr").args(["machine", "status", id, "--json"]).output().await else {
            return error;
        };
        let rows: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap_or_default();
        let Some(row) = rows.first() else { return error };
        match (row["status"].as_str(), row["error"].as_str()) {
            // SSH wants a password or MFA, which only a terminal can give.
            (Some("auth required"), Some(e)) => {
                format!("{e}\nRun `herdr machine reconnect {}` on the bridge's machine.", row["label"].as_str().unwrap_or(id))
            }
            (_, Some(e)) => e.to_string(),
            _ => error,
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

    /// Spawns the local headless herdr server if it is not already running.
    /// Rate-limited so failure doesn't spawn a storm of processes.
    async fn try_spawn_server(&self) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static LAST_SPAWN: AtomicU64 = AtomicU64::new(0);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let last = LAST_SPAWN.load(Ordering::Relaxed);
        if now.saturating_sub(last) < 15 {
            return;
        }
        if LAST_SPAWN.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err() {
            return;
        }

        info!("Local herdr socket unavailable; spawning headless `herdr server`...");
        let mut cmd = std::process::Command::new("herdr");
        if let Some(session) = &self.session {
            cmd.args(["--session", session.as_str()]);
        }
        cmd.arg("server")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        match cmd.spawn() {
            Ok(_) => {
                // Poll for socket readiness up to 2 seconds
                for _ in 0..20 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    if UnixStream::connect(&self.socket_path).await.is_ok() {
                        info!("Herdr server started and listening at {:?}", self.socket_path);
                        return;
                    }
                }
                warn!("Spawned `herdr server`, but socket {:?} was not ready in 2 s", self.socket_path);
            }
            Err(e) => {
                warn!("Failed to spawn `herdr server`: {e}");
            }
        }
    }

    /// Opens a connection to herdr and writes one request to it.
    async fn send(&self, id: &str, method: &str, params: Value) -> Result<Conn, String> {
        let (reader, mut writer, ssh): Conn = if self.ssh.is_none() {
            let stream = match UnixStream::connect(&self.socket_path).await {
                Ok(s) => s,
                Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) => {
                    self.try_spawn_server().await;
                    UnixStream::connect(&self.socket_path).await.map_err(|e| match e.kind() {
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                            format!("herdr isn't running on this machine (nothing at {:?}): start it", self.socket_path)
                        }
                        _ => format!("Failed to connect to herdr socket at {:?}: {}", self.socket_path, e),
                    })?
                }
                Err(e) => return Err(format!("Failed to connect to herdr socket at {:?}: {}", self.socket_path, e)),
            };
            let (reader, writer) = stream.into_split();
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
        // Git checkout/removal and repository hooks can outlast an ordinary RPC.
        let timeout = if method.starts_with("worktree.") { 120 } else { 10 };
        let req_id = format!(
            "bridge-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis()
        );
        let line = tokio::time::timeout(Duration::from_secs(timeout), async {
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
        .map_err(|_| format!("herdr did not answer {method} within {timeout} s; the operation may still finish"))??;

        let res: Value = serde_json::from_str(&line).map_err(|e| {
            debug!("Unparsable herdr response: {}", line);
            format!("Failed to parse herdr response: {}", e)
        })?;

        if let Some(err) = res.get("error") {
            return Err(format!("Herdr error response: {}", err));
        }

        Ok(res.get("result").cloned().unwrap_or(Value::Null))
    }

    /// `session.snapshot`, with a `git_branch` added to each workspace (Herdr's API doesn't expose it),
    /// and `git_ahead`/`git_behind` when that branch has an upstream.
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
        // Usage is this machine's account, shown for a remote machine's agents too: it's the same account
        // everywhere. The background monitors are read from this machine's `/proc`, so only its own.
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
        if self.ssh.is_none() {
            crate::monitors::mark(self, snap).await;
        }
        let panes = snap["panes"].as_array().cloned().unwrap_or_default();
        let Some(workspaces) = snap["workspaces"].as_array_mut() else { return Ok(res) };

        // A linked worktree's directory is its checkout, even when a shell changes directory; any other
        // workspace's is the focused pane of its active tab, since herdr can keep a stale main checkout.
        let dirs: Vec<Option<String>> = workspaces
            .iter()
            .map(|ws| {
                let tab_panes: Vec<&Value> = panes.iter().filter(|p| p["tab_id"] == ws["active_tab_id"]).collect();
                let pane = tab_panes.iter().find(|p| p["focused"] == true).or(tab_panes.first());
                let checkout = ws["worktree"]["checkout_path"].as_str();
                let cwd = pane.and_then(|p| p["foreground_cwd"].as_str().or(p["cwd"].as_str()));
                if ws["worktree"]["is_linked_worktree"] == true { checkout.or(cwd) } else { cwd.or(checkout) }
                    .map(String::from)
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

        let needs_fetch = dirs.iter().any(|d| match d {
            Some(dir) if !dir.is_empty() => {
                let cache = BRANCHES.lock().ok();
                cache.and_then(|c| c.get(&(self.ssh.clone(), dir.clone())).cloned())
                    .is_none_or(|(at, _)| at.elapsed() >= BRANCHES_TTL)
            }
            _ => false,
        });

        if needs_fetch && dirs.iter().any(|d| d.as_deref().is_some_and(|s| !s.is_empty())) {
            // One command for every workspace, one line each: its branch, then the commits it is ahead of and
            // behind its upstream as last fetched (both left out without one); empty outside a branch. On a
            // remote machine it is one SSH round trip.
            const SCRIPT: &str = r#"for d; do b=; c=; if [ -n "$d" ]; then b=$(git -C "$d" branch --show-current 2>/dev/null); [ -n "$b" ] && c=$(git -C "$d" rev-list --left-right --count 'HEAD...@{upstream}' 2>/dev/null); fi; printf '%s\t%s\n' "$b" "$c"; done"#;
            let mut args = vec!["-c", SCRIPT, "sh"];
            args.extend(dirs.iter().map(|d| d.as_deref().unwrap_or("")));
            let mut cmd = self.command("sh", &args);
            cmd.kill_on_drop(true);
            if let Ok(Ok(out)) = tokio::time::timeout(Duration::from_secs(10), cmd.output()).await {
                let stdout_str = String::from_utf8_lossy(&out.stdout);
                let lines: Vec<&str> = stdout_str.lines().collect();
                if let Ok(mut cache) = BRANCHES.lock() {
                    let now = Instant::now();
                    for (dir, line) in dirs.iter().zip(lines) {
                        if let Some(dir) = dir.as_deref().filter(|d| !d.is_empty()) {
                            cache.insert((self.ssh.clone(), dir.to_string()), (now, line.to_string()));
                        }
                    }
                }
            }
        }

        if let Ok(cache) = BRANCHES.lock() {
            for (ws, dir) in workspaces.iter_mut().zip(dirs.iter()) {
                if let Some(dir) = dir.as_deref().filter(|d| !d.is_empty())
                    && let Some((_, line)) = cache.get(&(self.ssh.clone(), dir.to_string()))
                {
                    let mut fields = line.split('\t').map(str::trim);
                    let branch = fields.next().unwrap_or("");
                    if branch.is_empty() {
                        continue;
                    }
                    ws["git_branch"] = Value::String(branch.to_string());
                    for key in ["git_ahead", "git_behind"] {
                        if let Some(n) = fields.next().and_then(|n| n.parse::<u64>().ok()) {
                            ws[key] = json!(n);
                        }
                    }
                }
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
                    "workspace.created", "workspace.updated", "workspace.renamed", "workspace.moved",
                    "workspace.reordered", "workspace.focused", "workspace.closed",
                    "worktree.created", "worktree.opened", "worktree.removed",
                    "tab.created", "tab.renamed", "tab.moved", "tab.focused", "tab.closed",
                    "pane.created", "pane.focused", "pane.closed", "pane.updated", "pane.moved", "pane.exited",
                    "pane.agent_detected", "layout.updated",
                ]
                .iter()
                .map(|t| json!({ "type": t }))
                .collect();
                match self.send("bridge-sub-all", "events.subscribe", json!({ "subscriptions": subscriptions })).await {
                    Ok((reader, writer, ssh)) => {
                        let mut lines = BufReader::new(reader).lines();
                        // herdr answers `subscription_started` first; anything else is why it didn't subscribe.
                        match lines.next_line().await {
                            Ok(Some(line)) if line.contains("subscription_started") => {
                                let _keep_alive = (writer, ssh);
                                info!("Subscribed to herdr's events");
                                wait = 3;
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
                                warn!("Herdr event stream closed. Reconnecting in {wait} s...");
                            }
                            Ok(Some(line)) => error!("herdr refused the event subscription: {line}. Retrying in {wait} s..."),
                            _ => error!("Failed to subscribe to herdr events: {}. Retrying in {wait} s...", Self::ssh_error(ssh).await),
                        }
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
