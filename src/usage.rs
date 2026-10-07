//! What an agent has used today, for the app's usage sheet, read from the files the agents themselves
//! write on this machine. Only what is really there is reported: no file holds Claude's rate limits, so
//! none are given for it, and no credentials are read (the API that has them would need the OAuth token).
//!
//! Every function here reads files: call [usage_of] from a blocking thread.

use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Each agent's usage, and when it was read: scanning today's transcripts is too much for every 1.5 s poll.
type Cache = HashMap<String, (Instant, Option<Value>)>;
static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Default::default);
const CACHE_TTL: Duration = Duration::from_secs(30);

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
    let usage = usage_file(home.as_deref(), &name).or_else(|| {
        let home = home.as_deref()?;
        if name.contains("claude") {
            claude(home)
        } else if name.contains("codex") {
            codex(home)
        } else if name == "agy" || name.contains("antigravity") || name.contains("gemini") {
            antigravity(home)
        } else {
            None
        }
    });
    if let Ok(mut cache) = CACHE.lock() {
        cache.insert(name, (Instant::now(), usage.clone()));
    }
    usage
}

/// A usage file another tool keeps (`$XDG_STATE_HOME/{herdr/usage,omarchy/agents/usage}/<agent>.json`,
/// with `tierLabel`, `limits`, `todayTotalTokens`, `todayPrompts`).
fn usage_file(home: Option<&Path>, name: &str) -> Option<Value> {
    let state = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).or_else(|| Some(home?.join(".local/state")))?;
    let id = if name == "agy" { "gemini" } else { name };
    ["herdr/usage", "omarchy/agents/usage"].iter().find_map(|dir| {
        let json: Value = serde_json::from_str(&std::fs::read_to_string(state.join(dir).join(format!("{id}.json"))).ok()?).ok()?;
        Some(json!({
            "tier_label": json.get("tierLabel").or_else(|| json.get("tier_label")),
            "limits": json.get("limits").cloned().unwrap_or_else(|| json!([])),
            "today_tokens": json.get("todayTotalTokens").or_else(|| json.get("today_tokens")),
            "today_prompts": json.get("todayPrompts").or_else(|| json.get("today_prompts")),
        }))
    })
}

/// Claude Code's transcripts (`~/.claude/projects/<project>/<session>.jsonl`): today's tokens (input,
/// output and cache writes; cache reads would swamp them), the prompts typed, and the model last used.
fn claude(home: &Path) -> Option<Value> {
    let today = today_utc();
    let (mut tokens, mut prompts, mut model, mut seen) = (0u64, 0u64, None::<(String, String)>, HashSet::new());
    for file in recent_files(&home.join(".claude/projects"), 4, "jsonl") {
        for line in lines(&file) {
            // Cheap test before parsing: most lines are from other days, or tool output.
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
                // A typed prompt's content is text; a tool's result comes back as a list.
                Some("user") if message["content"].is_string() && entry["isMeta"] != true => prompts += 1,
                Some("assistant") => {
                    // One message is written once per content block, each with the same usage.
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
    if tokens == 0 && prompts == 0 {
        return None;
    }
    Some(json!({ "tier_label": model.map(|(_, m)| m), "limits": [], "today_tokens": tokens, "today_prompts": prompts }))
}

/// Codex's session logs (`~/.codex/sessions/YYYY/MM/DD/*.jsonl`), whose `token_count` events carry the
/// rate limits as the server reported them: the latest of those, each session's last token total, and
/// today's prompts.
fn codex(home: &Path) -> Option<Value> {
    let today = today_utc();
    let (mut tokens, mut prompts) = (0u64, 0u64);
    let mut latest: Option<(String, Value)> = None;
    let mut model = None::<(String, String)>;
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
                    if limits.is_object() && latest.as_ref().is_none_or(|(t, _)| *t < time) {
                        latest = Some((time, limits.clone()));
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
    let limits: Vec<Value> = latest
        .map(|(_, limits)| {
            ["primary", "secondary"]
                .iter()
                .filter_map(|k| {
                    let limit = &limits[k];
                    let used = limit["used_percent"].as_f64()?;
                    let label = match limit["window_minutes"].as_u64() {
                        Some(10080) => "Weekly".to_string(),
                        Some(m) if m % 60 == 0 => format!("{}-hour window", m / 60),
                        Some(m) => format!("{m}-minute window"),
                        None => "Rate limit".to_string(),
                    };
                    let resets = limit["resets_at"].as_u64().map(iso).or_else(|| {
                        let secs = limit["resets_in_seconds"].as_u64()?;
                        Some(iso(SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() + secs))
                    });
                    Some(json!({ "label": label, "percent": used / 100.0, "resets_at": resets }))
                })
                .collect()
        })
        .unwrap_or_default();
    if tokens == 0 && prompts == 0 && limits.is_empty() {
        return None;
    }
    Some(json!({ "tier_label": model.map(|(_, m)| m), "limits": limits, "today_tokens": tokens, "today_prompts": prompts }))
}

/// Antigravity's transcripts (`~/.gemini/antigravity-cli/brain/<session>/…/transcript.jsonl`): today's
/// tokens and prompts, plus active model.
fn antigravity(home: &Path) -> Option<Value> {
    let today = today_utc();
    let (mut tokens, mut prompts) = (0u64, 0u64);
    let mut model: Option<(String, String)> = None;
    for file in recent_files(&home.join(".gemini/antigravity-cli/brain"), 5, "jsonl") {
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
                    if let Some(pos) = content.find("Model Selection` from None to ") {
                        let rest = &content[pos + 30..];
                        if let Some(end) = rest.find('.') {
                            let m = rest[..end].trim();
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
    if tokens == 0 && prompts == 0 {
        return None;
    }
    let tier = model.map(|(_, m)| m).unwrap_or_else(|| "Gemini 3.7 Flash".to_string());
    Some(json!({ "tier_label": tier, "limits": [], "today_tokens": tokens, "today_prompts": prompts }))
}

/// The files ending in `.ext` up to [depth] directories under [dir] that were written recently.
fn recent_files(dir: &Path, depth: usize, ext: &str) -> Vec<PathBuf> {
    let mut found = vec![];
    let Ok(entries) = std::fs::read_dir(dir) else { return found };
    for entry in entries.flatten() {
        // Not following symlinks keeps the walk inside [dir].
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
    // Days to a civil date (Howard Hinnant's algorithm).
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
        assert_eq!(got["limits"], json!([]));
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
        assert_eq!(got["limits"][0], json!({ "label": "5-hour window", "percent": 0.42, "resets_at": "2027-01-15T08:00:00Z" }));
        assert_eq!(got["limits"][1]["label"], "Weekly");
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
