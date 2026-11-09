//! 媒体层：封面解密 + HLS 代理。
//!
//! 站点封面是 AES-128-CBC 密文，m3u8 内部分片/key 又可能被 WebView 的 CORS 拦住，
//! 所以图片与视频统一走本地自定义协议（Windows/Android 为 `http://hg.localhost`，
//! macOS 为 `hg://localhost`，见 [`LOCAL_ORIGIN`]）。

use crate::net::fetch_bytes;
use aes::cipher::{BlockDecrypt, KeyInit};
use aes::Aes128;
use once_cell::sync::Lazy;
use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use ring::rand::{SecureRandom, SystemRandom};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::http::{Response, StatusCode};

/// 本平台自定义协议的完整基址。
///
/// Tauri 的自定义协议地址随平台不同：Windows / Android 是 `http://<scheme>.localhost`，
/// macOS / iOS / Linux 是 `<scheme>://localhost`。m3u8 里的分片与 key 地址按它改写。
/// 前端 `src/api.ts` 的 `LOCAL` 必须与此一致，否则封面与视频全部加载失败。
#[cfg(any(target_os = "windows", target_os = "android"))]
pub const LOCAL_ORIGIN: &str = "http://hg.localhost";
#[cfg(not(any(target_os = "windows", target_os = "android")))]
pub const LOCAL_ORIGIN: &str = "hg://localhost";

const IMG_KEY: &[u8] = b"f5d965df75336270";
const IMG_IV: &[u8] = b"97b60394abc2fbe1";

static IMG_CACHE: Lazy<Mutex<HashMap<String, Arc<Vec<u8>>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static PL_CACHE: Lazy<Mutex<HashMap<String, Arc<String>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

const IMG_CACHE_MAX: usize = 240;
const PL_CACHE_MAX: usize = 60;
const PLAYBACK_CONTEXT_MAX: usize = 64;
const PLAYBACK_CONTEXT_TTL: Duration = Duration::from_secs(3600);
const CONTEXT_BYTES: usize = 16;
const CDN_APP_HEADER: &str = "x-cdn-app";

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct CdnAuth {
    m3u8_enabled: bool,
    other_enabled: bool,
    auth_key: String,
    auth_value: String,
    app: String,
}

struct PlaybackContext {
    manifest: url::Url,
    referer: String,
    headers: HeaderMap,
    playlist_auth: bool,
    segment_auth: bool,
    created: Instant,
}

static PLAYBACK_CONTEXTS: Lazy<Mutex<HashMap<String, Arc<PlaybackContext>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

impl PlaybackContext {
    fn headers_for(&self, target: &str) -> Option<HeaderMap> {
        let target = url::Url::parse(target).ok()?;
        if target.origin() != self.manifest.origin() {
            return None;
        }
        let path = target.path().to_ascii_lowercase();
        let playlist = path.ends_with(".m3u8");
        let segment = [".ts", ".m4s", ".key"]
            .iter()
            .any(|ext| path.ends_with(ext));
        ((playlist && self.playlist_auth) || (segment && self.segment_auth))
            .then(|| self.headers.clone())
    }
}

