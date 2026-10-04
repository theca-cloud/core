//! The JWT signing key: read from its file, or generated into it when empty.

use std::path::Path;

use crate::Error;

/// HS256 key (32 bytes). Signing comes with authentication.
#[derive(Clone)]
pub struct JwtKey(#[allow(dead_code)] [u8; 32]);

impl std::fmt::Debug for JwtKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JwtKey(<redacted>)")
    }
}

impl JwtKey {
    /// Loads the key from `path` (64 hex characters). If the file is empty,
    /// generates a key and writes it there.
    pub fn load_or_generate(path: &Path) -> Result<Self, Error> {
        let content = std::fs::read_to_string(path).map_err(|source| Error::SecretFile {
            name: "JWT_KEY__FILE",
            path: path.to_owned(),
            source,
        })?;
        let content = content.trim();
        if content.is_empty() {
            let mut key = [0u8; 32];
            getrandom::fill(&mut key)
                .map_err(|e| Error::Io(std::io::Error::other(e.to_string())))?;
            std::fs::write(path, hex(&key))
                .map_err(|e| Error::JwtKeyNotWritable(path.to_owned(), e))?;
            tracing::info!(path = %path.display(), "generated a new JWT key");
            return Ok(Self(key));
        }
        let invalid = || Error::InvalidJwtKey(path.to_owned());
        if content.len() != 64 {
            return Err(invalid());
        }
        let mut key = [0u8; 32];
        for (i, byte) in key.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&content[2 * i..2 * i + 2], 16).map_err(|_| invalid())?;
        }
        Ok(Self(key))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str, content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("theca-jwt-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("key");
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn generates_into_empty_file_then_reloads_it() {
        let path = temp_file("gen", "");
        let generated = JwtKey::load_or_generate(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().len(), 64);
        let reloaded = JwtKey::load_or_generate(&path).unwrap();
        assert_eq!(generated.0, reloaded.0);
    }

    #[test]
    fn rejects_invalid_content() {
        let path = temp_file("bad", "not a key");
        assert!(matches!(
            JwtKey::load_or_generate(&path),
            Err(Error::InvalidJwtKey(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn empty_and_read_only_is_an_error() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_file("ro", "");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        // Root (e.g. in CI containers) can write to read-only files: nothing to test there.
        if std::fs::OpenOptions::new().write(true).open(&path).is_ok() {
            eprintln!("skipped: the file is still writable (running as root?)");
            return;
        }
        assert!(matches!(
            JwtKey::load_or_generate(&path),
            Err(Error::JwtKeyNotWritable(..))
        ));
    }
}
