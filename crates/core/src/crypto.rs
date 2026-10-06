//! Crypto for a control plane, in pure Rust (the same on WebAssembly and native): base64url, SHA-256, HMAC (GitHub's webhook
//! signature), RS256 JWTs (GitHub App keys, PKCS#1 or PKCS#8) and the control plane's own ES256 identity-provider key.
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use hmac::{Hmac, Mac};
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::signature::SignatureEncoding;
use sha2::{Digest, Sha256};

use crate::io::Result;

pub fn b64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn b64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

pub fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// A random URL-safe token (for sessions, states, ids).
pub fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::getrandom(&mut buf).expect("randomness");
    b64url(&buf)
}

/// A random lowercase id (a control plane's id: names its GitHub App, AWS role and machine tags).
pub fn random_id(len: usize) -> String {
    let mut buf = vec![0u8; len];
    getrandom::getrandom(&mut buf).expect("randomness");
    buf.iter().map(|b| (b"abcdefghijklmnopqrstuvwxyz0123456789")[(*b as usize) % 36] as char).collect()
}

/// Comparison that does not stop at the first difference (tokens, signatures).
pub fn safe_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// GitHub's webhook signature header (`sha256=<hex>`).
pub fn verify_hub_signature(secret: &str, body: &[u8], signature: Option<&str>) -> bool {
    let Some(sig) = signature.and_then(|s| s.strip_prefix("sha256=")) else { return false };
    if secret.is_empty() || sig.len() != 64 {
        return false;
    }
    safe_eq(hex(&hmac_sha256(secret.as_bytes(), body)).as_bytes(), sig.as_bytes())
}

fn jwt_parts(header: &serde_json::Value, claims: &serde_json::Value) -> String {
    format!("{}.{}", b64url(header.to_string().as_bytes()), b64url(claims.to_string().as_bytes()))
}

/// An RS256 JWT signed with an RSA key in PEM (GitHub gives PKCS#1, "BEGIN RSA PRIVATE KEY").
pub fn rs256_jwt(claims: &serde_json::Value, pem: &str) -> Result<String> {
    let key = if pem.contains("BEGIN RSA PRIVATE KEY") {
        rsa::RsaPrivateKey::from_pkcs1_pem(pem).map_err(|e| format!("App key: {e}"))?
    } else {
        rsa::RsaPrivateKey::from_pkcs8_pem(pem).map_err(|e| format!("App key: {e}"))?
    };
    let unsigned = jwt_parts(&serde_json::json!({ "alg": "RS256", "typ": "JWT" }), claims);
    let signer = rsa::pkcs1v15::SigningKey::<Sha256>::new(key);
    let sig = signer.sign(unsigned.as_bytes()).to_vec();
    Ok(format!("{unsigned}.{}", b64url(&sig)))
}

/// The control plane's identity-provider key (P-256): the private scalar is stored in the control plane, the public JWK is served.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct SigningKeyStore {
    pub private_b64: String,
    pub kid: String,
}

impl SigningKeyStore {
    pub fn generate() -> Self {
        let mut seed = [0u8; 32];
        let key = loop {
            getrandom::getrandom(&mut seed).expect("randomness");
            if let Ok(k) = SigningKey::from_slice(&seed) {
                break k;
            }
        };
        let public = key.verifying_key().to_encoded_point(false);
        SigningKeyStore { private_b64: b64(&key.to_bytes()), kid: sha256_hex(public.as_bytes())[..16].to_string() }
    }

    fn key(&self) -> Result<SigningKey> {
        let bytes = STANDARD.decode(&self.private_b64).map_err(|e| e.to_string())?;
        SigningKey::from_slice(&bytes).map_err(|e| e.to_string())
    }

    /// The public key as a JWK (served at /.well-known/jwks.json).
    pub fn public_jwk(&self) -> Result<serde_json::Value> {
        let point = self.key()?.verifying_key().to_encoded_point(false);
        Ok(serde_json::json!({
            "kty": "EC", "crv": "P-256", "use": "sig", "alg": "ES256", "kid": self.kid,
            "x": b64url(point.x().expect("uncompressed")), "y": b64url(point.y().expect("uncompressed")),
        }))
    }