/// 前端只得到带随机上下文编号的代理地址，CDN 头和播放凭据不进入 URL/日志/磁盘。
pub fn register_playback(origin: &str, payload: &serde_json::Value) -> Result<String, String> {
    if payload["manifest_method"] != "GET" {
        return Err("播放清单请求方式不受支持".into());
    }
    let raw = payload["streams"]["main"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("播放接口未返回可用的视频地址")?;
    let manifest = crate::network_policy::parse_target(raw)?;
    if manifest.scheme() != "https" {
        return Err("播放地址必须使用 HTTPS".into());
    }
    let auth: CdnAuth = serde_json::from_value(
        payload
            .get("cdn")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({})),
    )
    .map_err(|_| "媒体鉴权配置格式无效")?;
    let mut headers = HeaderMap::new();
    if auth.m3u8_enabled || auth.other_enabled {
        let name =
            HeaderName::from_bytes(auth.auth_key.as_bytes()).map_err(|_| "媒体鉴权头无效")?;
        // 服务端配置只能提供专用 X-* 头，不能覆盖 Cookie、Host、Authorization 等通用凭据。
        if !name.as_str().starts_with("x-")
            || name == CDN_APP_HEADER
            || auth.auth_value.is_empty()
            || auth.auth_value.len() > 1024
            || auth.app.is_empty()
            || auth.app.len() > 64
        {
            return Err("媒体鉴权配置无效".into());
        }
        headers.insert(
            name,
            HeaderValue::from_str(&auth.auth_value).map_err(|_| "媒体鉴权值无效")?,
        );
        headers.insert(
            CDN_APP_HEADER,
            HeaderValue::from_str(&auth.app).map_err(|_| "媒体应用标识无效")?,
        );
    }
    let context = Arc::new(PlaybackContext {
        manifest,
        referer: format!("{origin}/"),
        headers,
        playlist_auth: auth.m3u8_enabled,
        segment_auth: auth.other_enabled,
        created: Instant::now(),
    });
    let mut bytes = [0u8; CONTEXT_BYTES];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "无法建立媒体播放上下文")?;
    let key = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let mut contexts = PLAYBACK_CONTEXTS
        .lock()
        .map_err(|_| "媒体播放上下文锁失败")?;
    contexts.retain(|_, value| value.created.elapsed() < PLAYBACK_CONTEXT_TTL);
    if contexts.len() >= PLAYBACK_CONTEXT_MAX {
        if let Some(oldest) = contexts
            .iter()
            .min_by_key(|(_, value)| value.created)
            .map(|(key, _)| key.clone())
        {
            contexts.remove(&oldest);
        }
    }
    contexts.insert(key.clone(), context);
    Ok(format!("{LOCAL_ORIGIN}/hls?p={key}"))
}

async fn fetch_for_context(
    url: &str,
    context: Option<&PlaybackContext>,
) -> Result<(Option<String>, Vec<u8>), String> {
    match context {
        Some(context) => {
            crate::net::fetch_media_bytes(url, &context.referer, context.headers_for(url)).await
        }
        None => fetch_bytes(url).await,
    }
}

pub fn clear_playback_contexts() {
    if let Ok(mut contexts) = PLAYBACK_CONTEXTS.lock() {
        contexts.clear();
    }
    if let Ok(mut playlists) = PL_CACHE.lock() {
        playlists.clear();
    }
}

fn cache_put<V: Clone>(map: &Mutex<HashMap<String, V>>, key: String, val: V, max: usize) {
    let mut m = map.lock().unwrap();
    if m.len() >= max {
        if let Some(k) = m.keys().next().cloned() {
            m.remove(&k);
        }
    }
    m.insert(key, val);
}

/* ───────────────────────── AES ───────────────────────── */

fn aes_cbc_decrypt(data: &[u8]) -> Option<Vec<u8>> {
    if data.is_empty() || data.len() % 16 != 0 {
        return None;
    }
    let cipher = Aes128::new_from_slice(IMG_KEY).ok()?;
    let mut prev = [0u8; 16];
    prev.copy_from_slice(IMG_IV);
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks(16) {
        let mut ct = [0u8; 16];
        ct.copy_from_slice(chunk);
        let mut block = ct;
        cipher.decrypt_block((&mut block).into());
        for i in 0..16 {
            out.push(block[i] ^ prev[i]);
        }
        prev = ct;
    }
    Some(out)
}

fn strip_padding(mut buf: Vec<u8>) -> Vec<u8> {
    let pad = *buf.last().unwrap_or(&0) as usize;
    if pad > 0 && pad <= 16 && buf.len() >= pad {
        if buf[buf.len() - pad..].iter().all(|b| *b as usize == pad) {
            buf.truncate(buf.len() - pad);
        }
    }
    buf
}

