//! The Obsidian vaults, served for the Obsidian plugin's on-demand sync from a phone: a vault's file
//! list with mtimes, and each file to read, write or delete. The phone names its vault (`?vault=`),
//! and the vault of that name in this machine's Obsidian vault list is the one served, so switching
//! vaults on the phone needs no setup here.
use axum::{
    body::Bytes,
    extract::Query,
    http::{header, StatusCode},
    response::{IntoResponse, Json, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use tokio::fs;

#[derive(Deserialize)]
pub struct VaultQuery {
    pub vault: String,
}

#[derive(Deserialize)]
pub struct FileQuery {
    pub vault: String,
    pub path: String,
}

#[derive(Serialize)]
pub struct VaultFileInfo {
    pub path: String,
    pub mtime: u64,
    pub size: u64,
}

/// Obsidian's list of the vaults it has opened on this machine.
fn obsidian_registry() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
    [config.join("obsidian/obsidian.json"), home.join("Library/Application Support/obsidian/obsidian.json")]
        .into_iter()
        .find(|p| p.is_file())
}

/// The folder of the vault called `name` (its folder's name, as Obsidian shows it) in `registry`.
fn find_vault(registry: &str, name: &str) -> Result<PathBuf, (StatusCode, String)> {
    let registry: Value = serde_json::from_str(registry)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Unreadable Obsidian vault list: {e}")))?;
    let matches: Vec<PathBuf> = registry["vaults"]
        .as_object()
        .into_iter()
        .flat_map(|vaults| vaults.values())
        .filter_map(|v| v["path"].as_str().map(PathBuf::from))
        .filter(|p| p.file_name().is_some_and(|n| n == name))
        .collect();
    match matches.as_slice() {
        [path] => Ok(path.clone()),
        [] => Err((StatusCode::NOT_FOUND, format!("No Obsidian vault named \"{name}\" on this machine: open it in Obsidian here first"))),
        _ => Err((StatusCode::CONFLICT, format!("Several Obsidian vaults are named \"{name}\" on this machine: rename one"))),
    }
}

async fn vault_dir(name: &str) -> Result<PathBuf, (StatusCode, String)> {
    let path = obsidian_registry()
        .ok_or((StatusCode::NOT_FOUND, "No Obsidian vault list on this machine: is Obsidian installed here?".to_string()))?;
    let registry = fs::read_to_string(&path)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to read {}: {e}", path.display())))?;
    find_vault(&registry, name)
}

fn mtime_secs(meta: &Metadata) -> u64 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0)
}

pub async fn get_manifest(Query(q): Query<VaultQuery>) -> Result<Json<Vec<VaultFileInfo>>, (StatusCode, String)> {
    let base_dir = vault_dir(&q.vault).await?;
    let mut files = Vec::new();
    let mut stack = vec![PathBuf::new()];

    while let Some(rel_dir) = stack.pop() {
        let mut entries = match fs::read_dir(base_dir.join(&rel_dir)).await {
            Ok(e) => e,
            Err(_) => continue,
        };

        while let Ok(Some(entry)) = entries.next_entry().await {
            let entry_rel = rel_dir.join(entry.file_name());
            let Ok(file_type) = entry.file_type().await else { continue };
            if file_type.is_dir() {
                if !is_excluded_dir(&entry.file_name().to_string_lossy()) {
                    stack.push(entry_rel);
                }
            } else if file_type.is_file()
                && is_syncable(&entry_rel)
                && let Ok(meta) = entry.metadata().await
            {
                files.push(VaultFileInfo {
                    path: entry_rel.to_string_lossy().to_string(),
                    mtime: mtime_secs(&meta),
                    size: meta.len(),
                });
            }
        }
    }

    Ok(Json(files))
}

pub async fn get_file(Query(q): Query<FileQuery>) -> Result<Response, (StatusCode, String)> {
    let file_path = safe_join(&vault_dir(&q.vault).await?, &q.path)?;

    let bytes = fs::read(&file_path).await.map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => (StatusCode::NOT_FOUND, "File not found".into()),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to read file: {e}")),
    })?;

    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
}

