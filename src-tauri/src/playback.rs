//! 与网站当前公开播放器一致的播放会话。密钥、CDN 凭据只在 Rust 内存中保留。
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use ring::{aead, agreement, hkdf, rand::SystemRandom};
use serde::Deserialize;
use serde_json::{json, Value};

const PROTOCOL: &str = "dx150-edge-v1";
const SCOPE: &str = "playback";
const VERSION: u8 = 1;
const SEQUENCE: u32 = 1;
const MAX_SESSION_SECONDS: i64 = 600;
const SESSION_ID_MIN: usize = 16;
const SESSION_ID_MAX: usize = 128;

#[derive(Deserialize)]
struct Hello {
    v: u8,
    #[serde(rename = "type")]
    kind: String,
    kid: String,
    sid: String,
    site: String,
    server_pub: String,
    salt: String,
    server_time: i64,
    expires_at: i64,
}

fn identifier(value: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&value.len())
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
}

impl Hello {
    fn validate(&self) -> Result<(), String> {
        if self.v != VERSION
            || self.kind != "hello"
            || !identifier(&self.kid, 1, 64)
            || !identifier(&self.sid, SESSION_ID_MIN, SESSION_ID_MAX)
            || self.site.is_empty()
            || self.site.len() > 256
            || !self
                .site
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b".:[-]".contains(&c))
            || self.server_time < 0
            || self.expires_at <= self.server_time
            || self.expires_at.saturating_sub(self.server_time) > MAX_SESSION_SECONDS
        {
            return Err("播放加密握手响应无效".into());
        }
        Ok(())
    }
    fn info(&self) -> String {
        format!(
            "{PROTOCOL}\n{SCOPE}\n{}\n{}\n{}",
            self.site, self.kid, self.sid
        )
    }
}

struct KeyLength;
impl hkdf::KeyType for KeyLength {
    fn len(&self) -> usize {
        32
    }
}

fn derive_key(shared: &[u8], salt: &[u8], info: &str) -> Result<aead::LessSafeKey, String> {
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(shared);
    let parts = [info.as_bytes()];
    let mut bytes = [0u8; 32];
    prk.expand(&parts, KeyLength)
        .map_err(|_| "播放密钥派生失败")?
        .fill(&mut bytes)
        .map_err(|_| "播放密钥派生失败")?;
    aead::UnboundKey::new(&aead::AES_256_GCM, &bytes)
        .map(aead::LessSafeKey::new)
        .map_err(|_| "播放密钥无效".into())
}

fn nonce() -> aead::Nonce {
    let mut nonce = [0u8; 12];
    nonce[8..].copy_from_slice(&SEQUENCE.to_be_bytes());
    aead::Nonce::assume_unique_for_key(nonce)
}

fn decode_result(result: Value, hello: &Hello, key: &aead::LessSafeKey) -> Result<Value, String> {
    if result["v"] != VERSION
        || result["type"] != "result"
        || result["kid"] != hello.kid
        || result["sid"] != hello.sid
        || result["seq"] != SEQUENCE
        || result["ts"] != hello.server_time
    {
        return Err("播放加密响应不匹配".into());
    }
    let mut ciphertext = URL_SAFE_NO_PAD
        .decode(result["ciphertext"].as_str().unwrap_or(""))
        .map_err(|_| "播放响应编码无效")?;
    let aad = format!("{}\nres\n{SEQUENCE}\n{}", hello.info(), hello.server_time);
    let plain = key
        .open_in_place(nonce(), aead::Aad::from(aad.as_bytes()), &mut ciphertext)
        .map_err(|_| "播放响应校验失败")?;
    let inner: Value = serde_json::from_slice(plain).map_err(|_| "播放响应格式无效")?;
    let status = inner["status"].as_u64().ok_or("播放响应缺少状态")?;
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}：无法获取播放地址"));
    }
    let body = STANDARD
        .decode(inner["body"].as_str().ok_or("播放响应缺少正文")?)
        .map_err(|_| "播放正文编码无效")?;
    let value: Value = serde_json::from_slice(&body).map_err(|_| "播放信息格式无效")?;
    if value["status"].as_i64() != Some(1) {
        // 不把任意服务端正文或凭据写入 debug.log。
        return Err("播放接口拒绝了本次请求，请稍后重试".into());
    }
    Ok(value["data"].clone())
}

pub async fn prepare(origin: &str, token: Option<&str>, id: i64, ep: u32) -> Result<Value, String> {
    if id <= 0 || ep == 0 || ep > i32::MAX as u32 {
        return Err("视频或分集编号无效".into());
    }
    if token.is_some() && !crate::net::trusted_auth_origin(origin) {
        return Err("镜像线路仅供游客浏览".into());
    }
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        prepare_inner(origin, token, id, ep),
    )
    .await
    .map_err(|_| "获取播放地址超时")?
}