fn image_kind(buf: &[u8]) -> Option<&'static str> {
    if buf.len() < 12 {
        return None;
    }
    if buf[0] == 0xFF && buf[1] == 0xD8 {
        return Some("image/jpeg");
    }
    if buf.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        return Some("image/png");
    }
    if buf.starts_with(b"GIF8") {
        return Some("image/gif");
    }
    if buf.starts_with(b"RIFF") && &buf[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// 取封面：命中缓存直接返回，否则下载并按需解密。
pub async fn load_image(url: &str) -> Result<(String, Arc<Vec<u8>>), String> {
    if let Some(hit) = IMG_CACHE.lock().unwrap().get(url).cloned() {
        let kind = image_kind(&hit).unwrap_or("image/jpeg");
        return Ok((kind.to_string(), hit));
    }
    let (_, bytes) = fetch_bytes(url).await?;
    let plain = if image_kind(&bytes).is_some() {
        bytes
    } else {
        let dec = aes_cbc_decrypt(&bytes).ok_or_else(|| "封面解密失败".to_string())?;
        strip_padding(dec)
    };
    let kind = image_kind(&plain).unwrap_or("image/jpeg").to_string();
    let arc = Arc::new(plain);
    cache_put(&IMG_CACHE, url.to_string(), arc.clone(), IMG_CACHE_MAX);
    Ok((kind, arc))
}

/* ───────────────────────── HLS ───────────────────────── */

fn enc(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

fn dec(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().to_string()
}

fn absolutize(base: &str, target: &str) -> String {
    if target.starts_with("http://") || target.starts_with("https://") {
        return target.to_string();
    }
    if let Ok(b) = url::Url::parse(base) {
        if let Ok(j) = b.join(target) {
            return j.to_string();
        }
    }
    target.to_string()
}

/// 属性列表中的引号内可含逗号，不能直接 split(',')。
fn hls_attributes(line: &str) -> Option<(&str, Vec<&str>)> {
    let (tag, rest) = line.split_once(':')?;
    let mut attributes = Vec::new();
    let mut quoted = false;
    let mut start = 0;
    for (i, ch) in rest.char_indices() {
        if ch == '"' {
            quoted = !quoted;
        }
        if ch == ',' && !quoted {
            attributes.push(&rest[start..i]);
            start = i + 1;
        }
    }
    attributes.push(&rest[start..]);
    Some((tag, attributes))
}

fn map_uri_attributes(line: &str, mut rewrite: impl FnMut(&str) -> String) -> String {
    let Some((tag, attributes)) = hls_attributes(line) else {
        return line.to_string();
    };
    let mapped: Vec<String> = attributes
        .into_iter()
        .map(|attribute| {
            if let Some((key, value)) = attribute.split_once('=') {
                if key.trim() == "URI" {
                    if let Some(uri) = value
                        .trim()
                        .strip_prefix('"')
                        .and_then(|v| v.strip_suffix('"'))
                    {
                        return format!("{key}=\"{}\"", rewrite(uri));
                    }
                }
            }
            attribute.to_string()
        })
        .collect();
    format!("{tag}:{}", mapped.join(","))
}

#[cfg(test)]
fn rewrite_uri_attr(line: &str, prefix: &str) -> String {
    rewrite_uri_attr_with_context(line, prefix, None)
}

fn proxy_segment(prefix: &str, target: &str, context: Option<&str>) -> String {
    let mut url = format!("{prefix}/seg?u={}", enc(target));
    if let Some(key) = context {
        url.push_str(&format!("&p={key}"));
    }
    url
}

fn rewrite_uri_attr_with_context(line: &str, prefix: &str, context: Option<&str>) -> String {
    map_uri_attributes(line, |uri| proxy_segment(prefix, uri, context))
}

/// 取 m3u8 并把 key / 分片地址改写到本地协议。
async fn load_playlist_with_context(
    url: &str,
    prefix: &str,
    key: Option<&str>,
    context: Option<&PlaybackContext>,
) -> Result<Arc<String>, String> {
    let cache_key = format!("{prefix}:{}:{url}", key.unwrap_or_default());
    if let Some(hit) = PL_CACHE.lock().unwrap().get(&cache_key).cloned() {
        return Ok(hit);
    }
    let (_, bytes) = fetch_for_context(url, context).await?;
    let text = std::str::from_utf8(&bytes).map_err(|_| "播放列表不是 UTF-8 文本")?;
    if !text.trim_start_matches('\u{feff}').starts_with("#EXTM3U") {
        return Err("响应不是 HLS 播放列表".into());
    }
    let arc = Arc::new(rewrite_playlist_with_context(url, text, prefix, key));
    cache_put(&PL_CACHE, cache_key, arc.clone(), PL_CACHE_MAX);
    Ok(arc)
}

#[cfg(test)]
fn rewrite_playlist(url: &str, text: &str, prefix: &str) -> String {
    rewrite_playlist_with_context(url, text, prefix, None)
}

fn rewrite_playlist_with_context(
    url: &str,
    text: &str,
    prefix: &str,
    context: Option<&str>,
) -> String {
    let mut out = String::with_capacity(text.len() + 512);
    for raw in text.trim_start_matches('\u{feff}').lines() {
        let line = raw.trim_end();
        if line.is_empty() {
            out.push('\n');
            continue;
        }
        if line.starts_with('#') {
            // 禁用可派生 URL 的内容引导；DATERANGE 保留时间线/SCTE-35 元数据。
            if line.starts_with("#EXT-X-CONTENT-STEERING:") {
                continue;
            }
            if line.starts_with("#EXT-X-DATERANGE:") {
                if let Some((tag, attributes)) = hls_attributes(line) {
                    let safe: Vec<&str> = attributes
                        .into_iter()
                        .filter(|attribute| {
                            let key = attribute
                                .split('=')
                                .next()
                                .unwrap_or("")
                                .trim()
                                .to_ascii_uppercase();
                            !key.contains("URI") && key != "X-ASSET-LIST"
                        })
                        .collect();
                    out.push_str(&format!("{tag}:{}\n", safe.join(",")));
                }
                continue;
            }
            if line.contains("URI=\"") {
                // KEY/MAP/MEDIA/SESSION-KEY/PART/PRELOAD-HINT 等 URI 都必须经过目标检查。
                let absolute = absolutize_uri_in_attr(url, line);
                out.push_str(&rewrite_uri_attr_with_context(&absolute, prefix, context));
            } else {
                out.push_str(line);
            }
        } else {
            let absolute = absolutize(url, line);
            out.push_str(&proxy_segment(prefix, &absolute, context));
        }
        out.push('\n');
    }

    out
}

fn absolutize_uri_in_attr(base: &str, line: &str) -> String {
    map_uri_attributes(line, |uri| absolutize(base, uri))
}

/// 清空封面 / 播放列表内存缓存
pub fn clear_caches() {
    if let Ok(mut g) = IMG_CACHE.lock() {
        g.clear();
    }
    if let Ok(mut g) = PL_CACHE.lock() {
        g.clear();
    }
}

/* ───────────────────────── 协议响应 ───────────────────────── */

fn respond(status: u16, ctype: &str, body: Vec<u8>, cache: &str) -> Response<Vec<u8>> {
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::OK))
        .header("Content-Type", ctype)
        .header("X-Content-Type-Options", "nosniff")
        .header("Cache-Control", cache)
        .body(body)
        .unwrap_or_else(|_| Response::new(Vec::new()))
}

