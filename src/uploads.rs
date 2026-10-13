//! Image uploads for `@`-references: the app POSTs image bytes, the bridge saves them where the
//! agent runs, and answers the path to put after `@`. From then on it is plain text, so every
//! agent either reads the image or just sees a path; nothing binary ever reaches a pane.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

/// Refused above this: a downscaled phone photo is far smaller, so bigger is a mistake, not an image.
pub const MAX_BYTES: usize = 10 * 1024 * 1024;

/// How many uploads stay on disk; each upload prunes back down to this many.
const KEEP: usize = 100;

fn extension_of(mime: &str) -> Option<&'static str> {
    match mime.to_ascii_lowercase().as_str() {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

/// Whether `bytes` start like `mime` says: only image data is ever stored.
pub fn magic_ok(mime: &str, bytes: &[u8]) -> bool {
    match mime.to_ascii_lowercase().as_str() {
        "image/png" => bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
        "image/jpeg" => bytes.starts_with(&[0xFF, 0xD8, 0xFF]),
        "image/gif" => bytes.starts_with(b"GIF8"),
        "image/webp" => bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
        _ => false,
    }
}

/// A unique file name for `mime` (`None` for anything but an image mime): safe characters only, so
/// it can travel through a remote shell as one quoted argument.
pub fn file_name(mime: &str) -> Option<String> {
    let ext = extension_of(mime)?;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let millis = SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
    Some(format!("herdr-image-{millis}-{}-{}.{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed), ext))
}

/// The uploads dir under `home`, for tests; [dir] uses the real one.
pub fn dir_in(home: &Path) -> PathBuf {
    home.join(".cache/herdr-bridge/uploads")
}

/// The uploads dir on this machine, made 0700 when missing.
pub fn dir() -> PathBuf {
    let dir = dir_in(&PathBuf::from(std::env::var_os("HOME").unwrap_or_default()));
    use std::os::unix::fs::DirBuilderExt;
    let _ = std::fs::DirBuilder::new().mode(0o700).recursive(true).create(&dir);
    dir
}

/// Which of `entries` (path, modified) to delete to keep the newest `keep`.
pub fn to_prune(mut entries: Vec<(PathBuf, SystemTime)>, keep: usize) -> Vec<PathBuf> {
    entries.sort_by_key(|(_, modified)| *modified);
    entries.truncate(entries.len().saturating_sub(keep));
    entries.into_iter().map(|(path, _)| path).collect()
}

/// Prunes `dir` back to the newest [KEEP] uploads; failures are ignored, since a full disk of old
/// images is untidy, not an error.
pub fn prune(dir: PathBuf) {
    let Ok(listing) = std::fs::read_dir(&dir) else { return };
    let entries: Vec<(PathBuf, SystemTime)> = listing
        .filter_map(|entry| entry.ok())
        .map(|entry| {
            let modified = entry.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
            (entry.path(), modified)
        })
        .collect();
    for path in to_prune(entries, KEEP) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn names_images_uniquely() {
        let a = file_name("image/png").expect("png");
        let b = file_name("image/png").expect("png");
        assert!(a.ends_with(".png") && a.starts_with("herdr-image-") && a != b);
        assert!(file_name("IMAGE/JPEG").expect("case").ends_with(".jpg"));
        assert!(file_name("text/plain").is_none());
        assert!(file_name("").is_none());
    }

    #[test]
    fn magic_matches_mime() {
        let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00];
        assert!(magic_ok("image/png", &png));
        assert!(!magic_ok("image/jpeg", &png));
        assert!(magic_ok("image/jpeg", &[0xFF, 0xD8, 0xFF, 0x00]));
        assert!(magic_ok("image/gif", b"GIF89a..."));
        assert!(magic_ok("image/webp", b"RIFF\x00\x00\x00\x00WEBP..."));
        assert!(!magic_ok("image/webp", b"RIFF\x00\x00\x00\x00JPEG..."));
        assert!(!magic_ok("image/png", &[]));
        assert!(!magic_ok("application/octet-stream", &png));
    }

    #[test]
    fn prune_keeps_newest() {
        let at = |secs: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
        let entries: Vec<(PathBuf, SystemTime)> =
            (0..5).map(|i| (PathBuf::from(format!("f{i}")), at(i))).collect();
        assert_eq!(to_prune(entries, 2), [PathBuf::from("f0"), PathBuf::from("f1"), PathBuf::from("f2")]);
        let entries: Vec<(PathBuf, SystemTime)> =
            (0..2).map(|i| (PathBuf::from(format!("f{i}")), at(i))).collect();
        assert!(to_prune(entries, 5).is_empty());
    }

    #[test]
    fn dir_lives_in_cache() {
        assert_eq!(dir_in(Path::new("/home/u")), PathBuf::from("/home/u/.cache/herdr-bridge/uploads"));
    }
}