    /// An ES256 JWT (JWS signature: r || s, 64 bytes).
    pub fn jwt(&self, claims: &serde_json::Value) -> Result<String> {
        self.jwt_with_header(&serde_json::json!({ "alg": "ES256", "typ": "JWT", "kid": self.kid }), claims)
    }

    pub fn jwt_with_header(&self, header: &serde_json::Value, claims: &serde_json::Value) -> Result<String> {
        let unsigned = jwt_parts(header, claims);
        let sig: Signature = self.key()?.sign(unsigned.as_bytes());
        Ok(format!("{unsigned}.{}", b64url(&sig.to_bytes())))
    }

    /// A DPoP proof (RFC 9449) for one POST: the key's public half travels in the header, the token is bound to it.
    pub fn dpop(&self, url: &str, now_secs: u64) -> Result<String> {
        let jwk = self.public_jwk()?;
        let header = serde_json::json!({ "typ": "dpop+jwt", "alg": "ES256", "jwk": { "kty": "EC", "x": jwk["x"], "y": jwk["y"], "crv": "P-256" } });
        self.jwt_with_header(&header, &serde_json::json!({ "htm": "POST", "htu": url, "iat": now_secs, "jti": random_token(16) }))
    }
}

/// The claims of an ES256 JWT signed by the P-256 key in `jwk` (x, y), if the signature holds and it has not expired.
/// Issuer, audience and subject are for the caller to check.
pub fn verify_es256_jwt(token: &str, jwk: &serde_json::Value, now_secs: u64) -> Result<serde_json::Value> {
    use p256::ecdsa::signature::Verifier;
    use p256::ecdsa::VerifyingKey;
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 { return Err("not a JWT".into()); }
    let header: serde_json::Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    if header["alg"] != "ES256" { return Err("not ES256".into()); }
    let coord = |k: &str| URL_SAFE_NO_PAD.decode(jwk[k].as_str().unwrap_or_default()).map_err(|e| e.to_string());
    let (x, y) = (coord("x")?, coord("y")?);
    if x.len() != 32 || y.len() != 32 { return Err("bad key".into()); }
    let point = p256::EncodedPoint::from_affine_coordinates(x.as_slice().into(), y.as_slice().into(), false);
    let key = VerifyingKey::from_encoded_point(&point).map_err(|e| e.to_string())?;
    let sig = Signature::from_slice(&URL_SAFE_NO_PAD.decode(parts[2]).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    key.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig).map_err(|_| "bad signature".to_string())?;
    let claims: serde_json::Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    if claims["exp"].as_u64().is_none_or(|exp| exp < now_secs) { return Err("expired".into()); }
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::signature::Verifier;
    use p256::ecdsa::VerifyingKey;

    #[test]
    fn es256_tokens_verify_with_the_public_key_only() {
        let key = SigningKeyStore::generate();
        let jwk = key.public_jwk().unwrap();
        let token = key.jwt(&serde_json::json!({ "iss": "https://p", "aud": "https://a", "exp": 2_000 })).unwrap();
        assert_eq!(verify_es256_jwt(&token, &jwk, 1_000).unwrap()["aud"], "https://a");
        assert_eq!(verify_es256_jwt(&token, &jwk, 3_000).unwrap_err(), "expired");
        let other = SigningKeyStore::generate().public_jwk().unwrap();
        assert_eq!(verify_es256_jwt(&token, &other, 1_000).unwrap_err(), "bad signature");
        let mut forged: Vec<&str> = token.split('.').collect();
        let claims = URL_SAFE_NO_PAD.encode(br#"{"iss":"https://p","aud":"https://evil","exp":2000}"#);
        forged[1] = &claims;
        assert_eq!(verify_es256_jwt(&forged.join("."), &jwk, 1_000).unwrap_err(), "bad signature");
    }

    #[test]
    fn dpop_proof_carries_its_key_and_verifies() {
        let key = SigningKeyStore::generate();
        let proof = key.dpop("https://us-east-1.signin.aws.amazon.com/v1/token", 1_790_000_000).unwrap();
        let parts: Vec<&str> = proof.split('.').collect();
        let header: serde_json::Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        let claims: serde_json::Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!((header["typ"].as_str(), header["alg"].as_str(), header["jwk"]["crv"].as_str(), header.get("kid")), (Some("dpop+jwt"), Some("ES256"), Some("P-256"), None));
        assert_eq!((claims["htm"].as_str(), claims["iat"].as_u64()), (Some("POST"), Some(1_790_000_000)));
        let (x, y) = (URL_SAFE_NO_PAD.decode(header["jwk"]["x"].as_str().unwrap()).unwrap(), URL_SAFE_NO_PAD.decode(header["jwk"]["y"].as_str().unwrap()).unwrap());
        let point = p256::EncodedPoint::from_affine_coordinates(x.as_slice().into(), y.as_slice().into(), false);
        let sig = Signature::from_slice(&URL_SAFE_NO_PAD.decode(parts[2]).unwrap()).unwrap();
        VerifyingKey::from_encoded_point(&point).unwrap().verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig).unwrap();
    }

    #[test]
    fn hub_signature() {
        // GitHub's documented example: secret "It's a Secret to Everybody", payload "Hello, World!".
        let sig = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";
        assert!(verify_hub_signature("It's a Secret to Everybody", b"Hello, World!", Some(sig)));
        assert!(!verify_hub_signature("It's a Secret to Everybody", b"Hello, World?", Some(sig)));
        assert!(!verify_hub_signature("", b"Hello, World!", Some(sig)));
        assert!(!verify_hub_signature("x", b"x", None));
    }

    #[test]
    fn es256_jwt_verifies_with_its_jwk() {
        let key = SigningKeyStore::generate();
        let jwt = key.jwt(&serde_json::json!({ "sub": "plane:abc", "aud": "superci" })).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let jwk = key.public_jwk().unwrap();
        let x = URL_SAFE_NO_PAD.decode(jwk["x"].as_str().unwrap()).unwrap();
        let y = URL_SAFE_NO_PAD.decode(jwk["y"].as_str().unwrap()).unwrap();
        let point = p256::EncodedPoint::from_affine_coordinates(x.as_slice().into(), y.as_slice().into(), false);
        let verifying = VerifyingKey::from_encoded_point(&point).unwrap();
        let sig = Signature::from_slice(&URL_SAFE_NO_PAD.decode(parts[2]).unwrap()).unwrap();
        verifying.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig).unwrap();
        let header: serde_json::Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header["kid"], jwk["kid"]);
    }

    #[test]
    fn rs256_jwt_from_pkcs1_pem() {
        use rsa::pkcs1::EncodeRsaPrivateKey;
        let mut rng = rsa::rand_core::OsRng;
        let key = rsa::RsaPrivateKey::new(&mut rng, 1024).unwrap();
        let pem = key.to_pkcs1_pem(rsa::pkcs1::LineEnding::LF).unwrap();
        let jwt = rs256_jwt(&serde_json::json!({ "iss": "123" }), &pem).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        let verifying = rsa::pkcs1v15::VerifyingKey::<Sha256>::new(key.to_public_key());
        let sig = rsa::pkcs1v15::Signature::try_from(URL_SAFE_NO_PAD.decode(parts[2]).unwrap().as_slice()).unwrap();
        use rsa::signature::Verifier as _;
        verifying.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig).unwrap();
    }

    #[test]
    fn ids_and_equality() {
        assert_eq!(random_id(12).len(), 12);
        assert!(random_id(12).chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
        assert!(safe_eq(b"abc", b"abc"));
        assert!(!safe_eq(b"abc", b"abd"));
        assert!(!safe_eq(b"abc", b"ab"));
    }
}
