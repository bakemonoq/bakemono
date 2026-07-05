use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use anyhow::{anyhow, Context, Result};
use rand::rngs::OsRng;
use rand::RngCore;
use rsa::pkcs8::{DecodePrivateKey, DecodePublicKey, EncodePrivateKey, EncodePublicKey, LineEnding};
use rsa::{Oaep, RsaPrivateKey, RsaPublicKey};
use sha2::{Digest, Sha256};

// hybrid seal: a fresh AES-256-GCM key per cookie, RSA-OAEP-wrapped with the board's public key.
// the server never holds the private key, so a database dump yields nothing decryptable
pub struct Sealed {
    pub wrapped_key: String,
    pub nonce: String,
    pub ciphertext: String,
}

pub fn seal(pubkey_pem: &str, plaintext: &[u8]) -> Result<Sealed> {
    let pubkey =
        RsaPublicKey::from_public_key_pem(pubkey_pem).context("parsing cookie public key")?;
    let mut aes_key = [0u8; 32];
    OsRng.fill_bytes(&mut aes_key);
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&aes_key));
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), plaintext)
        .map_err(|_| anyhow!("aes-gcm encrypt failed"))?;
    let wrapped = pubkey
        .encrypt(&mut OsRng, Oaep::new::<Sha256>(), &aes_key)
        .context("rsa-oaep wrap failed")?;
    Ok(Sealed {
        wrapped_key: hex::encode(wrapped),
        nonce: hex::encode(nonce),
        ciphertext: hex::encode(ciphertext),
    })
}

// only ever called during an import round, with a private key held in memory for that round
pub fn open(privkey_pem: &str, sealed: &Sealed) -> Result<Vec<u8>> {
    let privkey =
        RsaPrivateKey::from_pkcs8_pem(privkey_pem).context("parsing cookie private key")?;
    let wrapped = hex::decode(&sealed.wrapped_key).context("wrapped key not hex")?;
    let aes_key = privkey
        .decrypt(Oaep::new::<Sha256>(), &wrapped)
        .context("rsa-oaep unwrap failed")?;
    let cipher = Aes256Gcm::new_from_slice(&aes_key).map_err(|_| anyhow!("bad aes key length"))?;
    let nonce = hex::decode(&sealed.nonce).context("nonce not hex")?;
    let ciphertext = hex::decode(&sealed.ciphertext).context("ciphertext not hex")?;
    cipher
        .decrypt(Nonce::from_slice(&nonce), ciphertext.as_ref())
        .map_err(|_| anyhow!("aes-gcm decrypt failed"))
}

// stable id for a cookie without decrypting it: dedups resubmits and lets the UI reference a cookie
pub fn fingerprint(platform: &str, token: &str) -> String {
    let mut h = Sha256::new();
    h.update(platform.as_bytes());
    h.update(b":");
    h.update(token.as_bytes());
    hex::encode(h.finalize())
}

pub fn generate_keypair() -> Result<(String, String)> {
    let priv_key = RsaPrivateKey::new(&mut OsRng, 4096).context("generating rsa key")?;
    let pub_key = RsaPublicKey::from(&priv_key);
    let priv_pem = priv_key
        .to_pkcs8_pem(LineEnding::LF)
        .context("encoding private key")?
        .to_string();
    let pub_pem = pub_key
        .to_public_key_pem(LineEnding::LF)
        .context("encoding public key")?;
    Ok((pub_pem, priv_pem))
}

// inline PEM (starts with -----BEGIN) or a path to one; None if unset (contributions closed)
pub fn load_public_pem() -> Option<String> {
    let raw = std::env::var("BAKEMONO_COOKIE_PUBKEY").ok().filter(|s| !s.is_empty())?;
    if raw.trim_start().starts_with("-----BEGIN") {
        Some(raw)
    } else {
        std::fs::read_to_string(&raw).ok()
    }
}

pub fn load_private_pem() -> Result<Option<String>> {
    let Some(raw) = std::env::var("BAKEMONO_COOKIE_PRIVKEY").ok().filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if raw.trim_start().starts_with("-----BEGIN") {
        Ok(Some(raw))
    } else {
        Ok(Some(std::fs::read_to_string(&raw).with_context(|| format!("reading {raw}"))?))
    }
}

// reject a value that does not parse as a private key before touching the database
pub fn validate_private_pem(pem: &str) -> Result<()> {
    RsaPrivateKey::from_pkcs8_pem(pem).context("not a valid PKCS#8 private key")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip() {
        let (pubk, privk) = generate_keypair().unwrap();
        let secret = b"session_id=abcd1234deadbeef";
        let sealed = seal(&pubk, secret).unwrap();
        assert_ne!(sealed.ciphertext, hex::encode(secret));
        let opened = open(&privk, &sealed).unwrap();
        assert_eq!(opened, secret);
    }

    #[test]
    fn wrong_key_fails_to_open() {
        let (pubk, _) = generate_keypair().unwrap();
        let (_, other_priv) = generate_keypair().unwrap();
        let sealed = seal(&pubk, b"secret").unwrap();
        assert!(open(&other_priv, &sealed).is_err());
    }

    #[test]
    fn fingerprint_is_stable_and_platform_scoped() {
        assert_eq!(fingerprint("patreon", "x"), fingerprint("patreon", "x"));
        assert_ne!(fingerprint("patreon", "x"), fingerprint("fanbox", "x"));
    }
}
