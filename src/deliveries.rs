//! Files agents send to the user (`herdr-bridge send`): copied here, so the phone gets the very bytes that
//! were sent even if a rebuild replaces them, announced on `/ws/session` as a `delivery`, and fetched by the
//! app when the user opens one. Only this folder is ever served, by id.

use std::io::Read;
use std::path::{Path, PathBuf};

/// Refused above this: an APK or a report is far smaller.
pub const MAX_BYTES: u64 = 200 * 1024 * 1024;

/// How many deliveries stay on disk; each one prunes back down to this many.
pub const KEEP: usize = 10;

/// The deliveries dir under `home`, for tests; [dir] uses the real one.
pub fn dir_in(home: &Path) -> PathBuf {
    home.join(".cache/herdr-bridge/deliveries")
}

/// The deliveries dir on this machine, made 0700 when missing.
pub fn dir() -> PathBuf {
    let dir = dir_in(&PathBuf::from(std::env::var_os("HOME").unwrap_or_default()));
    use std::os::unix::fs::DirBuilderExt;
    let _ = std::fs::DirBuilder::new().mode(0o700).recursive(true).create(&dir);
    dir
}

/// A new delivery's id: 16 hex digits, unguessable, so a file is fetched only by whoever was told of it.
pub fn new_id() -> std::io::Result<String> {
    let mut bytes = [0u8; 8];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Whether `id` is one [new_id] makes, so it can't name anything outside the folder.
pub fn is_id(id: &str) -> bool {
    id.len() == 16 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The file stored for delivery `id`, `<id>-<name>` in [dir].
pub fn find(id: &str) -> Option<PathBuf> {
    let prefix = format!("{id}-");
    std::fs::read_dir(dir()).ok()?.filter_map(|e| e.ok()).map(|e| e.path()).find(|p| {
        p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(&prefix))
    })
}

/// The type the phone opens `name` as: Android picks the app (the installer, a PDF viewer) by it.
pub fn mime_of(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map_or("", |(_, e)| e).to_ascii_lowercase();
    match ext.as_str() {
        "apk" => "application/vnd.android.package-archive",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "mp4" => "video/mp4",
        "mp3" => "audio/mpeg",
        "zip" => "application/zip",
        "html" | "htm" => "text/html",
        "md" | "txt" | "log" => "text/plain",
        "csv" => "text/csv",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_hex_only() {
        let id = new_id().expect("urandom");
        assert!(is_id(&id));
        assert!(!is_id("../../etc/passwd"));
        assert!(!is_id("0123456789ABCDEF"));
        assert!(!is_id("0123456789abcde"));
    }

    #[test]
    fn mime_follows_extension() {
        assert_eq!(mime_of("app-release.APK"), "application/vnd.android.package-archive");
        assert_eq!(mime_of("The Podarcis ecosystem.pdf"), "application/pdf");
        assert_eq!(mime_of("Makefile"), "application/octet-stream");
    }

    #[test]
    fn dir_lives_in_cache() {
        assert_eq!(dir_in(Path::new("/home/u")), PathBuf::from("/home/u/.cache/herdr-bridge/deliveries"));
    }
}
