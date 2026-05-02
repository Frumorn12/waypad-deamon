use base64::{Engine as _, engine::general_purpose::STANDARD};
use ring::{
    aead, agreement, digest, hkdf,
    rand::{SecureRandom, SystemRandom},
    signature::{self, EcdsaKeyPair, KeyPair},
};
use serde::{Serialize, de::DeserializeOwned};
use std::io;
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::tcp::{OwnedReadHalf, OwnedWriteHalf},
};

use crate::protocol::{ClientPlain, PROTOCOL_VERSION, ServerPlain};

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("base64 error: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("cryptographic operation failed")]
    Crypto,
}

impl From<ring::error::Unspecified> for CryptoError {
    fn from(_: ring::error::Unspecified) -> Self {
        Self::Crypto
    }
}

impl From<ring::error::KeyRejected> for CryptoError {
    fn from(_: ring::error::KeyRejected) -> Self {
        Self::Crypto
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HostIdentity {
    pub private_key_pkcs8_b64: String,
    pub public_key_b64: String,
    pub fingerprint: String,
}

impl HostIdentity {
    pub fn generate() -> Result<Self, CryptoError> {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&signature::ECDSA_P256_SHA256_ASN1_SIGNING, &rng)?;
        let keypair = EcdsaKeyPair::from_pkcs8(
            &signature::ECDSA_P256_SHA256_ASN1_SIGNING,
            pkcs8.as_ref(),
            &rng,
        )?;
        let public_key = keypair.public_key().as_ref();
        Ok(Self {
            private_key_pkcs8_b64: b64(pkcs8.as_ref()),
            public_key_b64: b64(public_key),
            fingerprint: fingerprint(public_key),
        })
    }

    pub fn key_pair(&self) -> Result<EcdsaKeyPair, CryptoError> {
        let pkcs8 = b64_decode(&self.private_key_pkcs8_b64)?;
        Ok(EcdsaKeyPair::from_pkcs8(
            &signature::ECDSA_P256_SHA256_ASN1_SIGNING,
            &pkcs8,
            &SystemRandom::new(),
        )?)
    }

    pub fn public_key_bytes(&self) -> Result<Vec<u8>, CryptoError> {
        Ok(b64_decode(&self.public_key_b64)?)
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct WireFrame {
    seq: u64,
    ciphertext: String,
}

pub struct SecureChannel {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    inbound_key: aead::LessSafeKey,
    outbound_key: aead::LessSafeKey,
    inbound_prefix: [u8; 4],
    outbound_prefix: [u8; 4],
    next_inbound_seq: u64,
    next_outbound_seq: u64,
}

impl SecureChannel {
    pub async fn server(
        reader: OwnedReadHalf,
        mut writer: OwnedWriteHalf,
        identity: &HostIdentity,
    ) -> Result<Self, CryptoError> {
        let mut reader = BufReader::new(reader);
        let hello: ClientPlain = read_plain_json(&mut reader).await?;
        let ClientPlain::ClientHello {
            protocol,
            client_ephemeral_pub,
            ..
        } = hello;
        if protocol != PROTOCOL_VERSION {
            let error = ServerPlain::Error {
                code: "unsupported_protocol".into(),
                message: format!("protocol {protocol} is not supported"),
            };
            write_plain_json(&mut writer, &error).await?;
            return Err(CryptoError::Protocol("unsupported protocol".into()));
        }

        let rng = SystemRandom::new();
        let private = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng)?;
        let server_pub = private.compute_public_key()?;
        let client_pub = b64_decode(&client_ephemeral_pub)?;
        let mut nonce = [0u8; 32];
        rng.fill(&mut nonce)?;

        let mut transcript = Vec::new();
        transcript.extend_from_slice(b"WAYPAD-HANDSHAKE-v1");
        transcript.extend_from_slice(&client_pub);
        transcript.extend_from_slice(server_pub.as_ref());
        transcript.extend_from_slice(&nonce);
        let keypair = identity.key_pair()?;
        let signature = keypair.sign(&rng, &transcript)?;

        let shared = agreement::agree_ephemeral(
            private,
            &agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, &client_pub),
            |shared| shared.to_vec(),
        )?;

        let hello = ServerPlain::ServerHello {
            protocol: PROTOCOL_VERSION,
            host_public_key: identity.public_key_b64.clone(),
            host_fingerprint: identity.fingerprint.clone(),
            server_ephemeral_pub: b64(server_pub.as_ref()),
            signature: b64(signature.as_ref()),
            session_nonce: b64(&nonce),
        };
        write_plain_json(&mut writer, &hello).await?;

        let (c2s, s2c) = derive_keys(&shared, &nonce)?;
        Ok(Self {
            reader,
            writer,
            inbound_key: make_key(&c2s)?,
            outbound_key: make_key(&s2c)?,
            inbound_prefix: *b"C2S\0",
            outbound_prefix: *b"S2C\0",
            next_inbound_seq: 0,
            next_outbound_seq: 0,
        })
    }

    pub async fn recv<T: DeserializeOwned>(&mut self) -> Result<T, CryptoError> {
        let frame: WireFrame = read_plain_json(&mut self.reader).await?;
        if frame.seq != self.next_inbound_seq {
            return Err(CryptoError::Protocol(format!(
                "unexpected encrypted frame sequence {}, expected {}",
                frame.seq, self.next_inbound_seq
            )));
        }
        self.next_inbound_seq += 1;

        let mut ciphertext = b64_decode(&frame.ciphertext)?;
        let nonce = nonce(self.inbound_prefix, frame.seq);
        let aad = frame.seq.to_be_bytes();
        let plaintext = self
            .inbound_key
            .open_in_place(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(&aad),
                &mut ciphertext,
            )?
            .to_vec();
        Ok(serde_json::from_slice(&plaintext)?)
    }

    pub async fn send<T: Serialize>(&mut self, message: &T) -> Result<(), CryptoError> {
        let seq = self.next_outbound_seq;
        self.next_outbound_seq += 1;
        let mut plaintext = serde_json::to_vec(message)?;
        let nonce = nonce(self.outbound_prefix, seq);
        let aad = seq.to_be_bytes();
        self.outbound_key.seal_in_place_append_tag(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(&aad),
            &mut plaintext,
        )?;
        let frame = WireFrame {
            seq,
            ciphertext: b64(&plaintext),
        };
        write_plain_json(&mut self.writer, &frame).await
    }
}

pub async fn read_plain_json<T: DeserializeOwned>(
    reader: &mut BufReader<OwnedReadHalf>,
) -> Result<T, CryptoError> {
    let mut line = String::new();
    let read = reader.read_line(&mut line).await?;
    if read == 0 {
        return Err(CryptoError::Protocol("connection closed".into()));
    }
    if line.len() > 256 * 1024 {
        return Err(CryptoError::Protocol("frame too large".into()));
    }
    Ok(serde_json::from_str(line.trim_end())?)
}

pub async fn write_plain_json<T: Serialize>(
    writer: &mut OwnedWriteHalf,
    message: &T,
) -> Result<(), CryptoError> {
    let raw = serde_json::to_vec(message)?;
    if raw.len() > 256 * 1024 {
        return Err(CryptoError::Protocol("frame too large".into()));
    }
    writer.write_all(&raw).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

fn derive_keys(shared: &[u8], salt_bytes: &[u8]) -> Result<([u8; 32], [u8; 32]), CryptoError> {
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, salt_bytes);
    let prk = salt.extract(shared);
    let mut c2s = [0u8; 32];
    let mut s2c = [0u8; 32];
    prk.expand(&[b"waypad v1 c2s"], Aes256KeyLen)?
        .fill(&mut c2s)?;
    prk.expand(&[b"waypad v1 s2c"], Aes256KeyLen)?
        .fill(&mut s2c)?;
    Ok((c2s, s2c))
}

struct Aes256KeyLen;

impl hkdf::KeyType for Aes256KeyLen {
    fn len(&self) -> usize {
        32
    }
}

fn make_key(raw: &[u8; 32]) -> Result<aead::LessSafeKey, CryptoError> {
    let unbound = aead::UnboundKey::new(&aead::AES_256_GCM, raw)?;
    Ok(aead::LessSafeKey::new(unbound))
}

fn nonce(prefix: [u8; 4], seq: u64) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[..4].copy_from_slice(&prefix);
    nonce[4..].copy_from_slice(&seq.to_be_bytes());
    nonce
}

