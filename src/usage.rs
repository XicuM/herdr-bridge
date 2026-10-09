//! What an agent has used today, for the app's usage sheet, with real live quota scraping
//! from the providers and local transcript stats.
//!
//! Every function here reads files or probes APIs: call [usage_of] from a blocking thread.

use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Each agent's usage, and when it was read: scanning today's transcripts is too much for every 1.5 s poll.
type Cache = HashMap<String, (Instant, Option<Value>)>;
static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Default::default);
const CACHE_TTL: Duration = Duration::from_secs(30);

/// Anthropic's usage endpoint answers 429 when asked every [CACHE_TTL]: when it was last asked, and its
/// last good answer, kept while it refuses.
type Live = (Option<Instant>, Option<(Option<String>, Vec<Value>)>);
static CLAUDE_LIVE: LazyLock<Mutex<Live>> = LazyLock::new(Default::default);
const CLAUDE_LIVE_TTL: Duration = Duration::from_secs(300);

/// Transcripts last written before this long ago hold nothing from today.
const RECENT: Duration = Duration::from_secs(26 * 3600);

/// The usage of the agent herdr calls [agent_name], or None when its files say nothing.
pub fn usage_of(agent_name: &str) -> Option<Value> {
    let name = agent_name.to_lowercase();
    // Its name goes into a file name: anything but a plain name gets none.
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return None;
    }
    if let Ok(cache) = CACHE.lock()
        && let Some((at, usage)) = cache.get(&name)
        && at.elapsed() < CACHE_TTL
    {
        return usage.clone();
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let home = home.as_deref()?;
    let usage = if name.contains("claude") || name.contains("anthropic") {
        claude(home)
    } else if name.contains("codex") || name.contains("openai") || name.contains("gpt") {
        codex(home)
    } else if name == "agy" || name.contains("antigravity") || name.contains("gemini") {
        antigravity(home)
    } else if name.contains("grok") {
        grok(home)
    } else if name.contains("opencode") {
        opencode(home)
    } else if name.contains("copilot") || name.contains("github") {
        copilot(home)
    } else if name.contains("cursor") {
        cursor(home)
    } else if name.contains("openrouter") {
        openrouter(home)
    } else if name.contains("deepseek") {
        deepseek(home)
    } else {
        None
    };
    if let Ok(mut cache) = CACHE.lock() {
        cache.insert(name, (Instant::now(), usage.clone()));
    }
    usage
}

