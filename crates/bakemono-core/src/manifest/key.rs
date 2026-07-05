use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

use crate::error::{Error, Result};

pub struct BoardKey(SigningKey);

impl BoardKey {
    pub fn generate() -> Self {
        Self(SigningKey::generate(&mut rand::rngs::OsRng))
    }

    pub fn from_hex(secret: &str) -> Result<Self> {
        Ok(Self(SigningKey::from_bytes(&decode32(secret, "secret key")?)))
    }

    pub fn secret_hex(&self) -> String {
        hex::encode(self.0.to_bytes())
    }

    pub fn public_hex(&self) -> String {
        hex::encode(self.0.verifying_key().to_bytes())
    }

    pub fn sign_hex(&self, msg: &[u8]) -> String {
        hex::encode(self.0.sign(msg).to_bytes())
    }
}

pub fn verify_sig(pubkey_hex: &str, msg: &[u8], sig_hex: &str) -> Result<()> {
    let pubkey = VerifyingKey::from_bytes(&decode32(pubkey_hex, "pubkey")?)
        .map_err(|_| Error::BadHex("pubkey"))?;
    let sig_bytes: [u8; 64] = hex::decode(sig_hex)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or(Error::BadHex("sig"))?;
    pubkey
        .verify(msg, &Signature::from_bytes(&sig_bytes))
        .map_err(|_| Error::BadSignature)
}

fn decode32(s: &str, field: &'static str) -> Result<[u8; 32]> {
    hex::decode(s)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or(Error::BadHex(field))
}