/// 主清单的子清单也经 /seg 返回：必须继续改写，不能把内层 URL 原样交给播放器。
#[cfg(test)]
fn segment_response(
    target: &str,
    ctype: Option<&str>,
    bytes: Vec<u8>,
    origin: &str,
) -> Response<Vec<u8>> {
    segment_response_with_context(target, ctype, bytes, origin, None)
}

fn segment_response_with_context(
    target: &str,
    ctype: Option<&str>,
    bytes: Vec<u8>,
    origin: &str,
    context: Option<&str>,
) -> Response<Vec<u8>> {
    let playlist = ctype.is_some_and(|t| t.to_ascii_lowercase().contains("mpegurl"))
        || bytes.starts_with(b"#EXTM3U")
        || bytes.starts_with(b"\xef\xbb\xbf#EXTM3U");
    if playlist {
        return match std::str::from_utf8(&bytes) {
            Ok(text) => respond(
                200,
                "application/vnd.apple.mpegurl",
                rewrite_playlist_with_context(target, text, origin, context).into_bytes(),
                // Rewriting nested URIs does not require disabling the upstream cache policy.
                "public, max-age=3600",
            ),
            Err(_) => respond(
                502,
                "text/plain",
                b"invalid playlist encoding".to_vec(),
                "no-store",
            ),
        };
    }
    respond(
        200,
        ctype.unwrap_or("application/octet-stream"),
        bytes,
        "public, max-age=3600",
    )
}

