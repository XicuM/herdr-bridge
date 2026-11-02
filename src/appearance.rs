//! The user's light/dark appearance, as herdr-obsidian sees it.
//!
//! herdr-obsidian posts it (`POST /api/appearance`); this records it and sets `theme.mode` in every
//! OpenCode CLI config reachable from this machine: the machine's own
//! (`~/.config/opencode/cli.json`), and, when `HERDR_BRIDGE_NEST_DATA` names a PodarcisNest data
//! dir, each user's `opencode-cli.json`, which PodarcisNest bind-mounts into the user container's
//! `/home/coder/.config/opencode/cli.json`. OpenCode reloads a changed `cli.json` while it runs, so
//! every running instance re-themes without a restart. A container PodarcisNest starts later is
//! seeded from `appearance.json`, the last mode, so it begins in the right appearance too.

use serde_json::{json, Value};
use std::path::Path;

/// "light" or "dark", from a request body; anything else is a 400.
pub fn mode(given: &str) -> Result<&'static str, String> {
    match given {
        "light" => Ok("light"),
        "dark" => Ok("dark"),
        _ => Err("mode must be \"light\" or \"dark\"".into()),
    }
}

/// The last mode this bridge applied, for `GET /api/appearance` and to skip repeat writes.
pub fn current() -> Option<String> {
    std::fs::read_to_string(state_file()).ok().and_then(|text| {
        let mode = text.trim();
        (mode == "light" || mode == "dark").then(|| mode.to_string())
    })
}

/// Sets `theme.mode` in every reachable OpenCode config, then records it as the current mode.
/// The writes keep everything else in each file; a failure keeps the old mode recorded nowhere,
/// so the next appearance change (or the app reconnecting) tries again.
pub fn apply(mode: &str) {
    if let Some(config) = config_dir() {
        let dir = config.join("opencode");
        // Only where this machine has an OpenCode: creating a config for a machine without one
        // would leave a file nobody reads.
        if dir.is_dir() {
            let _ = set_theme_mode(&dir.join("cli.json"), mode);
        }
    }
    if let Ok(data) = std::env::var("HERDR_BRIDGE_NEST_DATA") {
        let data = Path::new(&data);
        let _ = atomic_write(&data.join("appearance.json"), json!({ "mode": mode }));
        // Each live user container sees its file through the bind mount; one not started yet is
        // created by PodarcisNest from appearance.json when it starts.
        if let Ok(users) = std::fs::read_dir(data.join("users")) {
            for user in users.flatten() {
                let cli = user.path().join("opencode-cli.json");
                if cli.is_file() {
                    let _ = set_theme_mode(&cli, mode);
                }
            }
        }
    }
    if let Some(parent) = state_file().parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(state_file(), mode);
}

fn state_file() -> std::path::PathBuf {
    config_dir().unwrap_or_default().join("herdr-bridge/appearance")
}

fn config_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".config")))
}

/// Reads `cli.json` (creating one if missing), sets `theme.mode` inside it, and writes it back
/// atomically, keeping every other key. A file that is not a JSON object or won't parse is left
/// alone: OpenCode ignores it too, and clobbering a file we can't read back would be worse.
fn set_theme_mode(path: &Path, mode: &str) -> std::io::Result<()> {
    let mut value: Value = json!({});
    if path.exists() {
        let text = std::fs::read_to_string(path)?;
        let parsed: Value = serde_json::from_str(&text)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if !parsed.is_object() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "not a JSON object",
            ));
        }
        value = parsed;
    }
    let object = value.as_object_mut().expect("checked is_object");
    let theme = match object.get_mut("theme") {
        Some(Value::Object(theme)) => theme,
        _ => {
            object.insert("theme".to_string(), json!({}));
            object
                .get_mut("theme")
                .and_then(Value::as_object_mut)
                .expect("just inserted an object")
        }
    };
    theme.insert("mode".to_string(), json!(mode));
    if object.get("$schema").is_none() {
        object.insert("$schema".to_string(), json!("https://opencode.ai/v2/cli.json"));
    }
    atomic_write(path, value)
}

/// Writes through a sibling temp file and renames over the target, so OpenCode's watch never reads
/// a half-written file.
fn atomic_write(path: &Path, value: Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut temp = path.as_os_str().to_os_string();
    temp.push(format!(".{}", std::process::id()));
    std::fs::write(&temp, serde_json::to_string_pretty(&value).unwrap() + "\n")?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_mode(path: &Path, mode: &str) -> std::io::Result<()> {
        set_theme_mode(path, mode)
    }

    #[test]
    fn creates_a_config_with_the_mode() {
        let dir = std::env::temp_dir().join(format!("appearance-{}", std::process::id()));
        let path = dir.join("cli.json");
        set_mode(&path, "light").unwrap();
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["theme"]["mode"], "light");
        assert_eq!(value["$schema"], "https://opencode.ai/v2/cli.json");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn keeps_other_keys_and_theme_name() {
        let dir = std::env::temp_dir().join(format!("appearance-keep-{}", std::process::id()));
        let path = dir.join("cli.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, r#"{"theme":{"name":"catppuccin","mode":"dark"},"animations":false}"#).unwrap();
        set_mode(&path, "light").unwrap();
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["theme"]["name"], "catppuccin");
        assert_eq!(value["theme"]["mode"], "light");
        assert_eq!(value["animations"], false);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn leaves_an_invalid_file_untouched() {
        let dir = std::env::temp_dir().join(format!("appearance-bad-{}", std::process::id()));
        let path = dir.join("cli.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "not json").unwrap();
        assert!(set_mode(&path, "light").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json");
        std::fs::remove_dir_all(&dir).ok();
    }
}
