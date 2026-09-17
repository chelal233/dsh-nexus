//! Per-generation local API credentials. Secrets never travel over HTTP.
use std::{fs, io::{self, Read}, time::{SystemTime, UNIX_EPOCH}};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use serde::{Deserialize, Serialize};
use crate::{NexusPaths, data_root_identity, path_is_reparse, write_private_json_atomic};

pub const VERSION: u8 = 2;
pub const VERSION_HEADER: &str = "x-nexus-auth-version";
pub const TAG_BYTES: usize = 16;
pub const NONCE_HEADER: &str = "x-nexus-auth-nonce";
pub const TIME_HEADER: &str = "x-nexus-auth-time";
pub const SIGNATURE_HEADER: &str = "x-nexus-auth-signature";
pub const RESPONSE_HEADER: &str = "x-nexus-auth-response";
pub const MAX_CLOCK_SKEW_SECS: u64 = 60;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentCredential {
    schema_version: u8,
    pub data_root_id: String,
    pub instance_id: String,
    secret: String,
}

fn invalid() -> io::Error { io::Error::new(io::ErrorKind::InvalidData, "Agent credential is missing, unsafe, or belongs to another generation") }
pub fn random_hex() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| io::Error::other("Operating system randomness is unavailable"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
pub fn unix_seconds() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() }
pub fn valid_hex(value: &str) -> bool { value.len() == 64 && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) }

