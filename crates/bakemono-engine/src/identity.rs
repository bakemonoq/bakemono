use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use nostr::prelude::*;

#[derive(Clone)]
pub struct Identity {
    keys: Keys,
}

impl Identity {
    pub fn generate() -> Self {
        Self {
            keys: Keys::generate(),
        }
    }

    pub fn import(nsec: &str) -> Result<Self> {
        Ok(Self {
            keys: Keys::parse(nsec.trim()).context("parsing nsec")?,
        })
    }

    pub fn keys(&self) -> &Keys {
        &self.keys
    }

    pub fn npub(&self) -> Result<String> {
        Ok(self.keys.public_key().to_bech32()?)
    }

    pub fn nsec(&self) -> Result<String> {
        Ok(self.keys.secret_key().to_bech32()?)
    }

    // env override first so a test or operator can pin a key without touching disk
    pub fn load_or_generate(path: &Path) -> Result<Self> {
        if let Ok(nsec) = std::env::var("BAKEMONO_NSEC") {
            return Self::import(&nsec);
        }
        if let Some(existing) = Self::load(path)? {
            return Ok(existing);
        }
        let identity = Self::generate();
        identity.save(path)?;
        Ok(identity)
    }

    pub fn load(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let nsec = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        Ok(Some(Self::import(&nsec)?))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, self.nsec()?)
            .with_context(|| format!("writing {}", path.display()))?;
        restrict_permissions(path)?;
        Ok(())
    }
}

pub fn key_path() -> PathBuf {
    if let Ok(p) = std::env::var("BAKEMONO_KEY_FILE") {
        return PathBuf::from(p);
    }
    super::data_dir().join("identity.nsec")
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_export_round_trips() {
        let id = Identity::generate();
        let nsec = id.nsec().unwrap();
        let again = Identity::import(&nsec).unwrap();
        assert_eq!(id.npub().unwrap(), again.npub().unwrap());
    }

    #[test]
    fn load_returns_none_when_missing() {
        let path = std::env::temp_dir().join("bakemono-no-such-key.nsec");
        assert!(Identity::load(&path).unwrap().is_none());
    }
}