pub fn b64(data: &[u8]) -> String {
    STANDARD.encode(data)
}

pub fn b64_decode(data: &str) -> Result<Vec<u8>, base64::DecodeError> {
    STANDARD.decode(data)
}

pub fn sha256(data: &[u8]) -> Vec<u8> {
    digest::digest(&digest::SHA256, data).as_ref().to_vec()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(sha256(data))
}

pub fn fingerprint(public_key: &[u8]) -> String {
    let hex = sha256_hex(public_key);
    hex.as_bytes()
        .chunks(4)
        .map(|chunk| std::str::from_utf8(chunk).unwrap())
        .collect::<Vec<_>>()
        .join(":")
}

pub fn random_token() -> Result<String, CryptoError> {
    let rng = SystemRandom::new();
    let mut bytes = [0u8; 32];
    rng.fill(&mut bytes)?;
    Ok(b64(&bytes))
}

pub fn random_pairing_code() -> Result<String, CryptoError> {
    let rng = SystemRandom::new();
    let mut bytes = [0u8; 4];
    rng.fill(&mut bytes)?;
    let value = u32::from_be_bytes(bytes) % 1_000_000;
    Ok(format!("{value:06}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_are_stable_grouped_sha256() {
        let fp = fingerprint(b"abc");
        assert!(fp.contains(':'));
        assert_eq!(fp.replace(':', ""), sha256_hex(b"abc"));
    }

    #[test]
    fn pairing_codes_are_six_digits() {
        let code = random_pairing_code().unwrap();
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
    }
}
