//! Ed25519 detached-signature verification for skills and WASM modules.
//!
//! Signature files (`.sig`) are hex-encoded Ed25519 signatures placed
//! alongside the signed artifact (e.g. `SKILL.md.sig`, `tool.wasm.sig`).
//! Trusted public keys are loaded from the `IRONCLAW_SIGNING_KEYS` env var
//! (comma-separated hex-encoded 32-byte public keys).

use std::path::Path;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};

/// Configuration for signature verification.
#[derive(Debug, Clone)]
pub struct SigningConfig {
    /// Trusted Ed25519 public keys.
    pub keys: Vec<VerifyingKey>,
    /// When true, installed (registry) skills must have a valid signature.
    pub require_for_installed: bool,
}

impl Default for SigningConfig {
    fn default() -> Self {
        Self {
            keys: Vec::new(),
            require_for_installed: true,
        }
    }
}

impl SigningConfig {
    /// Returns `true` if no trusted keys are configured (verification is a no-op).
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// Errors from signature operations.
#[derive(Debug, thiserror::Error)]
pub enum SignatureError {
    #[error("Invalid hex public key '{key}': {reason}")]
    InvalidHexKey { key: String, reason: String },

    #[error("Invalid Ed25519 public key bytes: {0}")]
    InvalidPublicKey(String),

    #[error("Invalid signature hex: {0}")]
    InvalidSignatureHex(String),

    #[error("Signature verification failed: no trusted key matched")]
    VerificationFailed,

    #[error("IO error reading signature file: {0}")]
    Io(String),

    #[error("Signature required but .sig file not found for: {0}")]
    SignatureRequired(String),
}

/// Load trusted Ed25519 verifying keys from a comma-separated hex string.
///
/// Each key is a 32-byte (64-hex-char) Ed25519 public key. Empty strings
/// and whitespace-only entries are silently skipped.
pub fn load_trusted_keys(hex_csv: &str) -> Result<Vec<VerifyingKey>, SignatureError> {
    let mut keys = Vec::new();
    for raw in hex_csv.split(',') {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let bytes = hex::decode(trimmed).map_err(|e| SignatureError::InvalidHexKey {
            key: trimmed.to_string(),
            reason: e.to_string(),
        })?;
        let key_bytes: [u8; 32] =
            bytes
                .try_into()
                .map_err(|v: Vec<u8>| SignatureError::InvalidHexKey {
                    key: trimmed.to_string(),
                    reason: format!("expected 32 bytes, got {}", v.len()),
                })?;
        let key = VerifyingKey::from_bytes(&key_bytes)
            .map_err(|e| SignatureError::InvalidPublicKey(e.to_string()))?;
        keys.push(key);
    }
    Ok(keys)
}

/// Read a detached `.sig` sidecar file beside the given path.
///
/// Returns `Ok(None)` if the `.sig` file does not exist.
/// The file is expected to contain hex-encoded bytes (whitespace trimmed).
pub async fn read_sig_file(artifact_path: &Path) -> Result<Option<String>, SignatureError> {
    let file_name = artifact_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let sig_path = artifact_path.with_file_name(format!("{file_name}.sig"));

    match tokio::fs::read_to_string(&sig_path).await {
        Ok(content) => Ok(Some(content.trim().to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(SignatureError::Io(e.to_string())),
    }
}

/// Verify a detached Ed25519 signature against any of the trusted keys.
///
/// `payload` is the raw bytes of the signed artifact.
/// `signature_hex` is the hex-encoded 64-byte Ed25519 signature.
///
/// Returns `Ok(())` if at least one key produces a valid verification.
pub fn verify_detached_signature(
    payload: &[u8],
    signature_hex: &str,
    keys: &[VerifyingKey],
) -> Result<(), SignatureError> {
    let sig_bytes = hex::decode(signature_hex)
        .map_err(|e| SignatureError::InvalidSignatureHex(e.to_string()))?;
    let signature = Signature::from_slice(&sig_bytes)
        .map_err(|e| SignatureError::InvalidSignatureHex(e.to_string()))?;

    for key in keys {
        if key.verify(payload, &signature).is_ok() {
            return Ok(());
        }
    }

    Err(SignatureError::VerificationFailed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn generate_keypair() -> (SigningKey, VerifyingKey) {
        let signing = SigningKey::generate(&mut rand::rngs::OsRng);
        let verifying = signing.verifying_key();
        (signing, verifying)
    }

    #[test]
    fn sign_and_verify() {
        let (signing_key, verifying_key) = generate_keypair();
        let payload = b"hello world";

        use ed25519_dalek::Signer;
        let signature = signing_key.sign(payload);
        let sig_hex = hex::encode(signature.to_bytes());

        assert!(verify_detached_signature(payload, &sig_hex, &[verifying_key]).is_ok());
    }

    #[test]
    fn wrong_key_fails() {
        let (signing_key, _) = generate_keypair();
        let (_, wrong_key) = generate_keypair();
        let payload = b"hello world";

        use ed25519_dalek::Signer;
        let signature = signing_key.sign(payload);
        let sig_hex = hex::encode(signature.to_bytes());

        assert!(matches!(
            verify_detached_signature(payload, &sig_hex, &[wrong_key]),
            Err(SignatureError::VerificationFailed)
        ));
    }

    #[test]
    fn load_valid_hex_key() {
        let (_, verifying_key) = generate_keypair();
        let hex_key = hex::encode(verifying_key.as_bytes());

        let keys = load_trusted_keys(&hex_key).expect("should parse");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0], verifying_key);
    }

    #[test]
    fn load_multiple_keys() {
        let (_, k1) = generate_keypair();
        let (_, k2) = generate_keypair();
        let csv = format!(
            "{},{}",
            hex::encode(k1.as_bytes()),
            hex::encode(k2.as_bytes())
        );

        let keys = load_trusted_keys(&csv).expect("should parse");
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn load_empty_string_returns_no_keys() {
        let keys = load_trusted_keys("").expect("should parse");
        assert!(keys.is_empty());
    }

    #[test]
    fn load_invalid_hex_errors() {
        assert!(load_trusted_keys("not_valid_hex_zzzz").is_err());
    }

    #[test]
    fn load_wrong_length_errors() {
        // 16 bytes instead of 32
        assert!(load_trusted_keys(&hex::encode([0u8; 16])).is_err());
    }

    #[tokio::test]
    async fn read_missing_sig_returns_none() {
        let result = read_sig_file(Path::new("/tmp/nonexistent_file_ironclaw_test.md")).await;
        assert!(matches!(result, Ok(None)));
    }
}