/// Answers the written file's mtime: the phone records it, so its next sync doesn't take its own
/// upload for a change made on this side.
pub async fn post_file(Query(q): Query<FileQuery>, body: Bytes) -> Result<Json<Value>, (StatusCode, String)> {
    let file_path = safe_join(&vault_dir(&q.vault).await?, &q.path)?;

    if let Some(parent) = file_path.parent() {
        fs::create_dir_all(parent).await.map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to create parent directories: {e}"))
        })?;
    }

    // Atomic write: write to temp file then rename
    let tmp_path = file_path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp_path, &body).await.map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to write temp file: {e}"))
    })?;

    fs::rename(&tmp_path, &file_path).await.map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to rename temp file: {e}"))
    })?;

    let meta = fs::metadata(&file_path)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to stat written file: {e}")))?;
    Ok(Json(json!({ "status": "ok", "path": q.path, "mtime": mtime_secs(&meta) })))
}

pub async fn delete_file(Query(q): Query<FileQuery>) -> Result<Json<Value>, (StatusCode, String)> {
    let file_path = safe_join(&vault_dir(&q.vault).await?, &q.path)?;

    if file_path.exists() {
        fs::remove_file(&file_path).await.map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to delete file: {e}"))
        })?;
    }

    Ok(Json(json!({ "status": "ok", "deleted": q.path })))
}

/// Validates that `rel_path` is a synced file within `base_dir`.
fn safe_join(base_dir: &Path, rel_path: &str) -> Result<PathBuf, (StatusCode, String)> {
    let rel = Path::new(rel_path);
    // Disallow absolute paths or parent references
    if rel.is_absolute() || rel.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err((StatusCode::BAD_REQUEST, "Invalid path: parent directory traversal not allowed".into()));
    }
    if !is_syncable(rel) {
        return Err((StatusCode::BAD_REQUEST, "Not a synced vault file".into()));
    }
    Ok(base_dir.join(rel))
}

/// Hidden folders (`.obsidian` too: its settings are per device, and hold the plugin's bridge token)
/// and build or dependency folders.
fn is_excluded_dir(name: &str) -> bool {
    name.starts_with('.') || matches!(name, "node_modules" | "target" | "dist" | "build" | "__pycache__")
}

/// Notes, canvases and media outside [is_excluded_dir] folders. The plugin's `isSyncablePath` matches it.
fn is_syncable(rel_path: &Path) -> bool {
    let mut parts: Vec<String> = rel_path.iter().map(|c| c.to_string_lossy().into_owned()).collect();
    let Some(name) = parts.pop() else { return false };
    if name.starts_with('.') || parts.iter().any(|d| is_excluded_dir(d)) {
        return false;
    }
    let ext = rel_path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_lowercase();
    matches!(ext.as_str(), "md" | "canvas" | "txt" | "json" | "csv" | "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "pdf")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syncs_notes_and_media_outside_hidden_and_build_folders() {
        for p in ["a.md", "notes/deep/b.canvas", "img/c.PNG"] {
            assert!(is_syncable(Path::new(p)), "{p}");
        }
        for p in [".obsidian/plugins/herdr-obsidian/data.json", ".git/x.md", "node_modules/x.md", ".hidden.md", "src/main.rs", "Makefile"] {
            assert!(!is_syncable(Path::new(p)), "{p}");
        }
    }

    #[test]
    fn finds_the_vault_named_by_the_phone_in_obsidians_list() {
        let registry = r#"{"vaults":{
            "a1":{"path":"/home/u/Documents/Vaults/Personal","ts":1,"open":true},
            "b2":{"path":"/home/u/Documents/Vaults/Newsletters","ts":2},
            "c3":{"path":"/home/u/work/Notes","ts":3},
            "d4":{"path":"/home/u/old/Notes","ts":4}}}"#;
        assert_eq!(find_vault(registry, "Personal").unwrap(), PathBuf::from("/home/u/Documents/Vaults/Personal"));
        assert_eq!(find_vault(registry, "Missing").unwrap_err().0, StatusCode::NOT_FOUND);
        assert_eq!(find_vault(registry, "Notes").unwrap_err().0, StatusCode::CONFLICT);
        assert_eq!(find_vault(registry, "Vaults").unwrap_err().0, StatusCode::NOT_FOUND);
    }

    #[test]
    fn refuses_paths_outside_the_vault_or_not_synced() {
        let base = Path::new("/vault");
        assert_eq!(safe_join(base, "a/b.md").unwrap(), PathBuf::from("/vault/a/b.md"));
        for p in ["../x.md", "/etc/passwd", ".obsidian/app.json", "run.sh"] {
            assert!(safe_join(base, p).is_err(), "{p}");
        }
    }
}