/// Claude Code: live OAuth rate limits from Anthropic plus local transcript stats.
fn claude(home: &Path) -> Option<Value> {
    let today = today_utc();
    let (mut tokens, mut prompts, mut model, mut seen) = (0u64, 0u64, None::<(String, String)>, HashSet::new());
    for file in recent_files(&home.join(".claude/projects"), 4, "jsonl") {
        for line in lines(&file) {
            if !line.contains(&today) {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<Value>(&line) else { continue };
            let time = entry["timestamp"].as_str().unwrap_or_default();
            if !time.starts_with(&today) {
                continue;
            }
            let message = &entry["message"];
            match entry["type"].as_str() {
                // What was typed: a string, or blocks (text, a pasted image), not a tool's result.
                Some("user")
                    if entry["isMeta"] != true
                        && (message["content"].is_string()
                            || message["content"].as_array().is_some_and(|b| {
                                !b.is_empty() && b.iter().all(|b| b["type"] != "tool_result")
                            })) =>
                {
                    prompts += 1
                }
                Some("assistant") => {
                    let id = message["id"].as_str().unwrap_or_default().to_string();
                    if !id.is_empty() && !seen.insert(id) {
                        continue;
                    }
                    let usage = &message["usage"];
                    tokens += ["input_tokens", "output_tokens", "cache_creation_input_tokens"]
                        .iter()
                        .map(|k| usage[k].as_u64().unwrap_or(0))
                        .sum::<u64>();
                    if let Some(m) = message["model"].as_str().filter(|m| !m.starts_with('<'))
                        && model.as_ref().is_none_or(|(t, _)| t.as_str() < time)
                    {
                        model = Some((time.to_string(), m.to_string()));
                    }
                }
                _ => {}
            }
        }
    }

    let live = CLAUDE_LIVE.lock().ok().and_then(|mut live| {
        if live.0.is_none_or(|at| at.elapsed() >= CLAUDE_LIVE_TTL) {
            live.0 = Some(Instant::now());
            if let Some(got) = claude_live_limits(home) {
                live.1 = Some(got);
            }
        }
        live.1.clone()
    });
    let (tier, limits) = live.or_else(|| claude_cached_limits(home)).unwrap_or_default();

    if tokens == 0 && prompts == 0 && limits.is_empty() && tier.is_none() && model.is_none() {
        return None;
    }
    let final_tier = tier.or(model.map(|(_, m)| m));
    Some(json!({ "tier_label": final_tier, "limits": limits, "today_tokens": tokens, "today_prompts": prompts }))
}

fn claude_live_limits(home: &Path) -> Option<(Option<String>, Vec<Value>)> {
    let creds_file = home.join(".claude/.credentials.json");
    let content = std::fs::read_to_string(creds_file).ok()?;
    let creds: Value = serde_json::from_str(&content).ok()?;
    let oauth = creds.get("claudeAiOauth").unwrap_or(&creds);
    let token = oauth["accessToken"].as_str()?;
    let plan = match oauth["subscriptionType"].as_str().or_else(|| oauth["rateLimitTier"].as_str()) {
        Some("claude_team") => Some("Team".to_string()),
        Some("claude_pro") => Some("Pro".to_string()),
        Some("claude_free") => Some("Free".to_string()),
        Some(t) if !t.is_empty() => Some(t.to_string()),
        _ => None,
    };

    let resp = curl_get("https://api.anthropic.com/api/oauth/usage", &[
        ("Authorization", &format!("Bearer {token}")),
        ("anthropic-beta", "oauth-2025-04-20"),
        ("Accept", "application/json"),
    ])?;

    let mut limits = Vec::new();
    if let Some(session) = resp.get("five_hour")
        && let Some(used) = session["utilization"].as_f64()
    {
        limits.push(json!({
            "label": "Session (5-hour)",
            "percent": used / 100.0,
            "resets_at": session["resets_at"].as_str(),
        }));
    }
    if let Some(weekly) = resp.get("seven_day")
        && let Some(used) = weekly["utilization"].as_f64()
    {
        limits.push(json!({
            "label": "Weekly (7-day)",
            "percent": used / 100.0,
            "resets_at": weekly["resets_at"].as_str(),
        }));
    }
    if limits.is_empty()
        && let Some(arr) = resp["limits"].as_array()
    {
        for item in arr {
            if let Some(p) = item["percent"].as_f64() {
                let kind = item["kind"].as_str().unwrap_or("Limit");
                let label = if kind.contains("session") { "Session (5-hour)" } else { "Weekly (7-day)" };
                limits.push(json!({
                    "label": label,
                    "percent": p / 100.0,
                    "resets_at": item["resets_at"].as_str(),
                }));
            }
        }
    }
    // An error (e.g. rate_limit_error) has none: not an answer.
    if limits.is_empty() {
        return None;
    }
    Some((plan, limits))
}

fn claude_cached_limits(home: &Path) -> Option<(Option<String>, Vec<Value>)> {
    let json = cache_json(home, &["anthropic", "claude"])?;
    let tier = json.get("tierLabel").or_else(|| json.get("tier_label")).and_then(|v| v.as_str()).map(String::from);
    let mut limits = json.get("limits").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    for l in &mut limits {
        if l.get("label").is_none() {
            let session = l["kind"].as_str().is_some_and(|k| k.contains("session"));
            l["label"] = json!(if session { "Session (5-hour)" } else { "Weekly (7-day)" });
        }
    }
    Some((tier, limits))
}

/// OpenAI / Codex: live RPC / app-server rate limits plus local session stats.
fn codex(home: &Path) -> Option<Value> {
    let today = today_utc();
    let (mut tokens, mut prompts) = (0u64, 0u64);
    let mut model = None::<(String, String)>;
    let mut latest_log_limits: Option<(String, Value)> = None;

    for file in recent_files(&home.join(".codex/sessions"), 4, "jsonl") {
        let mut session_tokens = 0;
        for line in lines(&file) {
            let Ok(entry) = serde_json::from_str::<Value>(&line) else { continue };
            let time = entry["timestamp"].as_str().unwrap_or_default().to_string();
            let payload = &entry["payload"];
            match (entry["type"].as_str(), payload["type"].as_str()) {
                (Some("event_msg"), Some("token_count")) => {
                    if let Some(total) = payload["info"]["total_token_usage"]["total_tokens"].as_u64()
                        && time.starts_with(&today)
                    {
                        session_tokens = total;
                    }
                    let limits = &payload["rate_limits"];
                    if limits.is_object() && latest_log_limits.as_ref().is_none_or(|(t, _)| *t < time) {
                        latest_log_limits = Some((time, limits.clone()));
                    }
                }
                (Some("event_msg"), Some("user_message")) if time.starts_with(&today) => prompts += 1,
                (Some("turn_context"), _) => {
                    if let Some(m) = payload["model"].as_str()
                        && model.as_ref().is_none_or(|(t, _)| *t < time)
                    {
                        model = Some((time, m.to_string()));
                    }
                }
                _ => {}
            }
        }
        tokens += session_tokens;
    }

    let (plan, mut limits) = codex_rpc(home).unwrap_or_else(|| {
        codex_cached_limits(home).unwrap_or_default()
    });

    if limits.is_empty()
        && let Some((_, log_limits)) = latest_log_limits
    {
        for key in ["primary", "secondary"] {
            let w = &log_limits[key];
            if let Some(used) = w["used_percent"].as_f64() {
                let mins = w["window_minutes"].as_u64();
                let label = match mins {
                    Some(10080) => "Weekly (7-day)".to_string(),
                    Some(43200) => "Monthly".to_string(),
                    Some(m) if m % 60 == 0 => format!("{}-hour window", m / 60),
                    Some(m) => format!("{m}-minute window"),
                    None => "Rate limit".to_string(),
                };
                let resets_at = w["resets_at"].as_u64().map(iso).or_else(|| {
                    let secs = w["resets_in_seconds"].as_u64()?;
                    Some(iso(SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() + secs))
                });
                limits.push(json!({
                    "label": label,
                    "percent": used / 100.0,
                    "resets_at": resets_at,
                }));
            }
        }
    }

    if tokens == 0 && prompts == 0 && limits.is_empty() && plan.is_none() && model.is_none() {
        return None;
    }
    let final_tier = plan.or(model.map(|(_, m)| m));
    Some(json!({ "tier_label": final_tier, "limits": limits, "today_tokens": tokens, "today_prompts": prompts }))
}

fn codex_rpc(home: &Path) -> Option<(Option<String>, Vec<Value>)> {
    if !home.join(".codex").exists() {
        return None;
    }
    let mut cmd = std::process::Command::new("codex");
    cmd.args(["-s", "read-only", "-a", "never", "app-server"])
        .env("CODEX_HOME", home.join(".codex"))
        .env("HOME", home)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = cmd.spawn().ok()?;

    let mut stdin = child.stdin.take()?;
    let stdout = child.stdout.take()?;

    let _ = writeln!(stdin, "{}", json!({"id": 1, "method": "initialize", "params": {"clientInfo": {"name": "herdr-bridge", "version": "1"}}}));
    let _ = writeln!(stdin, "{}", json!({"method": "initialized", "params": {}}));
    let _ = writeln!(stdin, "{}", json!({"id": 2, "method": "account/rateLimits/read"}));
    let _ = stdin.flush();

    let reader = BufReader::new(stdout);
    let mut result = None;
    for line in reader.lines().map_while(Result::ok) {
        if let Ok(msg) = serde_json::from_str::<Value>(&line)
            && msg["id"] == 2
        {
            result = Some(msg);
            break;
        }
    }
    let _ = child.kill();
    let msg = result?;
    let limits_obj = &msg["result"]["rateLimits"];
    let plan = limits_obj["planType"].as_str().map(|p| {
        let mut c = p.chars();
        match c.next() {
            None => String::new(),
            Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        }
    });
    let mut limits = Vec::new();
    for key in ["primary", "secondary"] {
        let w = &limits_obj[key];
        if let Some(used) = w["usedPercent"].as_f64() {
            let mins = w["windowDurationMins"].as_u64();
            let label = match mins {
                Some(10080) => "Weekly (7-day)".to_string(),
                Some(43200) => "Monthly".to_string(),
                Some(m) if m % 60 == 0 => format!("{}-hour window", m / 60),
                Some(m) => format!("{m}-minute window"),
                None => "Rate limit".to_string(),
            };
            let resets_at = w["resetsAt"].as_u64().map(iso);
            limits.push(json!({
                "label": label,
                "percent": used / 100.0,
                "resets_at": resets_at,
            }));
        }
    }
    Some((plan, limits))
}

fn codex_cached_limits(home: &Path) -> Option<(Option<String>, Vec<Value>)> {
    let json = cache_json(home, &["openai", "codex"])?;
    let tier = json.get("tierLabel").or_else(|| json.get("plan_type")).and_then(|v| v.as_str()).map(String::from);
    let limits = json.get("limits").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    Some((tier, limits))
}

/// Antigravity / Gemini: live `agy -p /usage` probe plus local brain transcripts and Gemini CLI chats.
fn antigravity(home: &Path) -> Option<Value> {
    let today = today_utc();
    let (mut tokens, mut prompts) = (0u64, 0u64);
    let mut model: Option<(String, String)> = None;

    // 1. Antigravity CLI brain transcripts
    for file in recent_files(&home.join(".gemini/antigravity-cli/brain"), 6, "jsonl") {
        if file.file_name().is_none_or(|n| n != "transcript.jsonl") {
            continue;
        }
        for line in lines(&file) {
            let Ok(step) = serde_json::from_str::<Value>(&line) else { continue };
            let time = step["created_at"].as_str().unwrap_or_default();
            if !time.starts_with(&today) {
                continue;
            }
            if step["type"] == "USER_INPUT" {
                prompts += 1;
                if let Some(content) = step["content"].as_str() {
                    if let Some(pos) = content.find("Model Selection` from ") {
                        let rest = &content[pos + 22..];
                        if let Some(to_pos) = rest.find(" to ") {
                            let after_to = &rest[to_pos + 4..];
                            let m = if let Some(end) = after_to.find(". No need") {
                                &after_to[..end]
                            } else if let Some(end) = after_to.find("</") {
                                &after_to[..end]
                            } else if let Some(end) = after_to.find('\n') {
                                &after_to[..end]
                            } else {
                                after_to
                            };
                            let m = m.trim().trim_end_matches('.');
                            if !m.is_empty() && model.as_ref().is_none_or(|(t, _)| t.as_str() < time) {
                                model = Some((time.to_string(), m.to_string()));
                            }
                        }
                    }
                }
            }
            tokens += ["input_tokens", "output_tokens", "cache_read_tokens"]
                .iter()
                .map(|k| step[k].as_u64().unwrap_or(0))
                .sum::<u64>();
        }
    }

    // 2. Gemini CLI chat sessions
    for file in recent_files(&home.join(".gemini/tmp"), 6, "jsonl") {
        for line in lines(&file) {
            let Ok(entry) = serde_json::from_str::<Value>(&line) else { continue };
            let time = entry["timestamp"].as_str().unwrap_or_default();
            if !time.starts_with(&today) {
                continue;
            }
            match entry["type"].as_str() {
                Some("user") => prompts += 1,
                Some("gemini") => {
                    let toks = &entry["tokens"];
                    let inp = toks["input"].as_u64().unwrap_or(0);
                    let out = toks["output"].as_u64().unwrap_or(0);
                    let thoughts = toks["thoughts"].as_u64().unwrap_or(0);
                    tokens += inp + out + thoughts;
                    if let Some(m) = entry["model"].as_str()
                        && model.as_ref().is_none_or(|(t, _)| t.as_str() < time)
                    {
                        model = Some((time.to_string(), m.to_string()));
                    }
                }
                _ => {}
            }
        }
    }

    let mut limits = antigravity_probe(home);
    if limits.is_empty()
        && let Some(cached) = cache_json(home, &["antigravity", "gemini"])
        && let Some(arr) = cached.get("limits").and_then(|v| v.as_array())
    {
        limits = arr.clone();
    }

    if tokens == 0 && prompts == 0 && limits.is_empty() {
        return None;
    }
    let tier = model.map(|(_, m)| m).unwrap_or_else(|| "Gemini 3.7 Flash".to_string());
    Some(json!({ "tier_label": tier, "limits": limits, "today_tokens": tokens, "today_prompts": prompts }))
}

fn antigravity_probe(home: &Path) -> Vec<Value> {
    let mut cmd = std::process::Command::new("agy");
    cmd.args(["-p", "/usage", "--output-format", "json"])
        .env("HOME", home);
    let out = cmd.output().ok();
    let Some(out) = out.filter(|o| o.status.success()) else { return Vec::new() };
    let Ok(data) = serde_json::from_slice::<Value>(&out.stdout) else { return Vec::new() };
    let mut limits = Vec::new();
    if let Some(groups) = data["command"]["data"]["groups"].as_array() {
        for g in groups {
            let g_name = g["name"].as_str().unwrap_or("");
            if let Some(buckets) = g["buckets"].as_array() {
                let mut buckets = buckets.clone();
                buckets.sort_by_key(|b| match b["window"].as_str() {
                    Some("5h") => 0,
                    Some("weekly") => 1,
                    _ => 2,
                });
                for b in &buckets {
                    let rem = b["remaining_fraction"].as_f64().unwrap_or(1.0);
                    let used = (1.0 - rem).clamp(0.0, 1.0);
                    let window = b["window"].as_str().unwrap_or("");
                    let reset = b["reset_time"].as_str();
                    let label = if g_name == "Gemini Models" {
                        if window == "5h" {
                            "Session (5-hour)".to_string()
                        } else if window == "weekly" {
                            "Weekly (7-day)".to_string()
                        } else {
                            b["name"].as_str().unwrap_or("Limit").to_string()
                        }
                    } else if !g_name.is_empty() {
                        format!("{} {}", g_name, b["name"].as_str().unwrap_or("Limit"))
                    } else {
                        b["name"].as_str().unwrap_or("Limit").to_string()
                    };
                    limits.push(json!({
                        "label": label,
                        "percent": (used * 100.0).round() / 100.0,
                        "resets_at": reset,
                    }));
                }
            }
        }
    }
    limits
}

/// xAI Grok: live billing API, local session stats, settings cache, and usage files.
fn grok(home: &Path) -> Option<Value> {
    let today = today_utc();
    let (mut tokens, mut prompts) = (0u64, 0u64);
    let mut model: Option<(String, String)> = None;

    // Scan Grok session updates.jsonl for today's tokens & prompt counts
    for file in recent_files(&home.join(".grok/sessions"), 4, "jsonl") {
        if file.file_name().is_none_or(|n| n != "updates.jsonl") {
            continue;
        }
        for line in lines(&file) {
            if !line.contains("turn_completed") {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<Value>(&line) else { continue };
            let time = entry["timestamp"].as_str().unwrap_or_default();
            if !time.starts_with(&today) {
                continue;
            }
            let update = entry.get("params").and_then(|p| p.get("update")).unwrap_or(&entry);
            if let Some(usage) = update.get("usage").filter(|u| u.is_object()) {
                prompts += 1;
                let inp = usage["inputTokens"].as_u64().unwrap_or(0);
                let out = usage["outputTokens"].as_u64().unwrap_or(0);
                tokens += inp + out;
                if let Some(m_obj) = usage.get("modelUsage").and_then(|m| m.as_object()) {
                    for (m_name, _) in m_obj {
                        if model.as_ref().is_none_or(|(t, _)| t.as_str() < time) {
                            model = Some((time.to_string(), m_name.clone()));
                        }
                    }
                }
            }
        }
    }

    let (live_tier, mut limits) = grok_live_billing(home).unwrap_or_default();

    let mut tier = live_tier;
    if tier.is_none() {
        // Check ~/.grok/settings_cache.json
        if let Ok(content) = std::fs::read_to_string(home.join(".grok/settings_cache.json"))
            && let Ok(v) = serde_json::from_str::<Value>(&content)
            && let Some(payload_str) = v["payload"].as_str()
            && let Ok(payload) = serde_json::from_str::<Value>(payload_str)
            && let Some(t) = payload["settings"]["subscription_tier_display"].as_str()
        {
            tier = Some(t.to_string());
        }
    }

    if limits.is_empty()
        && let Some(json) = cache_json(home, &["supergrok", "grok"])
    {
        if tier.is_none() {
            tier = json.get("tierLabel").and_then(|v| v.as_str()).map(String::from).or_else(|| {
                json.get("snapshot").and_then(|s| s.get("plan")).and_then(|p| p.as_str()).map(String::from)
            });
        }
        if let Some(arr) = json.get("limits").and_then(|v| v.as_array()) {
            limits = arr.clone();
        } else if let Some(snap) = json.get("snapshot")
            && let Some(pct) = snap["percent"].as_f64()
        {
            limits.push(json!({
                "label": "Weekly SuperGrok Limit",
                "percent": pct / 100.0,
                "resets_at": snap["reset_at"].as_str(),
            }));
        }
    }

    if tokens == 0 && prompts == 0 && limits.is_empty() && tier.is_none() && model.is_none() {
        return None;
    }
    let final_tier = tier.or(model.map(|(_, m)| m)).unwrap_or_else(|| "Grok".to_string());
    Some(json!({ "tier_label": final_tier, "limits": limits, "today_tokens": tokens, "today_prompts": prompts }))
}

fn grok_live_billing(home: &Path) -> Option<(Option<String>, Vec<Value>)> {
    let auth_file = home.join(".grok/auth.json");
    let content = std::fs::read_to_string(auth_file).ok()?;
    let auth: Value = serde_json::from_str(&content).ok()?;
    let map = auth.as_object()?;
    let entry = map.values().find(|e| {
        let mode = e["auth_mode"].as_str().unwrap_or("");
        mode == "oidc" || mode == "oauth" || mode.is_empty()
    })?;
    let key = entry["key"].as_str()?;
    if key.is_empty() || key.starts_with("sk-") || key.starts_with("xai-") {
        return None;
    }
    let user_id = entry["user_id"].as_str().unwrap_or("");

    let resp = curl_get("https://cli-chat-proxy.grok.com/v1/billing?format=credits", &[
        ("Authorization", &format!("Bearer {key}")),
        ("x-xai-token-auth", "xai-grok-cli"),
        ("x-userid", user_id),
    ])?;

    let plan = resp.get("plan").or_else(|| resp.get("tier")).and_then(|v| v.as_str()).map(String::from);
    let mut limits = Vec::new();
    if let Some(pct) = resp["creditUsagePercent"].as_f64() {
        limits.push(json!({
            "label": "Weekly SuperGrok Limit",
            "percent": pct / 100.0,
            "resets_at": resp["currentPeriod"]["end"].as_str().or_else(|| resp["billingPeriodEnd"].as_str()),
        }));
    }
    Some((plan, limits))
}

/// OpenCode / OpenCode Go: API quota and account balance.
fn opencode(home: &Path) -> Option<Value> {
    let mut limits = Vec::new();
    let mut tier = None;

    // Check OpenCode Go API
    let auth_file = home.join(".local/share/opencode/auth.json");
    if let Ok(content) = std::fs::read_to_string(auth_file)
        && let Ok(auth) = serde_json::from_str::<Value>(&content)
        && let Some(key) = auth["opencode-go"]["key"].as_str()
    {
        if let Some(resp) = curl_get("https://opencode.ai/zen/go/v1/usage", &[("Authorization", &format!("Bearer {key}"))]) {
            tier = resp["plan"].as_str().map(String::from).or_else(|| Some("OpenCode Go".to_string()));
            if let Some(pct) = resp["percent"].as_f64() {
                limits.push(json!({
                    "label": "OpenCode Go Quota",
                    "percent": pct / 100.0,
                    "resets_at": resp["resets_at"].as_str(),
                }));
            }
        }
    }

    if limits.is_empty()
        && let Some(json) = cache_json(home, &["opencode-go", "opencode"])
    {
        tier = json.get("tierLabel").or_else(|| json.get("plan")).and_then(|v| v.as_str()).map(String::from);
        if let Some(arr) = json.get("limits").and_then(|v| v.as_array()) {
            limits = arr.clone();
        }
    }

    if limits.is_empty() && tier.is_none() {
        return None;
    }
    Some(json!({ "tier_label": tier.unwrap_or_else(|| "OpenCode".to_string()), "limits": limits, "today_tokens": 0, "today_prompts": 0 }))
}

/// GitHub Copilot: live OAuth token / internal quota endpoint and local caches.
fn copilot(home: &Path) -> Option<Value> {
    let mut tier = None;
    let mut limits = Vec::new();

    // Read token from ~/.config/github-copilot/hosts.json or ~/.config/gh/hosts.yml
    let token = copilot_auth_token(home);
    if let Some(tok) = &token {
        if let Some(resp) = curl_get("https://api.github.com/copilot_internal/v2/token", &[
            ("Authorization", &format!("Bearer {tok}")),
            ("Accept", "application/vnd.github.v3+json"),
            ("Editor-Version", "vscode/1.90.0"),
            ("User-Agent", "GitHubCopilot/1.190.0"),
        ]).or_else(|| {
            curl_get("https://api.github.com/copilot_internal/v2/token", &[
                ("Authorization", &format!("token {tok}")),
                ("Accept", "application/vnd.github.v3+json"),
                ("Editor-Version", "vscode/1.90.0"),
                ("User-Agent", "GitHubCopilot/1.190.0"),
            ])
        }) {
            if let Some(sku) = resp["sku"].as_str() {
                tier = Some(match sku {
                    "copilot_for_business" | "business" => "Copilot Business".to_string(),
                    "copilot_enterprise" | "enterprise" => "Copilot Enterprise".to_string(),
                    "individual" => "Copilot Individual".to_string(),
                    other => other.to_string(),
                });
            }
            if let Some(expires) = resp["expires_at"].as_u64() {
                limits.push(json!({
                    "label": "Session Token",
                    "percent": 0.0,
                    "resets_at": iso(expires),
                }));
            }
        }
    }

    if limits.is_empty() && tier.is_none()
        && let Some(json) = cache_json(home, &["copilot", "github-copilot"])
    {
        tier = json.get("tierLabel").or_else(|| json.get("sku")).and_then(|v| v.as_str()).map(String::from);
        if let Some(arr) = json.get("limits").and_then(|v| v.as_array()) {
            limits = arr.clone();
        }
    }

    if limits.is_empty() && tier.is_none() {
        return None;
    }
    Some(json!({ "tier_label": tier.unwrap_or_else(|| "Copilot".to_string()), "limits": limits, "today_tokens": 0, "today_prompts": 0 }))
}

fn copilot_auth_token(home: &Path) -> Option<String> {
    // 1. ~/.config/github-copilot/hosts.json
    if let Ok(content) = std::fs::read_to_string(home.join(".config/github-copilot/hosts.json"))
        && let Ok(v) = serde_json::from_str::<Value>(&content)
        && let Some(t) = v["github.com"]["oauth_token"].as_str()
    {
        return Some(t.to_string());
    }
    // 2. ~/.config/gh/hosts.yml
    if let Ok(content) = std::fs::read_to_string(home.join(".config/gh/hosts.yml")) {
        for line in content.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("oauth_token:") {
                let tok = rest.trim();
                if !tok.is_empty() {
                    return Some(tok.to_string());
                }
            }
        }
    }
    // 3. Command `gh auth token`
    if let Ok(out) = std::process::Command::new("gh").args(["auth", "token"]).output()
        && out.status.success()
    {
        let tok = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !tok.is_empty() {
            return Some(tok);
        }
    }
    std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.is_empty())
}

