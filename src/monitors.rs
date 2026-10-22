//! Whether a pane has background monitors running while herdr calls it `idle`.
//!
//! Herdr's `agent_status` is screen detection: a prompt box on screen reads as
//! `idle`, even while the agent's background monitors (Claude's `until … done`
//! watch loops, long `scons` runs, user-backgrounded jobs) are still working.
//! Everything running in a pane is a descendant of its shell, so one `/proc`
//! walk per snapshot finds them for every agent alike: anything left after
//! filtering out the agent itself and known wrappers (MCP servers, language
//! servers an editor integration keeps running for diagnostics/completion)
//! counts.
//!
//! Local machine only (like `usage`): over SSH the walk would have to run
//! there. Results are cached 10 s; pane→shell pids 60 s (ids are reused).

use crate::herdr::HerdrClient;
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Pane ids to monitoring state, and when it was computed: the snapshot polls
/// every 1.5 s, far too often for a `/proc` walk plus herdr calls.
static RESULTS: LazyLock<Mutex<HashMap<String, (Instant, bool)>>> = LazyLock::new(Default::default);
const RESULTS_TTL: Duration = Duration::from_secs(10);

/// Pane ids to their shell pid, from `herdr pane process-info`.
static SHELLS: LazyLock<Mutex<HashMap<String, (Instant, u32)>>> = LazyLock::new(Default::default);
const SHELLS_TTL: Duration = Duration::from_secs(60);

/// One process: its parent, its state (`RSDZTW`), and its command line.
struct Proc {
    ppid: u32,
    state: char,
    cmdline: String,
}

/// Marks each agent in `snap["agents"]` with `monitoring: true` when its pane
/// has background monitors running. Never fails the snapshot: on any error
/// the agents are left unmarked.
pub async fn mark(client: &HerdrClient, snap: &mut serde_json::Value) {
    let Some(agents) = snap["agents"].as_array() else { return };
    let panes: Vec<(String, String)> = agents
        .iter()
        .map(|a| {
            (
                a["pane_id"].as_str().unwrap_or_default().to_string(),
                a["agent"].as_str().or(a["name"].as_str()).unwrap_or_default().to_string(),
            )
        })
        .filter(|(pane, _)| !pane.is_empty())
        .collect();
    if panes.is_empty() {
        return;
    }
    let now = Instant::now();
    let mut fresh: HashMap<String, bool> = HashMap::new();
    if let Ok(results) = RESULTS.lock() {
        for (pane, _) in &panes {
            if let Some((at, monitoring)) = results.get(pane)
                && now.duration_since(*at) < RESULTS_TTL
            {
                fresh.insert(pane.clone(), *monitoring);
            }
        }
    }
    if fresh.len() == panes.len() {
        return apply(snap, &fresh);
    }
    let procs = tokio::task::spawn_blocking(read_procs).await.unwrap_or_default();
    if procs.is_empty() {
        return;
    }
    let mut computed = fresh;
    for (pane, agent) in &panes {
        if computed.contains_key(pane) {
            continue;
        }
        let monitoring = shell_pid(client, pane).await.is_some_and(|shell| has_monitors(&procs, shell, agent));
        computed.insert(pane.clone(), monitoring);
    }
    if let Ok(mut results) = RESULTS.lock() {
        for (pane, monitoring) in &computed {
            results.insert(pane.clone(), (now, *monitoring));
        }
    }
    apply(snap, &computed);
}

fn apply(snap: &mut serde_json::Value, monitoring: &HashMap<String, bool>) {
    let Some(agents) = snap["agents"].as_array_mut() else { return };
    for agent in agents {
        if let Some(pane) = agent["pane_id"].as_str()
            && let Some(monitoring) = monitoring.get(pane)
        {
            agent["monitoring"] = (*monitoring).into();
        }
    }
}

