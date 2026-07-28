//! ECDSA P-256 request signer for Xbox Live.
//!
//! Port of `XboxAuthNet/XboxLive/Crypto/ECDCertificatePopCryptoProvider.cs`
//! and `XboxRequestSigner.cs`.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use p256::ecdsa::{signature::Signer, Signature, SigningKey};
use rand_core::OsRng;
use serde::Serialize;
use url::Url;

use crate::error::Result;

/// JWK shape Xbox Live expects as `ProofKey` inside every signed request.
/// Matches the fields produced by `generateNewProofKey()` in the C# source.
#[derive(Debug, Clone, Serialize)]
pub struct ProofKey {
    pub kty: &'static str,
    pub x: String,
    pub y: String,
    pub crv: &'static str,
    pub alg: &'static str,
    #[serde(rename = "use")]
    pub usage: &'static str,
}

/// Fixed-size ECDSA P-256 signing key with the Xbox request-signing protocol.
///
/// One instance should be reused for the lifetime of a sign-in session — the
/// same `ProofKey` is sent with the device-token POST *and* the SISU POST;
/// Xbox matches the two to derive authorization.
#[derive(Clone)]
pub struct XboxRequestSigner {
    signing_key: SigningKey,
    proof_key: ProofKey,
}

impl std::fmt::Debug for XboxRequestSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XboxRequestSigner")
            .field("proof_key", &self.proof_key)
            .field("signing_key", &"<redacted>")
            .finish()
    }
}

impl XboxRequestSigner {
    /// Generate a fresh ephemeral P-256 keypair.
    pub fn generate() -> Self {
        let signing_key = SigningKey::random(&mut OsRng);
        let proof_key = derive_proof_key(&signing_key);
        Self {
            signing_key,
            proof_key,
        }
    }

    pub fn proof_key(&self) -> &ProofKey {
        &self.proof_key
    }

    /// Signs a request body for Xbox Live. Returns a base64 value suitable for
    /// the `Signature` HTTP header.
    ///
    /// The canonical message is:
    /// ```text
    ///   policy_version (u32 BE, =1)
    ///   \0
    ///   windows_timestamp (u64 BE)
    ///   \0
    ///   "POST\0" + path_and_query + "\0" + token + "\0" + body + "\0"   (ASCII)
    /// ```
    /// The returned header value is:
    /// ```text
    ///   policy_version (u32 BE) || windows_timestamp (u64 BE) || ECDSA P-256 signature
    /// ```
    pub fn sign_request(&self, request_url: &str, token: &str, body: &str) -> Result<String> {
        let parsed = Url::parse(request_url)?;
        let path_and_query = match parsed.query() {
            Some(q) => format!("{}?{}", parsed.path(), q),
            None => parsed.path().to_string(),
        };
        let timestamp = windows_timestamp_now();

        let canonical = build_canonical_bytes(1, timestamp, &path_and_query, token, body);

        // Xbox signs the bytes directly; `SigningKey::sign` runs SHA-256
        // internally and produces an r||s signature (64 bytes).
        let signature: Signature = self.signing_key.sign(&canonical);
        let mut raw = signature.to_bytes().to_vec();

        // Header = 4-byte policy_version || 8-byte timestamp || signature
        let mut header = Vec::with_capacity(12 + raw.len());
        header.extend_from_slice(&1u32.to_be_bytes());
        header.extend_from_slice(&timestamp.to_be_bytes());
        header.append(&mut raw);

        Ok(base64::engine::general_purpose::STANDARD.encode(header))
    }
}

fn derive_proof_key(signing_key: &SigningKey) -> ProofKey {
    let verifying_key = signing_key.verifying_key();
    let encoded = verifying_key.to_encoded_point(false);
    // Uncompressed SEC1 point: 0x04 || x (32B) || y (32B)
    let bytes = encoded.as_bytes();
    assert_eq!(
        bytes.len(),
        65,
        "unexpected uncompressed SEC1 P-256 point length: {}",
        bytes.len()
    );
    let x = URL_SAFE_NO_PAD.encode(&bytes[1..33]);
    let y = URL_SAFE_NO_PAD.encode(&bytes[33..65]);
    ProofKey {
        kty: "EC",
        x,
        y,
        crv: "P-256",
        alg: "ES256",
        usage: "sig",
    }
}

fn windows_timestamp_now() -> u64 {
    // Windows FILETIME: 100-ns ticks since 1601-01-01.
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    (unix + 11_644_473_600) * 10_000_000
}

fn build_canonical_bytes(
    policy_version: u32,
    timestamp: u64,
    path_and_query: &str,
    token: &str,
    body: &str,
) -> Vec<u8> {
    // Layout (mirrors `XboxRequestSigner.generatePayload`):
    //   0..4:  policy_version BE
    //   4:     \0
    //   5..13: timestamp BE
    //   13:    \0
    //   14..:  "POST\0" + path_and_query + "\0" + token + "\0" + body + "\0"
    let mut out = Vec::with_capacity(
        4 + 1 + 8 + 1 + 4 + 1 + path_and_query.len() + 1 + token.len() + 1 + body.len() + 1,
    );
    out.extend_from_slice(&policy_version.to_be_bytes());
    out.push(0);
    out.extend_from_slice(&timestamp.to_be_bytes());
    out.push(0);
    out.extend_from_slice(b"POST\0");
    out.extend_from_slice(path_and_query.as_bytes());
    out.push(0);
    out.extend_from_slice(token.as_bytes());
    out.push(0);
    out.extend_from_slice(body.as_bytes());
    out.push(0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proof_key_is_base64_url_64_bytes() {
        let signer = XboxRequestSigner::generate();
        let k = signer.proof_key();
        assert_eq!(k.kty, "EC");
        assert_eq!(k.crv, "P-256");
        assert_eq!(k.alg, "ES256");
        // base64url, no padding — 32 raw bytes → 43 chars.
        assert_eq!(k.x.len(), 43);
        assert_eq!(k.y.len(), 43);
    }

    #[test]
    fn canonical_bytes_layout_matches_csharp() {
        let bytes = build_canonical_bytes(1, 0x1122_3344_5566_7788, "/hello?a=b", "TOKEN", "{}");
        assert_eq!(&bytes[0..4], &[0, 0, 0, 1]);
        assert_eq!(bytes[4], 0);
        assert_eq!(
            &bytes[5..13],
            &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]
        );
        assert_eq!(bytes[13], 0);
        assert_eq!(&bytes[14..19], b"POST\0");
        assert_eq!(&bytes[19..29], b"/hello?a=b");
        assert_eq!(bytes[29], 0);
        assert_eq!(&bytes[30..35], b"TOKEN");
        assert_eq!(bytes[35], 0);
        assert_eq!(&bytes[36..38], b"{}");
        assert_eq!(bytes[38], 0);
    }

    #[test]
    fn sign_produces_base64_header() {
        let signer = XboxRequestSigner::generate();
        let sig = signer
            .sign_request("https://sisu.xboxlive.com/authorize", "", "{}")
            .unwrap();
        // 12 header bytes + 64 signature bytes = 76 → base64 of 76 bytes = 104
        // chars with '=' padding. Allow 100..=120 for safety across encoders.
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&sig)
            .unwrap();
        assert_eq!(decoded.len(), 4 + 8 + 64);
        assert_eq!(&decoded[0..4], &[0, 0, 0, 1], "policy version prefix");
    }
}