/// Cursor: live usage / membership endpoints and local caches.
fn cursor(home: &Path) -> Option<Value> {
    let mut tier = None;
    let mut limits = Vec::new();

    // Check token from auth files or state db
    let token = cursor_auth_token(home);
    if let Some(tok) = &token {
        if let Some(resp) = curl_get("https://authenticator.cursor.sh/api/usage", &[("Authorization", &format!("Bearer {tok}"))])
            .or_else(|| curl_get("https://api2.cursor.sh/auth/stripe", &[("Authorization", &format!("Bearer {tok}"))]))
        {
            if let Some(m) = resp.get("membershipType").or_else(|| resp.get("plan")).and_then(|v| v.as_str()) {
                tier = Some(match m.to_lowercase().as_str() {
                    "pro" => "Cursor Pro".to_string(),
                    "business" => "Cursor Business".to_string(),
                    "hobby" => "Cursor Hobby".to_string(),
                    _ => m.to_string(),
                });
            }
            if let Some(fast) = resp.get("fastRequests").or_else(|| resp.get("gpt4Usage"))
                && let Some(num) = fast["numRequests"].as_f64()
                && let Some(max) = fast["maxRequestUsage"].as_f64().filter(|&m| m > 0.0)
            {
                limits.push(json!({
                    "label": "Fast Requests",
                    "percent": (num / max).clamp(0.0, 1.0),
                    "resets_at": resp["startOfMonth"].as_str(),
                }));
            }
        }
    }

    if limits.is_empty() && tier.is_none()
        && let Some(json) = cache_json(home, &["cursor"])
    {
        tier = json.get("tierLabel").or_else(|| json.get("membershipType")).and_then(|v| v.as_str()).map(String::from);
        if let Some(arr) = json.get("limits").and_then(|v| v.as_array()) {
            limits = arr.clone();
        }
    }

    if limits.is_empty() && tier.is_none() {
        return None;
    }
    Some(json!({ "tier_label": tier.unwrap_or_else(|| "Cursor".to_string()), "limits": limits, "today_tokens": 0, "today_prompts": 0 }))
}