impl AgentCredential {
    fn crypt(&self, direction: &[u8], nonce: &str, context: &[&[u8]], body: &[u8], seal: bool) -> io::Result<Vec<u8>> {
        use ring::{aead, hkdf};
        if !valid_hex(nonce) { return Err(invalid()); }
        let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, b"nexus-agent-confidential-v2");
        let key = salt.extract(self.secret.as_bytes());
        // A fresh subkey for every full 256-bit request nonce avoids truncating
        // its collision resistance to the cipher's 96-bit nonce size.
        let info = [direction, self.data_root_id.as_bytes(), self.instance_id.as_bytes(), nonce.as_bytes()];
        let mut key_bytes = [0u8; 32];
        key.expand(&info, hkdf::HKDF_SHA256).map_err(|_| invalid())?.fill(&mut key_bytes).map_err(|_| invalid())?;
        let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::CHACHA20_POLY1305, &key_bytes).map_err(|_| invalid())?);
        let mut aad = Vec::new();
        for part in [b"nexus-agent-v2".as_slice(), direction, self.data_root_id.as_bytes(), self.instance_id.as_bytes(), nonce.as_bytes()].into_iter().chain(context.iter().copied()) {
            aad.extend_from_slice(&(part.len() as u64).to_be_bytes()); aad.extend_from_slice(part);
        }
        let mut buffer = body.to_vec();
        let nonce = aead::Nonce::assume_unique_for_key([0; 12]);
        if seal {
            key.seal_in_place_append_tag(nonce, aead::Aad::from(aad), &mut buffer).map_err(|_| invalid())?;
        } else {
            let length = key.open_in_place(nonce, aead::Aad::from(aad), &mut buffer).map_err(|_| invalid())?.len();
            buffer.truncate(length);
        }
        Ok(buffer)
    }
    pub fn seal_request(&self, method: &str, path: &str, nonce: &str, time: &str, body: &[u8]) -> io::Result<Vec<u8>> {
        self.crypt(b"request", nonce, &[method.as_bytes(), path.as_bytes(), time.as_bytes()], body, true)
    }
    pub fn open_request(&self, method: &str, path: &str, nonce: &str, time: &str, body: &[u8]) -> io::Result<Vec<u8>> {
        self.crypt(b"request", nonce, &[method.as_bytes(), path.as_bytes(), time.as_bytes()], body, false)
    }
    pub fn seal_response(&self, nonce: &str, status: u16, body: &[u8]) -> io::Result<Vec<u8>> {
        if status == 204 && body.is_empty() { return Ok(Vec::new()); }
        self.crypt(b"response", nonce, &[&status.to_be_bytes()], body, true)
    }
    pub fn open_response(&self, nonce: &str, status: u16, body: &[u8]) -> io::Result<Vec<u8>> {
        if status == 204 && body.is_empty() { return Ok(Vec::new()); }
        self.crypt(b"response", nonce, &[&status.to_be_bytes()], body, false)
    }
    /// Caller must hold the exclusive Agent runtime lock before rotating.
    pub fn publish(paths: &NexusPaths, instance_id: &str) -> io::Result<Self> {
        if instance_id.is_empty() || instance_id.len() > 256 { return Err(invalid()); }
        for path in [&paths.root, &paths.run_dir] {
            let metadata = fs::symlink_metadata(path)?;
            if !metadata.is_dir() || path_is_reparse(&metadata) { return Err(invalid()); }
        }
        let result = Self { schema_version: VERSION, data_root_id: data_root_identity(paths)?, instance_id: instance_id.into(), secret: random_hex()? };
        write_private_json_atomic(&paths.run_dir, &paths.run_dir.join("agent-credential.json"), &result)?;
        Ok(result)
    }
    pub fn read(paths: &NexusPaths, root: &str, instance: &str) -> io::Result<Self> {
        let path = paths.run_dir.join("agent-credential.json");
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_file() || path_is_reparse(&metadata) || metadata.len() > 4096 { return Err(invalid()); }
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(windows)] {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT
        }
        let file = options.open(path)?;
        if path_is_reparse(&file.metadata()?) { return Err(invalid()); }
        nexus_private_file::verify_private(&file)?;
        let mut bytes = Vec::new();
        file.take(4097).read_to_end(&mut bytes)?;
        if bytes.len() > 4096 { return Err(invalid()); }
        let value: Self = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        if value.schema_version != VERSION || value.data_root_id != root || value.data_root_id != data_root_identity(paths)?
            || value.instance_id != instance || instance.is_empty() || !valid_hex(&value.secret) { return Err(invalid()); }
        Ok(value)
    }
    fn mac(&self, parts: &[&[u8]]) -> Hmac<Sha256> {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.secret.as_bytes()).expect("HMAC key length");
        for part in parts { mac.update(&(part.len() as u64).to_be_bytes()); mac.update(part); }
        mac
    }
    pub fn request_signature(&self, method: &str, path: &str, nonce: &str, time: &str, body: &[u8]) -> String {
        self.mac(&[b"nexus-agent-request-v2", self.data_root_id.as_bytes(), self.instance_id.as_bytes(), method.as_bytes(), path.as_bytes(), nonce.as_bytes(), time.as_bytes(), body])
            .finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect()
    }
    pub fn verify_request(&self, method: &str, path: &str, nonce: &str, time: &str, body: &[u8], signature: &str) -> bool {
        valid_hex(nonce) && verify(self.mac(&[b"nexus-agent-request-v2", self.data_root_id.as_bytes(), self.instance_id.as_bytes(), method.as_bytes(), path.as_bytes(), nonce.as_bytes(), time.as_bytes(), body]), signature)
    }
    pub fn response_signature(&self, nonce: &str, status: u16, body: &[u8]) -> String {
        self.mac(&[b"nexus-agent-response-v2", nonce.as_bytes(), &status.to_be_bytes(), body]).finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect()
    }
    pub fn verify_response(&self, nonce: &str, status: u16, body: &[u8], signature: &str) -> bool {
        verify(self.mac(&[b"nexus-agent-response-v2", nonce.as_bytes(), &status.to_be_bytes(), body]), signature)
    }
}
fn verify(mac: Hmac<Sha256>, signature: &str) -> bool {
    // valid_hex guarantees an even-length ASCII hex string, so neither the
    // UTF-8 slice nor the radix decode below can fail.
    if !valid_hex(signature) { return false; }
    let bytes: Vec<u8> = signature.as_bytes().chunks_exact(2).map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap()).collect();
    mac.verify_slice(&bytes).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn credentials_are_private_strict_and_generation_bound() {
        let root = std::env::temp_dir().join(format!("nexus-credential-{}", random_hex().unwrap()));
        let paths = NexusPaths::from_root(root.clone());
        fs::create_dir_all(&paths.run_dir).unwrap();
        let credential = AgentCredential::publish(&paths, "one").unwrap();
        assert!(AgentCredential::read(&paths, &credential.data_root_id, "one").is_ok());
        assert!(AgentCredential::read(&paths, &credential.data_root_id, "two").is_err());
        let next = AgentCredential::publish(&paths, "two").unwrap();
        assert_ne!(credential.secret, next.secret);
        assert!(AgentCredential::read(&paths, &credential.data_root_id, "one").is_err());
        let mut value = serde_json::to_value(&next).unwrap(); value["schema_version"] = 99.into();
        write_private_json_atomic(&paths.run_dir, &paths.run_dir.join("agent-credential.json"), &value).unwrap();
        assert!(AgentCredential::read(&paths, &credential.data_root_id, "two").is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn signatures_bind_request_and_response_without_disclosing_secret() {
        let credential = AgentCredential { schema_version: VERSION, data_root_id: "root".into(), instance_id: "generation".into(), secret: random_hex().unwrap() };
        let nonce = random_hex().unwrap();
        let secret_body = br#"{"args":["--token","SECRET_SENTINEL"],"readiness_url":"http://localhost/?token=SECRET_SENTINEL"}"#;
        let encrypted = credential.seal_request("POST", "/v1/config", &nonce, "123", secret_body).unwrap();
        assert!(!encrypted.windows(b"SECRET_SENTINEL".len()).any(|window| window == b"SECRET_SENTINEL"));
        assert_eq!(credential.open_request("POST", "/v1/config", &nonce, "123", &encrypted).unwrap(), secret_body);
        assert!(credential.open_request("POST", "/v1/other", &nonce, "123", &encrypted).is_err());
        assert!(credential.open_request("POST", "/v1/config", &nonce, "123", secret_body).is_err());
        assert!(credential.open_response(&nonce, 200, &encrypted).is_err());
        let response_ciphertext = credential.seal_response(&nonce, 200, secret_body).unwrap();
        assert!(!response_ciphertext.windows(b"SECRET_SENTINEL".len()).any(|window| window == b"SECRET_SENTINEL"));
        assert_eq!(credential.open_response(&nonce, 200, &response_ciphertext).unwrap(), secret_body);
        assert!(credential.open_response(&nonce, 201, &response_ciphertext).is_err());
        let signature = credential.request_signature("POST", "/v1/shutdown", &nonce, "123", b"");
        assert!(credential.verify_request("POST", "/v1/shutdown", &nonce, "123", b"", &signature));
        assert!(!credential.verify_request("GET", "/v1/shutdown", &nonce, "123", b"", &signature));
        assert!(!credential.verify_request("POST", "/v1/shutdown", &nonce, "123", b"{}", &signature));
        assert!(!credential.verify_request("POST", "/v1/config", &nonce, "123", b"", &signature));
        let response = credential.response_signature(&nonce, 200, b"result");
        assert!(credential.verify_response(&nonce, 200, b"result", &response));
        assert!(!credential.verify_response(&nonce, 200, b"forged", &response));
        assert!(!credential.verify_response(&nonce, 401, b"result", &response));
    }
}
