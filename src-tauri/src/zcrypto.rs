use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use sha2::{Digest, Sha256};
use std::path::Path;

pub const PREFIX: &str = "enc:v1:";

fn node_os() -> &'static str {
    match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    }
}

fn pick_username(username: Option<&str>, user: Option<&str>, logname: Option<&str>) -> String {
    username
        .or(user)
        .or(logname)
        .unwrap_or("unknown")
        .to_string()
}

fn compose_fallback_secret(platform: &str, home: &str, username: &str) -> String {
    format!("lobster-plus-credential-fallback:{platform}:{home}:{username}")
}

pub fn default_secret(home: &Path) -> String {
    if let Ok(s) = std::env::var("LOBSTER_PLUS_CREDENTIAL_SECRET") {
        return s;
    }
    #[cfg(windows)]
    let primary = std::env::var("USERNAME").ok();
    #[cfg(not(windows))]
    let primary = std::env::var("USER").ok();
    let username = pick_username(
        primary.as_deref(),
        std::env::var("USER").ok().as_deref(),
        std::env::var("LOGNAME").ok().as_deref(),
    );
    compose_fallback_secret(node_os(), &home.display().to_string(), &username)
}

fn derive_key(secret: &str) -> [u8; 32] {
    let d = Sha256::digest(secret.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(&d);
    out
}

pub fn is_encrypted(v: &str) -> bool {
    v.starts_with(PREFIX)
}

pub fn encrypt_with_secret(plain: &str, secret: &str) -> Result<String, String> {
    let key = derive_key(secret);
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| format!("密钥初始化失败：{e}"))?;
    let nonce = <Aes256Gcm as AeadCore>::generate_nonce(&mut OsRng);
    let ct = cipher
        .encrypt(
            &nonce,
            Payload { msg: plain.as_bytes(), aad: b"lobster-plus:v1" },
        )
        .map_err(|e| format!("加密失败：{e}"))?;
    // enc:v1:nonce.tag.ciphertext 格式（tag 单独拆出，与家族实现对称）
    let ct_body = &ct[..ct.len() - 16];
    let tag = &ct[ct.len() - 16..];
    Ok(format!(
        "{PREFIX}{}.{}.{}",
        URL_SAFE_NO_PAD.encode(nonce),
        URL_SAFE_NO_PAD.encode(tag),
        URL_SAFE_NO_PAD.encode(ct_body)
    ))
}

pub fn decrypt_with_secret(value: &str, secret: &str) -> Result<String, String> {
    let body = value.strip_prefix(PREFIX).ok_or("不是 enc:v1 格式")?;
    let parts: Vec<&str> = body.split('.').collect();
    if parts.len() != 3 {
        return Err("enc:v1 格式不正确".into());
    }
    let nonce_b = URL_SAFE_NO_PAD.decode(parts[0]).map_err(|e| format!("nonce 解码失败：{e}"))?;
    let tag_b = URL_SAFE_NO_PAD.decode(parts[1]).map_err(|e| format!("tag 解码失败：{e}"))?;
    let ct_b = URL_SAFE_NO_PAD.decode(parts[2]).map_err(|e| format!("密文解码失败：{e}"))?;
    if nonce_b.len() != 12 {
        return Err("nonce 长度异常".into());
    }
    let key = derive_key(secret);
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| format!("密钥初始化失败：{e}"))?;
    let mut buf = ct_b.clone();
    buf.extend_from_slice(&tag_b);
    let pt = cipher
        .decrypt(
            Nonce::from_slice(&nonce_b),
            Payload { msg: buf.as_slice(), aad: b"lobster-plus:v1" },
        )
        .map_err(|_| "解密失败（密钥不匹配或数据损坏）".to_string())?;
    Ok(String::from_utf8_lossy(&pt).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let s = "some-secret";
        let enc = encrypt_with_secret("hello 中文 🦞", s).unwrap();
        assert!(is_encrypted(&enc));
        assert_eq!(decrypt_with_secret(&enc, s).unwrap(), "hello 中文 🦞");
    }

    #[test]
    fn wrong_secret_fails() {
        let enc = encrypt_with_secret("data", "secret-a").unwrap();
        assert!(decrypt_with_secret(&enc, "secret-b").is_err());
    }
}
