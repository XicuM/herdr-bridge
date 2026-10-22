//! The Obsidian vaults, served for the Obsidian plugin's sync from a phone: a vault's file list with
//! mtimes, and each file to read, write or delete. The phone names its vault (`?vault=`), and the
//! vault of that name in this machine's Obsidian vault list is the one served, so switching vaults on
//! the phone needs no setup here. [Watcher] pushes the files that change, so the phone needn't ask.
use axum::{
    body::Bytes,
    extract::{ws::Utf8Bytes, Query},
    http::{header, StatusCode},
    response::{IntoResponse, Json, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant, UNIX_EPOCH};
use tokio::fs;
use tokio::sync::broadcast;
use tracing::warn;

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
    /// Two writes within a second share an `mtime`; the plugin tells them apart by this.
    pub mtime_ms: u64,
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

/// The vault folders in Obsidian's vault list `registry`.
fn vault_roots(registry: &str) -> Result<Vec<PathBuf>, (StatusCode, String)> {
    let registry: Value = serde_json::from_str(registry)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Unreadable Obsidian vault list: {e}")))?;
    Ok(registry["vaults"]
        .as_object()
        .into_iter()
        .flat_map(|vaults| vaults.values())
        .filter_map(|v| v["path"].as_str().map(PathBuf::from))
        .collect())
}

/// The folder of the vault called `name` (its folder's name, as Obsidian shows it) in `registry`.
fn find_vault(registry: &str, name: &str) -> Result<PathBuf, (StatusCode, String)> {
    let matches: Vec<PathBuf> = vault_roots(registry)?.into_iter().filter(|p| p.file_name().is_some_and(|n| n == name)).collect();
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

/// The vault (its folder's name, as the plugin knows it) in `registry` that holds `file`, and the
/// file's path inside it. The innermost vault wins when one sits inside another.
fn find_vault_of(registry: &str, file: &Path) -> Result<(String, String), (StatusCode, String)> {
    vault_roots(registry)?
        .into_iter()
        .filter_map(|root| Some((root.file_name()?.to_string_lossy().into_owned(), file.strip_prefix(&root).ok()?.to_string_lossy().into_owned(), root)))
        .max_by_key(|(_, _, root)| root.components().count())
        .map(|(name, rel, _)| (name, rel))
        .ok_or((StatusCode::NOT_FOUND, format!("{} is not inside an Obsidian vault on this machine", file.display())))
}

pub async fn vault_of(file: &str) -> Result<(String, String), (StatusCode, String)> {
    let file = Path::new(file);
    // `..` would climb out of the vault after the prefix check.
    if !file.is_absolute() || file.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err((StatusCode::BAD_REQUEST, "Invalid path: absolute, without `..`".into()));
    }
    let path = obsidian_registry()
        .ok_or((StatusCode::NOT_FOUND, "No Obsidian vault list on this machine: is Obsidian installed here?".to_string()))?;
    let registry = fs::read_to_string(&path)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to read {}: {e}", path.display())))?;
    find_vault_of(&registry, file)
}

fn mtime_secs(meta: &Metadata) -> u64 {
    mtime_ms(meta) / 1000
}

fn mtime_ms(meta: &Metadata) -> u64 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Watches every vault in Obsidian's list and pushes `{"type":"vault_changed","vault","files"}` for
/// the synced files that changed there, each as the manifest lists it or `{"path","deleted":true}`.
/// The phone applies just those, instead of fetching the whole manifest for each agent's edit.
pub struct Watcher {
    pub changes: broadcast::Sender<Utf8Bytes>,
    /// Each watched vault's folder and name.
    roots: Arc<Mutex<Vec<(PathBuf, String)>>>,
    watcher: Mutex<Option<RecommendedWatcher>>,
}

impl Watcher {
    pub fn new() -> Arc<Self> {
        let changes = broadcast::channel(64).0;
        let roots: Arc<Mutex<Vec<(PathBuf, String)>>> = Default::default();
        let (tx, watched) = (changes.clone(), roots.clone());
        let (events_tx, events) = std::sync::mpsc::channel();
        let watcher = notify::recommended_watcher(events_tx).map_err(|e| warn!("Vault changes won't be pushed: {e}")).ok();
        std::thread::spawn(move || {
            // An agent's edit is a burst of writes: half a second of quiet (or two seconds of
            // writing) makes it one message.
            let mut pending = BTreeSet::new();
            let (mut first, mut last) = (Instant::now(), Instant::now());
            loop {
                let event = if pending.is_empty() {
                    events.recv().map_err(|_| RecvTimeoutError::Disconnected)
                } else {
                    events.recv_timeout((last + Duration::from_millis(500)).min(first + Duration::from_secs(2)).saturating_duration_since(Instant::now()))
                };
                match event {
                    // Reading a file or listing a folder (as [changes_messages] does) is no change.
                    Ok(Ok(event)) if !matches!(event.kind, EventKind::Access(_)) => {
                        if pending.is_empty() {
                            first = Instant::now();
                        }
                        last = Instant::now();
                        pending.extend(event.paths);
                    }
                    Ok(_) => {}
                    Err(RecvTimeoutError::Timeout) => {
                        let roots = watched.lock().map(|r| r.clone()).unwrap_or_default();
                        for message in changes_messages(&roots, std::mem::take(&mut pending).into_iter()) {
                            // No phone connected: nobody to tell, and its next full sync catches up.
                            let _ = tx.send(message);
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        });
        Arc::new(Self { changes, roots, watcher: Mutex::new(watcher) })
    }

    /// Watches the vaults in Obsidian's list not watched yet. Called as a client connects, so a vault
    /// opened in Obsidian since the bridge started is picked up. Blocking: watching a folder walks it.
    pub fn watch_vaults(&self) {
        let Some(registry) = obsidian_registry().and_then(|p| std::fs::read_to_string(p).ok()) else { return };
        let Ok(found) = vault_roots(&registry) else { return };
        let (Ok(mut watcher), Ok(mut roots)) = (self.watcher.lock(), self.roots.lock()) else { return };
        let Some(watcher) = watcher.as_mut() else { return };
        for root in found {
            let Some(name) = root.file_name().map(|n| n.to_string_lossy().into_owned()) else { continue };
            if roots.iter().any(|(r, _)| *r == root) {
                continue;
            }
            match watcher.watch(&root, RecursiveMode::Recursive) {
                Ok(()) => roots.push((root, name)),
                Err(e) => warn!("Not watching vault {}: {e}", root.display()),
            }
        }
    }
}

/// The `vault_changed` messages for `paths` that changed on disk, one per vault, with each synced file
/// as it is now. A path that can't be read for another reason than being gone is left out, never
/// reported deleted: the phone would delete its copy.
fn changes_messages(roots: &[(PathBuf, String)], paths: impl Iterator<Item = PathBuf>) -> Vec<Utf8Bytes> {
    let mut by_vault: BTreeMap<&str, BTreeMap<String, Value>> = BTreeMap::new();
    for path in paths {
        // Every vault holding it: a vault inside another is in the outer one's manifest too.
        for (root, name) in roots {
            let Ok(rel) = path.strip_prefix(root) else { continue };
            let files = match std::fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_file() && is_syncable(rel) => vec![json!(file_info(rel, &meta))],
                // A folder made or moved in: notes written into it before it was watched raised nothing.
                Ok(meta) if meta.is_dir() && !rel.as_os_str().is_empty() && !rel.iter().any(|d| is_excluded_dir(&d.to_string_lossy())) => {
                    synced_files(root, rel.to_path_buf()).iter().map(|f| json!(f)).collect()
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && is_syncable(rel) => {
                    vec![json!({ "path": rel.to_string_lossy(), "deleted": true })]
                }
                _ => continue,
            };
            for file in files {
                by_vault.entry(name).or_default().insert(file["path"].as_str().unwrap_or_default().to_string(), file);
            }
        }
    }
    by_vault
        .into_iter()
        .map(|(vault, files)| json!({ "type": "vault_changed", "vault": vault, "files": files.into_values().collect::<Vec<_>>() }).to_string().into())
        .collect()
}

pub async fn get_manifest(Query(q): Query<VaultQuery>) -> Result<Json<Vec<VaultFileInfo>>, (StatusCode, String)> {
    let base_dir = vault_dir(&q.vault).await?;
    tokio::task::spawn_blocking(move || synced_files(&base_dir, PathBuf::new()))
        .await
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to list the vault: {e}")))
}

/// The synced files in `base_dir`'s folder `rel_dir` and below it.
fn synced_files(base_dir: &Path, rel_dir: PathBuf) -> Vec<VaultFileInfo> {
    let mut files = Vec::new();
    let mut stack = vec![rel_dir];
    while let Some(rel_dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(base_dir.join(&rel_dir)) else { continue };
        for entry in entries.flatten() {
            let entry_rel = rel_dir.join(entry.file_name());
            let Ok(file_type) = entry.file_type() else { continue };
            if file_type.is_dir() {
                if !is_excluded_dir(&entry.file_name().to_string_lossy()) {
                    stack.push(entry_rel);
                }
            } else if file_type.is_file()
                && is_syncable(&entry_rel)
                && let Ok(meta) = entry.metadata()
            {
                files.push(file_info(&entry_rel, &meta));
            }
        }
    }
    files
}

fn file_info(rel: &Path, meta: &Metadata) -> VaultFileInfo {
    VaultFileInfo { path: rel.to_string_lossy().into_owned(), mtime: mtime_secs(meta), mtime_ms: mtime_ms(meta), size: meta.len() }
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
    Ok(Json(json!({ "status": "ok", "path": q.path, "mtime": mtime_secs(&meta), "mtime_ms": mtime_ms(&meta) })))
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
    fn finds_the_vault_holding_a_file() {
        let registry = r#"{"vaults":{"a":{"path":"/home/u/Notes"},"b":{"path":"/home/u/Notes/Inner"}}}"#;
        let found = |f: &str| find_vault_of(registry, Path::new(f)).ok();
        assert_eq!(found("/home/u/Notes/a/b.md"), Some(("Notes".into(), "a/b.md".into())));
        assert_eq!(found("/home/u/Notes/Inner/c.md"), Some(("Inner".into(), "c.md".into())));
        assert_eq!(found("/home/u/Notes2/c.md"), None);
        assert_eq!(found("/etc/passwd"), None);
    }

    #[test]
    fn pushes_the_synced_files_that_changed_in_each_vault() {
        let base = std::env::temp_dir().join(format!("herdr-bridge-vault-test-{}", std::process::id()));
        let (notes, inner) = (base.join("Notes"), base.join("Notes/Inner"));
        std::fs::create_dir_all(notes.join(".obsidian")).unwrap();
        std::fs::create_dir_all(&inner).unwrap();
        for f in ["a.md", ".obsidian/workspace.json", "a.md.tmp.123", "Inner/b.md"] {
            std::fs::write(notes.join(f), "x").unwrap();
        }
        // A vault whose only change is an empty folder gets no message.
        std::fs::create_dir_all(base.join("Other/empty")).unwrap();
        let roots = vec![(notes.clone(), "Notes".to_string()), (inner.clone(), "Inner".to_string()), (base.join("Other"), "Other".to_string())];
        std::fs::create_dir_all(notes.join("fresh/deeper")).unwrap();
        std::fs::write(notes.join("fresh/deeper/c.md"), "x").unwrap();
        let changed = ["a.md", "a.md", ".obsidian/workspace.json", "a.md.tmp.123", "gone.md", "Inner/b.md", "fresh", ".obsidian"].map(|f| notes.join(f));
        let messages: Vec<Value> = changes_messages(&roots, changed.into_iter().chain([base.join("elsewhere.md"), base.join("Other/empty")]))
            .iter()
            .map(|m| serde_json::from_str(m.as_str()).unwrap())
            .collect();
        std::fs::remove_dir_all(&base).unwrap();

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["vault"], "Inner");
        assert_eq!(messages[0]["files"][0]["path"], "b.md");
        assert_eq!(messages[1]["vault"], "Notes");
        let files = messages[1]["files"].as_array().unwrap();
        assert_eq!(files.len(), 4, "{files:?}");
        assert_eq!(files[0]["path"], "Inner/b.md");
        assert_eq!(files[1]["path"], "a.md");
        assert_eq!(files[1]["size"], 1);
        assert_eq!(files[1]["mtime"], files[1]["mtime_ms"].as_u64().unwrap() / 1000);
        assert_eq!(files[2]["path"], "fresh/deeper/c.md");
        assert_eq!(files[3], json!({ "path": "gone.md", "deleted": true }));
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
