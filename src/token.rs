//! The token the app sends with every request (`Authorization: Bearer <token>`). Tailscale decides who
//! can reach the bridge, but not which user on a reachable machine: without it, anyone else logged in to
//! this one (or to any device on the tailnet) could type into every pane as you.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// No 0/O/1/L/I, so it can be read off a screen and typed on a phone.
const ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";

/// The token in [path], made (readable by this user only) when there is none. One that others can read
/// is refused, since anyone who can read it can use the bridge.
pub fn load_or_create(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    if let Ok(meta) = fs::metadata(path) {
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(format!("{path:?} can be read by other users; run chmod 600 on it").into());
        }
        let token = fs::read_to_string(path)?.trim().to_string();
        if token.len() < 8 {
            return Err(format!("the token in {path:?} is shorter than 8 characters").into());
        }
        return Ok(token);
    }
    if let Some(dir) = path.parent() {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    // 8 characters of 31: about 40 bits, enough as the server answers one wrong token a second.
    // Rejection sampling keeps every character equally likely.
    let mut random = fs::File::open("/dev/urandom")?;
    let mut chars = Vec::new();
    let mut byte = [0u8];
    while chars.len() < 8 {
        random.read_exact(&mut byte)?;
        if (byte[0] as usize) < 256 - 256 % ALPHABET.len() {
            chars.push(ALPHABET[byte[0] as usize % ALPHABET.len()]);
        }
    }
    // In groups of four, e.g. `K3MF-9QXA`, to type it.
    let token = chars.chunks(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join("-");
    // Written aside and linked into place, so a bridge starting at the same time (the service, and
    // `--print-token` from install.sh) never reads half a token, and the first one made wins.
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    writeln!(file, "{token}")?;
    drop(file);
    let linked = fs::hard_link(&tmp, path);
    let _ = fs::remove_file(&tmp);
    match linked {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => load_or_create(path),
        Err(e) => Err(e.into()),
        Ok(()) => Ok(token),
    }
}

/// Whether [given] is [token], in time that doesn't depend on where they differ. Spaces and dashes are
/// ignored and case too, so a token typed by hand in groups still matches.
pub fn matches(token: &str, given: &str) -> bool {
    let norm = |s: &str| s.bytes().filter(|b| !b" -\t".contains(b)).map(|b| b.to_ascii_lowercase()).collect::<Vec<u8>>();
    let (a, b) = (norm(token), norm(given));
    a.len() == b.len() && a.iter().zip(&b).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    #[test]
    fn matches_ignores_case_spaces_and_dashes_only() {
        assert!(super::matches("k3mf9q-abcdef", "K3MF9Q abcdef"));
        assert!(!super::matches("k3mf9q-abcdef", "k3mf9q-abcdeg"));
        assert!(!super::matches("k3mf9q-abcdef", "k3mf9q"));
        assert!(!super::matches("k3mf9q-abcdef", ""));
    }
}