/// 统一入口：`<LOCAL_ORIGIN>/img?u=` / `hls?u=` / `seg?u=`
///
/// `origin` 是本平台自定义协议的完整基址，见 [`LOCAL_ORIGIN`]；用于改写 m3u8 里的
/// 分片与 key 地址。正常情况下调用方直接传 [`LOCAL_ORIGIN`]。
pub async fn handle(path: &str, query: &str, origin: &str) -> Response<Vec<u8>> {
    let context_key = query.split('&').find_map(|kv| kv.strip_prefix("p="));
    let context = if let Some(key) = context_key {
        let context = PLAYBACK_CONTEXTS
            .lock()
            .ok()
            .and_then(|contexts| contexts.get(key).cloned())
            .filter(|context| context.created.elapsed() < PLAYBACK_CONTEXT_TTL);
        if context.is_none() {
            return respond(
                410,
                "text/plain",
                "播放信息已失效，请重新获取".as_bytes().to_vec(),
                "no-store",
            );
        }
        context
    } else {
        None
    };
    let target = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("u="))
        .map(dec)
        .or_else(|| {
            context
                .as_ref()
                .filter(|_| path == "/hls")
                .map(|c| c.manifest.to_string())
        })
        .unwrap_or_default();

    if target.is_empty() {
        return respond(400, "text/plain", b"missing u".to_vec(), "no-store");
    }

    let started = std::time::Instant::now();
    let route = match path {
        "/img" => "image",
        "/hls" => "playlist",
        "/seg" => "segment_or_key",
        _ => "unknown",
    };
    crate::playback_log::record(
        "proxy.begin",
        serde_json::json!({"route": route, "resource": crate::playback_log::resource_id(&target)}),
    );
    let mut response = match path {
        "/img" => match load_image(&target).await {
            Ok((kind, bytes)) => {
                respond(200, &kind, bytes.as_ref().clone(), "public, max-age=86400")
            }
            Err(e) => respond(502, "text/plain", e.into_bytes(), "no-store"),
        },
        "/hls" => {
            match load_playlist_with_context(&target, origin, context_key, context.as_deref()).await
            {
                Ok(text) => respond(
                    200,
                    "application/vnd.apple.mpegurl",
                    text.as_bytes().to_vec(),
                    "no-store",
                ),
                Err(e) => respond(502, "text/plain", e.into_bytes(), "no-store"),
            }
        }
        "/seg" => match fetch_for_context(&target, context.as_deref()).await {
            Ok((ctype, bytes)) => {
                segment_response_with_context(&target, ctype.as_deref(), bytes, origin, context_key)
            }
            Err(e) => respond(502, "text/plain", e.into_bytes(), "no-store"),
        },
        _ => respond(404, "text/plain", b"not found".to_vec(), "no-store"),
    };
    if context_key.is_some() {
        response
            .headers_mut()
            .insert("Cache-Control", HeaderValue::from_static("no-store"));
    }
    crate::playback_log::record(
        "proxy.end",
        serde_json::json!({"route": route, "resource": crate::playback_log::resource_id(&target), "status": response.status().as_u16(), "bytes": response.body().len(), "elapsed_ms": started.elapsed().as_millis() as u64}),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_context_keeps_credentials_private_and_scopes_them_to_cdn_origin() {
        let payload = serde_json::json!({"manifest_method":"GET", "streams":{"main":"https://cdn.example/list.m3u8"},
            "cdn":{"m3u8Enabled":true,"otherEnabled":true,"authKey":"x-media-auth","authValue":"private-value","app":"player"}});
        let source = register_playback("https://site.example", &payload).unwrap();
        assert!(!source.contains("private-value") && !source.contains("cdn.example"));
        let key = source.split("?p=").nth(1).unwrap();
        let context = PLAYBACK_CONTEXTS.lock().unwrap().get(key).unwrap().clone();
        assert_eq!(
            context.headers_for("https://cdn.example/key.key").unwrap()["x-media-auth"],
            "private-value"
        );
        assert!(context
            .headers_for("https://other.example/key.key")
            .is_none());
        assert!(context
            .headers_for("http://cdn.example/segment.ts")
            .is_none());
        assert!(context
            .headers_for("https://cdn.example/image.jpg")
            .is_none());
        let body = rewrite_playlist_with_context(
            "https://cdn.example/list.m3u8",
            "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.key\"\nchild.m3u8\n",
            LOCAL_ORIGIN,
            Some(key),
        );
        assert_eq!(body.matches(&format!("&p={key}")).count(), 2);
        assert!(!body.contains("private-value"));
        let mut invalid = payload.clone();
        invalid["cdn"]["authKey"] = serde_json::json!("Authorization");
        assert!(register_playback("https://site.example", &invalid).is_err());
        invalid["cdn"]["authKey"] = serde_json::json!("x-media-auth");
        invalid["cdn"]["authValue"] = serde_json::json!("value\r\nCookie: secret");
        assert!(register_playback("https://site.example", &invalid).is_err());
    }

    #[tokio::test]
    async fn unknown_playback_context_fails_without_fetching_upstream() {
        let response = handle("/hls", "p=missing-session", LOCAL_ORIGIN).await;
        assert_eq!(response.status(), 410);
    }

    #[test]
    fn dateranges_keep_timing_and_scte_metadata_but_drop_external_resources() {
        let line = "#EXTM3U\n#EXT-X-DATERANGE:ID=\"event\",START-DATE=\"2026-11-07T00:00:00Z\",DURATION=4,SCTE35-OUT=0xFC,X-TEXT=\"a,b\",X-ASSET-URI=\"https://evil.example/ad\",X-ASSET-LIST=\"https://evil.example/list.json\"\n";
        let out = rewrite_playlist("https://cdn.example/master.m3u8", line, LOCAL_ORIGIN);
        assert!(out.contains("DURATION=4,SCTE35-OUT=0xFC,X-TEXT=\"a,b\""));
        assert!(out.contains("START-DATE="));
        assert!(!out.contains("evil.example") && !out.contains("X-ASSET"));
    }

    #[test]
    fn every_uri_attribute_is_rewritten_without_splitting_quoted_commas() {
        let line = "#EXT-X-MEDIA:NAME=\"audio,main\",URI=\"first.m3u8\",URI=\"second.m3u8\"";
        let absolute = absolutize_uri_in_attr("https://cdn.example/master.m3u8", line);
        let rewritten = rewrite_uri_attr(&absolute, LOCAL_ORIGIN);
        assert_eq!(
            rewritten.matches(&format!("{LOCAL_ORIGIN}/seg?u=")).count(),
            2
        );
        assert!(rewritten.contains("NAME=\"audio,main\""));
        assert!(!rewritten.contains("URI=\"https://"));
    }

    #[test]
    fn nested_playlists_and_alternate_uris_stay_behind_proxy() {
        let text = "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,URI=\"audio/list.m3u8\"\n#EXT-X-SESSION-KEY:METHOD=AES-128,URI=\"http://127.0.0.1/key\"\n#EXT-X-PART:DURATION=1,URI=\"part.ts\"\nvariant/list.m3u8\n";
        let response = segment_response(
            "https://cdn.example/master.m3u8",
            Some("application/octet-stream"),
            text.as_bytes().to_vec(),
            LOCAL_ORIGIN,
        );
        let body = std::str::from_utf8(response.body()).unwrap();
        assert_eq!(body.matches(&format!("{LOCAL_ORIGIN}/seg?u=")).count(), 4);
        assert!(!body.contains("URI=\"http://127.0.0.1"));
        assert!(body.contains("https%3A%2F%2Fcdn%2Eexample%2Faudio%2Flist%2Em3u8"));
        assert_eq!(
            response.headers()["Content-Type"],
            "application/vnd.apple.mpegurl"
        );
        assert_eq!(response.headers()["X-Content-Type-Options"], "nosniff");
        assert!(!response
            .headers()
            .contains_key("Access-Control-Allow-Origin"));
        let optional = rewrite_playlist("https://cdn.example/list.m3u8",
            "#EXTM3U\n#EXT-X-CONTENT-STEERING:SERVER-URI=\"https://other.example/list.json\"\n#EXT-X-DATERANGE:ID=\"ad\",X-ASSET-LIST=\"http://127.0.0.1/assets\"\nsegment.ts\n", LOCAL_ORIGIN);
        assert!(!optional.contains("CONTENT-STEERING") && !optional.contains("X-ASSET-LIST"));
        assert!(optional.contains("#EXT-X-DATERANGE:ID=\"ad\""));
        let bom = segment_response(
            "https://cdn.example/list.m3u8",
            None,
            b"\xef\xbb\xbf#EXTM3U\nsegment.ts\n".to_vec(),
            LOCAL_ORIGIN,
        );
        assert!(bom.body().starts_with(b"#EXTM3U\n"));
    }

    /// 联网测试：默认跳过，手动跑 `cargo test --lib -- --ignored`。
    /// 依赖上游当前播放服务；仅请求一份清单、一个密钥和一个分片，不保存内容。
    #[tokio::test]
    #[ignore = "联网测试：依赖上游实时播放地址，手动用 --ignored 运行"]
    async fn live_play_chain() {
        let payload = crate::playback::prepare(&crate::net::base(), None, 12, 1)
            .await
            .expect("playback prepare");
        let source = register_playback(&crate::net::base(), &payload).expect("register playback");
        let q = source.split('?').nth(1).unwrap();
        let resp = handle("/hls", q, LOCAL_ORIGIN).await;
        assert_eq!(resp.status(), 200, "playlist fetch failed");
        let body = String::from_utf8_lossy(resp.body()).to_string();
        assert!(body.contains("#EXTM3U"));
        if let Some(key) = body
            .lines()
            .find(|line| line.starts_with("#EXT-X-KEY:"))
            .and_then(|line| line.split("URI=\"").nth(1))
            .and_then(|value| value.split('"').next())
        {
            let key = url::Url::parse(key).unwrap();
            let response = handle(key.path(), key.query().unwrap(), LOCAL_ORIGIN).await;
            assert_eq!(response.status(), 200, "key fetch failed");
            assert_eq!(response.body().len(), 16, "invalid AES-128 key");
        }
        // 取一个分片验证
        let seg_line = body
            .lines()
            .find(|l| !l.starts_with('#') && l.contains("/seg?u="))
            .expect("seg line")
            .to_string();
        let seg_url = url::Url::parse(&seg_line).unwrap();
        let seg = handle(seg_url.path(), seg_url.query().unwrap(), LOCAL_ORIGIN).await;
        assert_eq!(seg.status(), 200);
        assert!(seg.body().len() > 1000);
        println!(
            "playback handshake, playlist, key and first segment succeeded ({} bytes)",
            seg.body().len()
        );
    }

    /// 离线回归：不再依赖带 auth_key 时效的真实上游地址（旧地址已过期，会 403）。
    /// 与真实播放列表同构：带签名的分片地址 + 加密 KEY + 相对路径分片。
    #[test]
    fn playlist_rewrite_works() {
        let base = "https://cdn.example/videos5/abc/abc.m3u8?auth_key=9999999999-0-0-deadbeef&v=3&via=hgdj";
        let text = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin?auth_key=9999999999-0-0-deadbeef\"\n#EXTINF:4,\nseg-1.ts?auth_key=9999999999-0-0-deadbeef\n";
        let body = rewrite_playlist(base, text, LOCAL_ORIGIN);

        assert!(body.contains("#EXTM3U"), "not a playlist: {body}");
        assert!(
            body.contains(&format!("{LOCAL_ORIGIN}/seg?u=")),
            "segments not rewritten: {body}"
        );
        // 分片必须绝对化到源站，并保留签名查询串。
        assert!(
            body.contains(
                "https%3A%2F%2Fcdn%2Eexample%2Fvideos5%2Fabc%2Fseg%2D1%2Ets%3Fauth%5Fkey%3D"
            ),
            "segment not absolutized with query: {body}"
        );
        // 加密 KEY 也要走本地代理，否则 hls.js 会直接打源站被 CORS 拦住。
        assert!(
            body.contains("URI=\"") && body.contains("%2Fkey%2Ebin%3Fauth%5Fkey%3D"),
            "key uri not rewritten: {body}"
        );
        assert!(!body.contains("URI=\"key.bin"), "key uri left relative");
    }

    /// 离线回归：用已知明文 + 项目内置 key/IV 构造密文，验证解密、去填充与类型识别。
    /// 真实封面抓取那条仍保留为 `#[ignore]` 联网测试。
    #[test]
    fn cover_decrypt_roundtrip() {
        use aes::cipher::BlockEncrypt;

        let plain = b"\xFF\xD8\xFF\xE0cover-payload-for-test".to_vec();
        let mut buf = plain.clone();
        let pad = 16 - plain.len() % 16;
        buf.extend(std::iter::repeat_n(pad as u8, pad));

        let cipher = Aes128::new_from_slice(IMG_KEY).expect("key");
        let mut prev = [0u8; 16];
        prev.copy_from_slice(IMG_IV);
        let mut encrypted = Vec::with_capacity(buf.len());
        for chunk in buf.chunks(16) {
            let mut block = [0u8; 16];
            block.copy_from_slice(chunk);
            for i in 0..16 {
                block[i] ^= prev[i];
            }
            let mut out_block = block;
            cipher.encrypt_block((&mut out_block).into());
            encrypted.extend_from_slice(&out_block);
            prev = out_block;
        }

        let decrypted = aes_cbc_decrypt(&encrypted).expect("decrypt");
        let stripped = strip_padding(decrypted);
        assert_eq!(stripped, plain);
        assert_eq!(image_kind(&stripped), Some("image/jpeg"));
    }

    /// 联网测试：默认跳过。封面地址带时效签名，不能硬编码进仓库，
    /// 需要用当次上游返回的地址显式传入：
    /// `HGHUB_TEST_COVER_URL='https://…?auth_key=…' cargo test --lib -- --ignored`
    #[tokio::test]
    #[ignore = "联网测试：需要 HGHUB_TEST_COVER_URL，手动用 --ignored 运行"]
    async fn cover_decrypt_works() {
        let url = std::env::var("HGHUB_TEST_COVER_URL")
            .expect("需要设置 HGHUB_TEST_COVER_URL 为当前有效的封面地址");
        match load_image(&url).await {
            Ok((kind, bytes)) => {
                println!("kind={kind} len={}", bytes.len());
                assert!(kind.starts_with("image/"));
            }
            Err(e) => panic!("image failed: {e}"),
        }
    }
}