fn cursor_auth_token(home: &Path) -> Option<String> {
    for path in [
        home.join(".cursor/auth.json"),
        home.join(".cursor/credentials.json"),
    ] {
        if let Ok(content) = std::fs::read_to_string(path)
            && let Ok(v) = serde_json::from_str::<Value>(&content)
            && let Some(t) = v["accessToken"].as_str().or_else(|| v["token"].as_str())
        {
            return Some(t.to_string());
        }
    }
    None
}

/// OpenRouter: live API key limits and credit balances.
fn openrouter(home: &Path) -> Option<Value> {
    let mut tier = None;
    let mut limits = Vec::new();

    let key = std::fs::read_to_string(home.join(".config/openrouter/key.txt"))
        .or_else(|_| std::fs::read_to_string(home.join(".openrouter/auth.json")))
        .ok()
        .and_then(|s| {
            if let Ok(v) = serde_json::from_str::<Value>(&s) {
                v["key"].as_str().map(String::from)
            } else {
                let trimmed = s.trim().to_string();
                if !trimmed.is_empty() { Some(trimmed) } else { None }
            }
        })
        .or_else(|| std::env::var("OPENROUTER_API_KEY").ok());

    if let Some(k) = key
        && let Some(resp) = curl_get("https://openrouter.ai/api/v1/auth/key", &[("Authorization", &format!("Bearer {k}"))])
        && let Some(data) = resp.get("data")
    {
        let label = data["label"].as_str().unwrap_or("OpenRouter");
        tier = Some(label.to_string());
        let usage = data["usage"].as_f64().unwrap_or(0.0);
        if let Some(limit) = data["limit"].as_f64().filter(|&l| l > 0.0) {
            limits.push(json!({
                "label": "Credit Limit",
                "percent": (usage / limit).clamp(0.0, 1.0),
            }));
        } else if data["is_free_tier"].as_bool() == Some(true) {
            limits.push(json!({
                "label": "Free Tier",
                "percent": 0.0,
            }));
        }
    }

    if limits.is_empty() && tier.is_none()
        && let Some(json) = cache_json(home, &["openrouter"])
    {
        tier = json.get("tierLabel").or_else(|| json.get("label")).and_then(|v| v.as_str()).map(String::from);
        if let Some(arr) = json.get("limits").and_then(|v| v.as_array()) {
            limits = arr.clone();
        }
    }

    if limits.is_empty() && tier.is_none() {
        return None;
    }
    Some(json!({ "tier_label": tier.unwrap_or_else(|| "OpenRouter".to_string()), "limits": limits, "today_tokens": 0, "today_prompts": 0 }))
}

