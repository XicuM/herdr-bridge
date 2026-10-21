use axum::{
    body::Bytes,
    extract::{Extension, Query},
    http::{header, StatusCode},
    response::{IntoResponse, Json, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use tokio::fs;
use crate::server::AppState;

#[derive(Deserialize)]
pub struct ManifestQuery {
    pub workspace_id: String,
}

#[derive(Deserialize)]
pub struct FileQuery {
    pub workspace_id: String,
    pub path: String,
}

#[derive(Serialize)]
pub struct VaultFileInfo {
    pub path: String,
    pub mtime: u64,
    pub size: u64,
}

/// Resolves the absolute directory path of a workspace from its snapshot.
async fn resolve_workspace_dir(state: &AppState, workspace_id: &str) -> Result<PathBuf, (StatusCode, String)> {
    let snapshot = state.herdr.snapshot().await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    
    // 1. Look in panes
    if let Some(panes) = snapshot.get("panes").and_then(|p| p.as_array()) {
        for pane in panes {
            if pane.get("workspace_id").and_then(|w| w.as_str()) == Some(workspace_id) {
                if let Some(cwd) = pane.get("cwd").or_else(|| pane.get("foreground_cwd")).and_then(|c| c.as_str()) {
                    let path = PathBuf::from(cwd);
                    if path.is_dir() {
                        return Ok(path);
                    }
                }
            }
        }
    }

    // 2. Look in workspaces (worktree checkout_path)
    if let Some(workspaces) = snapshot.get("workspaces").and_then(|w| w.as_array()) {
        for ws in workspaces {
            if ws.get("workspace_id").and_then(|w| w.as_str()) == Some(workspace_id) {
                if let Some(checkout) = ws.get("worktree").and_then(|wt| wt.get("checkout_path")).and_then(|c| c.as_str()) {
                    let path = PathBuf::from(checkout);
                    if path.is_dir() {
                        return Ok(path);
                    }
                }
            }
        }
    }

    Err((StatusCode::NOT_FOUND, format!("Workspace {workspace_id} not found or has no active cwd")))
}

/// Validates that `rel_path` is safe and within `base_dir`.
fn safe_join(base_dir: &Path, rel_path: &str) -> Result<PathBuf, (StatusCode, String)> {
    let rel = Path::new(rel_path);
    // Disallow absolute paths or parent references
    if rel.is_absolute() || rel.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err((StatusCode::BAD_REQUEST, "Invalid path: parent directory traversal not allowed".into()));
    }
    Ok(base_dir.join(rel))
}

/// Checks if a file path is a syncable vault file (notes, canvas, media, configs).
fn is_syncable(rel_path: &Path) -> bool {
    for c in rel_path.components() {
        let s = c.as_os_str().to_string_lossy();
        if s == ".git" || s == "node_modules" || s == "target" || s == ".venv" || s == "dist" || s == "build" || s == "__pycache__" {
            return false;
        }
        // Exclude hidden folders except .obsidian
        if s.starts_with('.') && s != ".obsidian" {
            return false;
        }
    }

    if let Some(ext) = rel_path.extension().and_then(|e| e.to_str()) {
        let ext = ext.to_lowercase();
        matches!(
            ext.as_str(),
            "md" | "canvas" | "txt" | "json" | "csv" | "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "pdf"
        )
    } else {
        false
    }
}

pub async fn get_manifest(
    Extension(state): Extension<AppState>,
    Query(q): Query<ManifestQuery>,
) -> Result<Json<Vec<VaultFileInfo>>, (StatusCode, String)> {
    let base_dir = resolve_workspace_dir(&state, &q.workspace_id).await?;
    let mut files = Vec::new();
    let mut stack = vec![PathBuf::new()];

    while let Some(rel_dir) = stack.pop() {
        let current_dir = base_dir.join(&rel_dir);
        let mut entries = match fs::read_dir(&current_dir).await {
            Ok(e) => e,
            Err(_) => continue,
        };

        while let Ok(Some(entry)) = entries.next_entry().await {
            let file_name = entry.file_name();
            let name_str = file_name.to_string_lossy();
            
            // Fast skip obvious directories
            if name_str == ".git" || name_str == "node_modules" || name_str == "target" || name_str == ".venv" || name_str == "dist" {
                continue;
            }

            let entry_rel = rel_dir.join(&file_name);
            let file_type = match entry.file_type().await {
                Ok(t) => t,
                Err(_) => continue,
            };

            if file_type.is_dir() {
                if !name_str.starts_with('.') || name_str == ".obsidian" {
                    stack.push(entry_rel);
                }
            } else if file_type.is_file() && is_syncable(&entry_rel) {
                if let Ok(meta) = entry.metadata().await {
                    let mtime = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    files.push(VaultFileInfo {
                        path: entry_rel.to_string_lossy().to_string(),
                        mtime,
                        size: meta.len(),
                    });
                }
            }
        }
    }

    Ok(Json(files))
}

pub async fn get_file(
    Extension(state): Extension<AppState>,
    Query(q): Query<FileQuery>,
) -> Result<Response, (StatusCode, String)> {
    let base_dir = resolve_workspace_dir(&state, &q.workspace_id).await?;
    let file_path = safe_join(&base_dir, &q.path)?;

    let bytes = fs::read(&file_path).await.map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => (StatusCode::NOT_FOUND, "File not found".into()),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to read file: {e}")),
    })?;

    let mime = if q.path.ends_with(".md") {
        "text/markdown; charset=utf-8"
    } else if q.path.ends_with(".json") || q.path.ends_with(".canvas") {
        "application/json"
    } else if q.path.ends_with(".png") {
        "image/png"
    } else if q.path.ends_with(".jpg") || q.path.ends_with(".jpeg") {
        "image/jpeg"
    } else {
        "application/octet-stream"
    };

    Ok((
        [(header::CONTENT_TYPE, mime)],
        bytes,
    ).into_response())
}

pub async fn post_file(
    Extension(state): Extension<AppState>,
    Query(q): Query<FileQuery>,
    body: Bytes,
) -> Result<Json<Value>, (StatusCode, String)> {
    let base_dir = resolve_workspace_dir(&state, &q.workspace_id).await?;
    let file_path = safe_join(&base_dir, &q.path)?;

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

    Ok(Json(json!({ "status": "ok", "path": q.path })))
}

pub async fn delete_file(
    Extension(state): Extension<AppState>,
    Query(q): Query<FileQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let base_dir = resolve_workspace_dir(&state, &q.workspace_id).await?;
    let file_path = safe_join(&base_dir, &q.path)?;

    if file_path.exists() {
        fs::remove_file(&file_path).await.map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to delete file: {e}"))
        })?;
    }

    Ok(Json(json!({ "status": "ok", "deleted": q.path })))
}