async fn prepare_inner(
    origin: &str,
    token: Option<&str>,
    id: i64,
    ep: u32,
) -> Result<Value, String> {
    // 每次取址建立独立短会话，序号从 1 开始；不缓存会话、不复用 nonce。
    let private =
        agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &SystemRandom::new())
            .map_err(|_| "无法建立播放加密会话")?;
    let public = private
        .compute_public_key()
        .map_err(|_| "无法生成播放公钥")?;
    let hello: Hello = serde_json::from_value(
        crate::net::playback_post(
            origin,
            &json!({
                "v": VERSION, "type": "hello", "client_pub": URL_SAFE_NO_PAD.encode(public.as_ref())
            }),
        )
        .await?,
    )
    .map_err(|_| "播放加密握手响应格式无效")?;
    hello.validate()?;
    let server_pub = URL_SAFE_NO_PAD
        .decode(&hello.server_pub)
        .map_err(|_| "播放服务公钥无效")?;
    let salt = URL_SAFE_NO_PAD
        .decode(&hello.salt)
        .map_err(|_| "播放服务盐值无效")?;
    if salt.len() != 32 {
        return Err("播放服务盐值长度无效".into());
    }
    let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, server_pub);
    let (request_key, response_key) = agreement::agree_ephemeral(private, &peer, |shared| {
        Ok::<_, String>((
            derive_key(shared, &salt, &format!("{}\nreq", hello.info()))?,
            derive_key(shared, &salt, &format!("{}\nres", hello.info()))?,
        ))
    })
    .map_err(|_| "播放密钥协商失败")??;
    let mut packet = json!({"request": {
        "method": "POST", "path": format!("/api/videos/{id}/episodes/{ep}/playback"),
        "body": STANDARD.encode(b"{}"), "content_type": "application/json"
    }});
    if let Some(token) = token.filter(|t| !t.is_empty()) {
        packet["authorization"] = json!(format!("Bearer {token}"));
    }
    let mut ciphertext = serde_json::to_vec(&packet).map_err(|_| "播放请求格式无效")?;
    let aad = format!("{}\nreq\n{SEQUENCE}\n{}", hello.info(), hello.server_time);
    request_key
        .seal_in_place_append_tag(nonce(), aead::Aad::from(aad.as_bytes()), &mut ciphertext)
        .map_err(|_| "播放请求加密失败")?;
    let result = crate::net::playback_post(origin, &json!({
        "v": VERSION, "type": "call", "kid": hello.kid, "sid": hello.sid,
        "seq": SEQUENCE, "ts": hello.server_time, "ciphertext": URL_SAFE_NO_PAD.encode(ciphertext)
    })).await?;
    decode_result(result, &hello, &response_key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello() -> Hello {
        Hello {
            v: VERSION,
            kind: "hello".into(),
            kid: "key-one".into(),
            sid: "session_1234567890".into(),
            site: "media.example".into(),
            server_pub: String::new(),
            salt: String::new(),
            server_time: 1000,
            expires_at: 1300,
        }
    }

    #[test]
    fn encrypted_response_checks_binding_and_authentication() {
        let hello = hello();
        let key = derive_key(
            b"shared-test-secret",
            &[7u8; 32],
            &format!("{}\nres", hello.info()),
        )
        .unwrap();
        let inner = json!({ "status": 200, "headers": {}, "body": STANDARD.encode(
            serde_json::to_vec(&json!({"status":1,"data":{"streams":{"main":"sample"}}})).unwrap()) });
        let mut ciphertext = serde_json::to_vec(&inner).unwrap();
        let aad = format!("{}\nres\n{SEQUENCE}\n{}", hello.info(), hello.server_time);
        key.seal_in_place_append_tag(nonce(), aead::Aad::from(aad.as_bytes()), &mut ciphertext)
            .unwrap();
        let result = json!({"v":VERSION,"type":"result","kid":hello.kid,"sid":hello.sid,
            "seq":SEQUENCE,"ts":hello.server_time,"ciphertext":URL_SAFE_NO_PAD.encode(&ciphertext)});
        assert_eq!(
            decode_result(result.clone(), &hello, &key).unwrap()["streams"]["main"],
            "sample"
        );
        let mut wrong_sequence = result.clone();
        wrong_sequence["seq"] = json!(2);
        assert!(decode_result(wrong_sequence, &hello, &key).is_err());
        let mut wrong_session = result.clone();
        wrong_session["sid"] = json!("other-session");
        assert!(decode_result(wrong_session, &hello, &key).is_err());
        ciphertext[0] ^= 1;
        let mut tampered = result.clone();
        tampered["ciphertext"] = json!(URL_SAFE_NO_PAD.encode(ciphertext));
        assert!(decode_result(tampered, &hello, &key).is_err());
        let request_key = derive_key(
            b"shared-test-secret",
            &[7u8; 32],
            &format!("{}\nreq", hello.info()),
        )
        .unwrap();
        assert!(decode_result(result, &hello, &request_key).is_err());
    }

    #[test]
    fn handshake_rejects_invalid_lifetime_and_identifiers() {
        let mut hello = hello();
        hello.validate().unwrap();
        hello.expires_at = hello.server_time + MAX_SESSION_SECONDS + 1;
        assert!(hello.validate().is_err());
        hello.expires_at = hello.server_time;
        assert!(hello.validate().is_err());
        hello.expires_at = hello.server_time + 60;
        hello.sid = "invalid\nsession".into();
        assert!(hello.validate().is_err());
    }
}