/// DeepSeek: account balance and availability probe.
fn deepseek(home: &Path) -> Option<Value> {
    let mut tier = None;
    let mut limits = Vec::new();

    let key = std::fs::read_to_string(home.join(".config/deepseek/key.txt"))
        .or_else(|_| std::fs::read_to_string(home.join(".deepseek/auth.json")))
        .ok()
        .and_then(|s| {
            if let Ok(v) = serde_json::from_str::<Value>(&s) {
                v["key"].as_str().map(String::from)
            } else {
                let trimmed = s.trim().to_string();
                if !trimmed.is_empty() { Some(trimmed) } else { None }
            }
        })
        .or_else(|| std::env::var("DEEPSEEK_API_KEY").ok());

    if let Some(k) = key
        && let Some(resp) = curl_get("https://api.deepseek.com/user/balance", &[("Authorization", &format!("Bearer {k}"))])
    {
        tier = Some("DeepSeek API".to_string());
        if let Some(balances) = resp["balance_infos"].as_array() {
            for b in balances {
                let curr = b["currency"].as_str().unwrap_or("USD");
                let total = b["total_balance"].as_str().unwrap_or("0");
                limits.push(json!({
                    "label": format!("Balance ({curr})"),
                    "percent": 0.0,
                    "title": format!("{total} {curr}"),
                }));
            }
        }
    }

    if limits.is_empty() && tier.is_none()
        && let Some(json) = cache_json(home, &["deepseek"])
    {
        tier = json.get("tierLabel").and_then(|v| v.as_str()).map(String::from);
        if let Some(arr) = json.get("limits").and_then(|v| v.as_array()) {
            limits = arr.clone();
        }
    }

    if limits.is_empty() && tier.is_none() {
        return None;
    }
    Some(json!({ "tier_label": tier.unwrap_or_else(|| "DeepSeek".to_string()), "limits": limits, "today_tokens": 0, "today_prompts": 0 }))
}