/// The pane's shell pid, cached: it never changes while the pane lives, but
/// herdr reuses a closed pane's id, so the entry expires.
async fn shell_pid(client: &HerdrClient, pane: &str) -> Option<u32> {
    if let Ok(shells) = SHELLS.lock()
        && let Some((at, pid)) = shells.get(pane)
        && at.elapsed() < SHELLS_TTL
    {
        return Some(*pid);
    }
    let mut cmd = client.command("herdr", &["pane", "process-info", "--pane", pane]);
    cmd.kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(10), cmd.output()).await.ok()?.ok()?;
    if !out.status.success() {
        return None;
    }
    let pid = serde_json::from_slice::<serde_json::Value>(&out.stdout)
        .ok()?
        .pointer("/result/process_info/shell_pid")?
        .as_u64()? as u32;
    if let Ok(mut shells) = SHELLS.lock() {
        shells.insert(pane.to_string(), (Instant::now(), pid));
    }
    Some(pid)
}

/// Any live descendant of `shell` that isn't the agent itself or a known
/// wrapper (MCP servers) is a background monitor.
fn has_monitors(procs: &HashMap<u32, Proc>, shell: u32, agent: &str) -> bool {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, proc_) in procs {
        children.entry(proc_.ppid).or_default().push(*pid);
    }
    let mut stack = vec![shell];
    let mut seen = HashSet::from([shell]);
    while let Some(pid) = stack.pop() {
        for child in children.get(&pid).into_iter().flatten() {
            if !seen.insert(*child) {
                continue;
            }
            let Some(proc_) = procs.get(child) else { continue };
            // Zombies are already reaped in effect; kernel threads have none.
            if proc_.state == 'Z' || proc_.cmdline.is_empty() {
                continue;
            }
            if is_wrapper(&proc_.cmdline, agent) {
                // Still descend: a monitor could hide behind a wrapper pid
                // only in theory, but the walk is cheap.
                stack.push(*child);
                continue;
            }
            return true;
        }
    }
    false
}

/// The agent binary itself, MCP servers (`… mcp …`), the JS runtimes they are
/// near-universally run with, and language servers: coding agents routinely
/// keep one running for diagnostics/completion, and it sits idle between
/// edits like an MCP server does. A node-based monitor is missed by this;
/// wrongly calling every MCP or LSP pane "monitoring" would be worse.
fn is_wrapper(cmdline: &str, agent: &str) -> bool {
    let argv0 = cmdline.split(' ').next().unwrap_or_default();
    let base = argv0.rsplit('/').next().unwrap_or_default().to_lowercase();
    if !agent.is_empty() && base == agent.to_lowercase() {
        return true;
    }
    if cmdline.to_lowercase().contains("mcp") {
        return true;
    }
    if matches!(base.as_str(), "node" | "npm" | "npx" | "bun" | "deno") {
        return true;
    }
    is_language_server(&base)
}

/// Common language-server binary names, by suffix where they follow one
/// (`*-language-server`, `*-langserver`, `*-lsp`, `*-ls`) and by name where
/// they don't.
fn is_language_server(base: &str) -> bool {
    if base.ends_with("-language-server") || base.ends_with("-langserver") || base.ends_with("-lsp") || base.ends_with("-ls")
    {
        return true;
    }
    matches!(
        base,
        "clangd" | "gopls" | "rust-analyzer" | "pylsp" | "solargraph" | "jdtls" | "omnisharp" | "intelephense" | "metals" | "sqls" | "zls"
    )
}

/// Every process from `/proc`: pid → parent, state, and command line.
fn read_procs() -> HashMap<u32, Proc> {
    let mut procs = HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else { return procs };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Ok(pid) = name.to_string_lossy().parse::<u32>() else { continue };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
        let Some((ppid, state)) = parse_stat(&stat) else { continue };
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline"))
            .map(|b| String::from_utf8_lossy(&b).replace('\0', " ").trim().to_string())
            .unwrap_or_default();
        procs.insert(pid, Proc { ppid, state, cmdline });
    }
    procs
}