fn cache_json(home: &Path, names: &[&str]) -> Option<Value> {
    let state_dirs = [
        home.join(".cache/ai-usagebar"),
        home.join(".local/state/omarchy/agents/usage"),
        home.join(".local/state/herdr/usage"),
    ];
    // The newest of them: a tool no longer running leaves its last numbers behind.
    let newest = state_dirs
        .iter()
        .flat_map(|dir| names.iter().flat_map(move |name| [dir.join(name).join("usage.json"), dir.join(format!("{name}.json"))]))
        .filter_map(|p| Some((std::fs::metadata(&p).ok()?.modified().ok()?, p)))
        .max_by_key(|(at, _)| *at)?;
    serde_json::from_str(&std::fs::read_to_string(newest.1).ok()?).ok()
}

fn curl_get(url: &str, headers: &[(&str, &str)]) -> Option<Value> {
    use std::process::Stdio;
    // Headers on stdin (`-H @-`), not in the arguments, where any user's `ps` would read their tokens.
    let mut child = std::process::Command::new("curl")
        .args(["-s", "-m", "4", "--compressed", "-H", "@-", url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    for (k, v) in headers {
        writeln!(stdin, "{k}: {v}").ok()?;
    }
    drop(stdin);
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// The files ending in `.ext` up to [depth] directories under [dir] that were written recently.
fn recent_files(dir: &Path, depth: usize, ext: &str) -> Vec<PathBuf> {
    let mut found = vec![];
    let Ok(entries) = std::fs::read_dir(dir) else { return found };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        let path = entry.path();
        if kind.is_dir() && depth > 1 {
            found.extend(recent_files(&path, depth - 1, ext));
        } else if kind.is_file()
            && path.extension().is_some_and(|e| e == ext)
            && entry.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|e| e < RECENT))
        {
            found.push(path);
        }
    }
    found
}