/// `/proc/<pid>/stat` is `pid (comm) state ppid …`; the name can hold spaces
/// and parens, so it is cut at the last `)`.
fn parse_stat(stat: &str) -> Option<(u32, char)> {
    let after = stat.rsplit_once(") ")?.1;
    let mut fields = after.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let ppid = fields.next()?.parse().ok()?;
    Some((ppid, state))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc_(ppid: u32, state: char, cmdline: &str) -> Proc {
        Proc { ppid, state, cmdline: cmdline.into() }
    }

    fn table() -> HashMap<u32, Proc> {
        // shell(100) → claude(200) → {npm mcp(300), bash monitor(400) → scons(500)}.
        HashMap::from([
            (100, proc_(1, 'S', "/usr/bin/bash")),
            (200, proc_(100, 'S', "claude --resume abc")),
            (300, proc_(200, 'S', "npm exec @inkeep/open-knowledge@latest mcp")),
            (400, proc_(200, 'S', "/usr/bin/bash -c until grep -q done; do timeout 600 tail -f /dev/null; done")),
            (500, proc_(400, 'S', "/usr/bin/python /home/xicu/bin/scons cocotb TOP=tmu_top")),
        ])
    }

    #[test]
    fn finds_monitors_past_wrappers() {
        assert!(has_monitors(&table(), 100, "claude"));
    }

    #[test]
    fn wrappers_alone_are_not_monitors() {
        let mut procs = table();
        procs.remove(&400);
        procs.remove(&500);
        assert!(!has_monitors(&procs, 100, "claude"));
    }

    #[test]
    fn zombies_do_not_count() {
        let mut procs = table();
        procs.remove(&400);
        procs.remove(&500);
        procs.insert(600, proc_(200, 'Z', "[scons] <defunct>"));
        assert!(!has_monitors(&procs, 100, "claude"));
    }

    #[test]
    fn agent_name_matches_by_basename() {
        assert!(is_wrapper("/usr/bin/claude --resume abc", "claude"));
        assert!(is_wrapper("agy", "agy"));
        assert!(!is_wrapper("/usr/bin/bash -c scons", "claude"));
    }

    #[test]
    fn mcp_and_js_runtimes_are_wrappers() {
        assert!(is_wrapper("npm exec @inkeep/open-knowledge@latest mcp", "claude"));
        assert!(is_wrapper("node /home/xicu/Projects/overleaf-suggest/dist/index.js serve", "agy"));
        assert!(!is_wrapper("/usr/bin/python /home/xicu/bin/scons cocotb", "claude"));
        assert!(!is_wrapper("timeout 1500 scons cocotb TOP=tmu_top", "claude"));
    }

    #[test]
    fn language_servers_are_wrappers() {
        assert!(is_wrapper("clangd --background-index", "claude"));
        assert!(is_wrapper("/usr/bin/rust-analyzer", "claude"));
        assert!(is_wrapper("typescript-language-server --stdio", "claude"));
        assert!(is_wrapper("pyright-langserver --stdio", "claude"));
        assert!(is_wrapper("sourcekit-lsp", "claude"));
        assert!(!is_wrapper("/usr/bin/python /home/xicu/bin/scons cocotb", "claude"));
    }

    #[test]
    fn stat_names_with_parens_and_spaces() {
        assert_eq!(parse_stat("12 (my (odd) name) S 1 2 3"), Some((1, 'S')));
        assert_eq!(parse_stat("400 (bash) S 200 1"), Some((200, 'S')));
        assert_eq!(parse_stat("garbage"), None);
    }

    /// End to end on live `/proc`: a spawned `sleep` under this process must
    /// show up in the walk and read as a monitor, and be gone once reaped.
    /// Scoped to our own child pid, so parallel tests' children can't flake it.
    #[test]
    #[cfg(target_os = "linux")]
    fn live_proc_walk_finds_own_child() {
        let mut child = std::process::Command::new("sleep").arg("60").spawn().unwrap();
        let me = std::process::id();
        let is_ours = |procs: &HashMap<u32, Proc>| {
            procs.values().any(|p| p.ppid == me && p.cmdline.starts_with("sleep"))
        };
        let procs = read_procs();
        assert!(is_ours(&procs));
        assert!(has_monitors(&procs, me, "test-agent"));
        child.kill().unwrap();
        let _ = child.wait();
        assert!(!is_ours(&read_procs()));
    }
}