fn lines(path: &Path) -> impl Iterator<Item = String> {
    File::open(path).into_iter().flat_map(|f| BufReader::new(f).lines().map_while(Result::ok))
}

/// Today in UTC, as `YYYY-MM-DD`: the transcripts' timestamps are UTC.
fn today_utc() -> String {
    iso(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs())[..10].to_string()
}

/// [secs] since the epoch as `YYYY-MM-DDTHH:MM:SSZ`.
fn iso(secs: u64) -> String {
    let z = (secs / 86400) as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let t = secs % 86400;
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", t / 3600, t / 60 % 60, t % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, lines: &[Value]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();
    }

    fn home(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("herdr-usage-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn claude_counts_today_once_per_message() {
        let home = home("claude");
        let now = format!("{}T10:00:00.000Z", today_utc());
        let usage = json!({ "input_tokens": 10, "output_tokens": 5, "cache_creation_input_tokens": 100, "cache_read_input_tokens": 9999 });
        write(&home.join(".claude/projects/p/s.jsonl"), &[
            json!({ "type": "user", "timestamp": now, "message": { "content": "hi" } }),
            json!({ "type": "user", "timestamp": now, "message": { "content": [{ "type": "tool_result" }] } }),
            json!({ "type": "assistant", "timestamp": now, "message": { "id": "a", "model": "claude-opus-5-5", "usage": usage } }),
            json!({ "type": "assistant", "timestamp": now, "message": { "id": "a", "model": "claude-opus-5-5", "usage": usage } }),
            json!({ "type": "assistant", "timestamp": "2000-01-01T00:00:00Z", "message": { "id": "b", "usage": usage } }),
        ]);
        let got = claude(&home).unwrap();
        assert_eq!(got["today_tokens"], 115);
        assert_eq!(got["today_prompts"], 1);
        assert_eq!(got["tier_label"], "claude-opus-5-5");
    }

    #[test]
    fn codex_reports_its_rate_limits() {
        let home = home("codex");
        let now = format!("{}T10:00:00.000Z", today_utc());
        write(&home.join(".codex/sessions/2026/10/07/rollout.jsonl"), &[
            json!({ "timestamp": now, "type": "turn_context", "payload": { "model": "gpt-5" } }),
            json!({ "timestamp": now, "type": "event_msg", "payload": { "type": "user_message" } }),
            json!({ "timestamp": now, "type": "event_msg", "payload": { "type": "token_count",
                "info": { "total_token_usage": { "total_tokens": 1234 } },
                "rate_limits": { "primary": { "used_percent": 42.0, "window_minutes": 300, "resets_at": 1_800_000_000u64 },
                                 "secondary": { "used_percent": 7.0, "window_minutes": 10080 } } } }),
        ]);
        let got = codex(&home).unwrap();
        assert_eq!(got["today_tokens"], 1234);
        assert_eq!(got["today_prompts"], 1);
        assert_eq!(got["tier_label"], "gpt-5");
    }

    #[test]
    fn antigravity_counts_today_and_parses_model() {
        let home = home("antigravity");
        let now = format!("{}T10:00:00Z", today_utc());
        write(&home.join(".gemini/antigravity-cli/brain/session1/.system_generated/logs/transcript.jsonl"), &[
            json!({ "type": "USER_INPUT", "created_at": now, "content": "hello\n<USER_SETTINGS_CHANGE>\nThe user changed setting `Model Selection` from None to Gemini 3.7 Flash (High).\n</USER_SETTINGS_CHANGE>" }),
            json!({ "type": "PLANNER_RESPONSE", "created_at": now, "input_tokens": 100, "cache_read_tokens": 50, "output_tokens": 25 }),
            json!({ "type": "USER_INPUT", "created_at": "2000-01-01T00:00:00Z", "content": "old" }),
            json!({ "type": "PLANNER_RESPONSE", "created_at": "2000-01-01T00:00:00Z", "input_tokens": 999 }),
        ]);
        let got = antigravity(&home).unwrap();
        assert_eq!(got["today_tokens"], 175);
        assert_eq!(got["today_prompts"], 1);
        assert_eq!(got["tier_label"], "Gemini 3.7 Flash (High)");
    }

    #[test]
    fn antigravity_model_change_from_existing_model() {
        let home = home("antigravity-model-switch");
        let now = format!("{}T10:00:00Z", today_utc());
        write(&home.join(".gemini/antigravity-cli/brain/session1/.system_generated/logs/transcript.jsonl"), &[
            json!({ "type": "USER_INPUT", "created_at": now, "content": "hello\n<USER_SETTINGS_CHANGE>\nThe user changed setting `Model Selection` from Gemini 3.7 Flash (High) to Claude 3.7 Sonnet (Thinking).\n</USER_SETTINGS_CHANGE>" }),
            json!({ "type": "PLANNER_RESPONSE", "created_at": now, "input_tokens": 100, "cache_read_tokens": 0, "output_tokens": 25 }),
        ]);
        let got = antigravity(&home).unwrap();
        assert_eq!(got["tier_label"], "Claude 3.7 Sonnet (Thinking)");
    }

    #[test]
    fn grok_parses_cache() {
        let home = home("grok");
        let path = home.join(".cache/ai-usagebar/supergrok/usage.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, json!({
            "snapshot": {
                "plan": "SuperGrok",
                "percent": 80.0,
                "reset_at": "2026-10-15T15:00:00Z"
            }
        }).to_string()).unwrap();

        let got = grok(&home).unwrap();
        assert_eq!(got["tier_label"], "SuperGrok");
        assert_eq!(got["limits"][0]["percent"], 0.8);
        assert_eq!(got["limits"][0]["label"], "Weekly SuperGrok Limit");
    }

    #[test]
    fn opencode_parses_cache() {
        let home = home("opencode");
        let path = home.join(".cache/ai-usagebar/opencode-go/usage.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, json!({
            "tierLabel": "Go Unlimited",
            "limits": [{"label": "OpenCode Go Quota", "percent": 0.35, "resets_at": "2026-10-15T00:00:00Z"}]
        }).to_string()).unwrap();

        let got = opencode(&home).unwrap();
        assert_eq!(got["tier_label"], "Go Unlimited");
        assert_eq!(got["limits"][0]["percent"], 0.35);
    }

    #[test]
    fn grok_parses_sessions_and_settings() {
        let home = home("grok-sessions");
        let now = format!("{}T10:00:00Z", today_utc());
        write(&home.join(".grok/sessions/s1/updates.jsonl"), &[
            json!({
                "timestamp": now,
                "params": {
                    "update": {
                        "sessionUpdate": "turn_completed",
                        "usage": {
                            "inputTokens": 100,
                            "outputTokens": 50,
                            "modelUsage": { "grok-4.6": { "inputTokens": 100, "outputTokens": 50 } }
                        }
                    }
                }
            })
        ]);
        let settings_path = home.join(".grok/settings_cache.json");
        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        std::fs::write(&settings_path, json!({
            "payload": json!({
                "settings": {
                    "subscription_tier_display": "SuperGrok Pro"
                }
            }).to_string()
        }).to_string()).unwrap();

        let got = grok(&home).unwrap();
        assert_eq!(got["today_tokens"], 150);
        assert_eq!(got["today_prompts"], 1);
        assert_eq!(got["tier_label"], "SuperGrok Pro");
    }

    #[test]
    fn copilot_parses_cache() {
        let home = home("copilot");
        let path = home.join(".cache/ai-usagebar/copilot/usage.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, json!({
            "tierLabel": "Copilot Business",
            "limits": [{"label": "Session Token", "percent": 0.0, "resets_at": "2026-10-15T00:00:00Z"}]
        }).to_string()).unwrap();

        let got = copilot(&home).unwrap();
        assert_eq!(got["tier_label"], "Copilot Business");
        assert_eq!(got["limits"][0]["label"], "Session Token");
    }

    #[test]
    fn cursor_parses_cache() {
        let home = home("cursor");
        let path = home.join(".cache/ai-usagebar/cursor/usage.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, json!({
            "tierLabel": "Cursor Pro",
            "limits": [{"label": "Fast Requests", "percent": 0.45, "resets_at": "2026-11-01T00:00:00Z"}]
        }).to_string()).unwrap();

        let got = cursor(&home).unwrap();
        assert_eq!(got["tier_label"], "Cursor Pro");
        assert_eq!(got["limits"][0]["percent"], 0.45);
    }

    #[test]
    fn openrouter_parses_cache() {
        let home = home("openrouter");
        let path = home.join(".cache/ai-usagebar/openrouter/usage.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, json!({
            "tierLabel": "Default Key",
            "limits": [{"label": "Credit Limit", "percent": 0.12}]
        }).to_string()).unwrap();

        let got = openrouter(&home).unwrap();
        assert_eq!(got["tier_label"], "Default Key");
        assert_eq!(got["limits"][0]["percent"], 0.12);
    }

    #[test]
    fn deepseek_parses_cache() {
        let home = home("deepseek");
        let path = home.join(".cache/ai-usagebar/deepseek/usage.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, json!({
            "tierLabel": "DeepSeek API",
            "limits": [{"label": "Balance (USD)", "percent": 0.0, "title": "50.00 USD"}]
        }).to_string()).unwrap();

        let got = deepseek(&home).unwrap();
        assert_eq!(got["tier_label"], "DeepSeek API");
        assert_eq!(got["limits"][0]["title"], "50.00 USD");
    }

    #[test]
    fn gemini_cli_chat_sessions() {
        let home = home("gemini-cli");
        let now = format!("{}T10:00:00Z", today_utc());
        write(&home.join(".gemini/tmp/proj/session-1.jsonl"), &[
            json!({ "type": "user", "timestamp": now }),
            json!({ "type": "gemini", "timestamp": now, "model": "gemini-2.5-pro", "tokens": { "input": 200, "output": 100, "thoughts": 50 } }),
        ]);

        let got = antigravity(&home).unwrap();
        assert_eq!(got["today_tokens"], 350);
        assert_eq!(got["today_prompts"], 1);
        assert_eq!(got["tier_label"], "gemini-2.5-pro");
    }

    #[test]
    fn live_smoke_test() {
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            let _ = claude(&home);
            let _ = codex(&home);
            let _ = antigravity(&home);
            let _ = grok(&home);
            let _ = opencode(&home);
            let _ = copilot(&home);
            let _ = cursor(&home);
            let _ = openrouter(&home);
            let _ = deepseek(&home);
        }
    }

    #[test]
    fn names_that_are_not_plain_get_nothing() {
        assert_eq!(usage_of("../../etc/passwd"), None);
        assert_eq!(usage_of("a/b"), None);
    }

    #[test]
    fn iso_dates() {
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(951_782_400), "2000-02-29T00:00:00Z");
    }
}
