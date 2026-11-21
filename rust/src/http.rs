//! HTTP 层：路由分发、JSON 响应、静态文件。
//!
//! 分工：真正的数据逻辑在 store.rs，这里只做「解析请求 → 调用 → 写响应」。

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use tiny_http::{Header, Method, Request, Response, StatusCode};

use crate::aggregate;
use crate::store::Store;

/// 请求体上限，与 Python 版 MAX_SESSION 一致。
const MAX_BODY: u64 = 32 << 20;

fn header(name: &[u8], value: &[u8]) -> Header {
    Header::from_bytes(name, value).expect("static header")
}

fn json_ct() -> Header {
    header(b"Content-Type", b"application/json; charset=utf-8")
}

fn no_store() -> Header {
    header(b"Cache-Control", b"no-store")
}

/// 发 JSON 响应。序列化失败时退化成 500。
pub fn send_json<T: Serialize>(req: Request, code: u16, value: &T) {
    match serde_json::to_vec(value) {
        Ok(body) => {
            let _ = req.respond(
                Response::from_data(body)
                    .with_status_code(StatusCode(code))
                    .with_header(json_ct())
                    .with_header(no_store()),
            );
        }
        Err(e) => {
            let _ = req.respond(
                Response::from_string(format!("{{\"error\":\"序列化失败: {e}\"}}"))
                    .with_status_code(StatusCode(500))
                    .with_header(json_ct()),
            );
        }
    }
}

fn send_error(req: Request, code: u16, msg: &str) {
    #[derive(Serialize)]
    struct ErrBody {
        error: String,
    }
    send_json(
        req,
        code,
        &ErrBody {
            error: msg.to_string(),
        },
    );
}

/// 204 No Content：前端对 PUT/DELETE 期望空体。
fn send_no_content(req: Request) {
    let _ = req.respond(Response::empty(StatusCode(204)));
}

/// 读请求体，超过上限直接报错。
fn read_body(req: &mut Request) -> Result<Vec<u8>, String> {
    let len = req.body_length().unwrap_or(0) as u64;
    if len > MAX_BODY {
        return Err("请求体过大".into());
    }
    let mut buf = Vec::with_capacity(len as usize);
    req.as_reader()
        .take(MAX_BODY + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("读取请求体失败: {e}"))?;
    Ok(buf)
}

/// 路径安全：拒绝 `..` 和绝对路径分量，防止目录穿越。
fn safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
    let mut out = root.to_path_buf();
    for comp in Path::new(rel).components() {
        match comp {
            Component::Normal(seg) => out.push(seg),
            // Normal 以外的（RootDir / ParentDir / CurDir / Prefix）一律拒绝
            _ => return None,
        }
    }
    Some(out)
}

fn content_type_of(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "ico" => "image/x-icon",
        "map" => "application/json; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// 静态文件，语义与 server.py 的 serve_static 一致：
///   - 目录或未知路径 → index.html（SPA fallback）
///   - 带扩展名却找不到 → 404
///   - static/ 下的资源长缓存，其余 no-cache
fn serve_static(req: Request, www: &Path, rel: &str) {
    let rel = if rel.is_empty() { "index.html" } else { rel };
    let Some(mut full) = safe_join(www, rel) else {
        send_error(req, 403, "forbidden");
        return;
    };
    if full.is_dir() {
        full = full.join("index.html");
    }
    if !full.is_file() {
        // 有扩展名说明是具体资源，缺了就是 404，不能兜底成 index.html
        if Path::new(rel).extension().is_some() {
            send_error(req, 404, "not found");
            return;
        }
        full = www.join("index.html");
        if !full.is_file() {
            send_error(req, 500, "www/index.html 缺失");
            return;
        }
    }
    let data = match std::fs::read(&full) {
        Ok(d) => d,
        Err(e) => {
            send_error(req, 500, &format!("读取失败: {e}"));
            return;
        }
    };
    let cache = if rel.starts_with("static/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let _ = req.respond(
        Response::from_data(data)
            .with_status_code(StatusCode(200))
            .with_header(header(b"Content-Type", content_type_of(&full).as_bytes()))
            .with_header(header(b"Cache-Control", cache.as_bytes())),
    );
}

/// 把 `/db/session/<id>` 拆出 id；`/db/session/<id>/rename` 返回 (id, true)。
fn parse_session_path(path: &str) -> Option<(&str, bool)> {
    let rest = path.strip_prefix("/db/session/")?;
    if rest.is_empty() {
        return None;
    }
    match rest.strip_suffix("/rename") {
        Some(id) if !id.is_empty() => Some((id, true)),
        Some(_) => None,
        None => Some((rest, false)),
    }
}

pub fn handle(mut req: Request, st: &Store, www: &Path, version: &str) {
    let method = req.method().clone();
    let url = req.url().to_string();
    let path = url.split('?').next().unwrap_or("/").to_string();

    // ---------- /local/status ----------
    if method == Method::Get && path == "/local/status" {
        #[derive(Serialize)]
        struct Status {
            version: String,
            #[serde(rename = "dataDir")]
            data_dir: String,
            port: u16,
            #[serde(rename = "syncPort")]
            sync_port: u16,
            #[serde(rename = "uptimeSec")]
            uptime_secs: u64,
            platform: String,
        }
        send_json(
            req,
            200,
            &Status {
                version: version.to_string(),
                data_dir: st.dir().display().to_string(),
                port: st.port(),
                sync_port: 8791,
                uptime_secs: st.uptime_secs(),
                platform: std::env::consts::OS.to_string(),
            },
        );
        return;
    }

    // ---------- /health ----------
    if method == Method::Get && path == "/health" {
        #[derive(Serialize)]
        struct Health {
            ok: bool,
            version: String,
            platform: String,
        }
        send_json(
            req,
            200,
            &Health {
                ok: true,
                version: version.to_string(),
                platform: std::env::consts::OS.to_string(),
            },
        );
        return;
    }

    // ---------- /local/storage ----------
    if method == Method::Get && path == "/local/storage" {
        #[derive(Serialize)]
        struct Storage {
            #[serde(rename = "activeSessions")]
            active_sessions: usize,
            #[serde(rename = "archivedSessions")]
            archived_sessions: usize,
            #[serde(rename = "activeBytes")]
            active_bytes: u64,
            #[serde(rename = "archiveBytes")]
            archive_bytes: u64,
            #[serde(rename = "mediaBytes")]
            media_bytes: u64,
            #[serde(rename = "assetsBytes")]
            assets_bytes: u64,
            #[serde(rename = "assetsDir")]
            assets_dir: String,
            #[serde(rename = "logBytes")]
            log_bytes: u64,
        }
        let count = st.summaries().map(|v| v.len()).unwrap_or(0);
        let log_path = st.dir().join("app.log");
        send_json(
            req,
            200,
            &Storage {
                active_sessions: count,
                // 归档计数需要 index.json，Rust 版尚未实现归档，先报 0
                archived_sessions: 0,
                active_bytes: st.subdir_bytes("sessions") + st.subdir_bytes("meta"),
                archive_bytes: st.subdir_bytes("archive"),
                media_bytes: st.subdir_bytes("media"),
                assets_bytes: crate::store::dir_size(www),
                assets_dir: www
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string(),
                log_bytes: std::fs::metadata(&log_path).map(|m| m.len()).unwrap_or(0),
            },
        );
        return;
    }

    // ---------- /db/summaries ----------
    if method == Method::Get && path == "/db/summaries" {
        match st.summaries() {
            Ok(list) => send_json(req, 200, &list),
            Err(e) => send_error(req, 500, &format!("读取会话失败: {e}")),
        }
        return;
    }

    // ---------- /db/session/<id>[/rename] ----------
    if let Some((raw_id, is_rename)) = parse_session_path(&path) {
        let id = urldecode(raw_id);
        if is_rename {
            if method != Method::Post {
                send_error(req, 404, "not found");
                return;
            }
            let body = match read_body(&mut req) {
                Ok(b) => b,
                Err(e) => {
                    send_error(req, 400, &e);
                    return;
                }
            };
            #[derive(serde::Deserialize)]
            struct RenameBody {
                #[serde(default)]
                title: String,
            }
            let title = serde_json::from_slice::<RenameBody>(&body)
                .map(|b| b.title)
                .unwrap_or_default();
            match st.rename_session(&id, &title) {
                Ok(()) => send_no_content(req),
                Err(e) if e == "not found" => send_error(req, 404, "not found"),
                Err(e) => send_error(req, 500, &e),
            }
            return;
        }
        match method {
            Method::Get => match st.load_session(&id) {
                Ok(Some(raw)) => {
                    let _ = req.respond(
                        Response::from_data(raw)
                            .with_status_code(StatusCode(200))
                            .with_header(header(
                                b"Content-Type",
                                b"application/json; charset=utf-8",
                            ))
                            .with_header(no_store()),
                    );
                }
                Ok(None) => send_error(req, 404, "not found"),
                Err(e) => send_error(req, 500, &format!("读取失败: {e}")),
            },
            Method::Put => {
                let body = match read_body(&mut req) {
                    Ok(b) => b,
                    Err(e) => {
                        send_error(req, 400, &e);
                        return;
                    }
                };
                match st.save_session(&id, &body) {
                    Ok(()) => send_no_content(req),
                    Err(e) => send_error(req, 500, &e), // 前端会把这句显示出来
                }
            }
            Method::Delete => match st.delete_session(&id) {
                Ok(()) => send_no_content(req),
                Err(e) => send_error(req, 500, &e),
            },
            _ => send_error(req, 404, "not found"),
        }
        return;
    }

    // ---------- /db/file/<id> ----------
    if let Some(raw_id) = path.strip_prefix("/db/file/") {
        if raw_id.is_empty() {
            send_error(req, 404, "not found");
            return;
        }
        let id = urldecode(raw_id);
        match method {
            Method::Put => {
                let mime = req
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("Content-Type"))
                    .map(|h| h.value.as_str().to_string())
                    .unwrap_or_else(|| "application/octet-stream".into());
                let image = req
                    .headers()
                    .iter()
                    .any(|h| h.field.equiv("X-Image") && h.value.as_str() == "1")
                    || mime.starts_with("image/");
                let name = req
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("X-File-Name"))
                    .map(|h| urldecode(h.value.as_str()))
                    .unwrap_or_default();
                let body = match read_body(&mut req) {
                    Ok(b) => b,
                    Err(e) => {
                        send_error(req, 400, &e);
                        return;
                    }
                };
                match st.put_file(&id, &body, &mime, &name, image) {
                    Ok(_) => send_no_content(req),
                    Err(e) => send_error(req, 500, &e),
                }
            }
            Method::Get => match st.get_file(&id) {
                Ok(Some((meta, data))) => {
                    let cache = if meta.sha.is_empty() {
                        "no-cache"
                    } else {
                        // 内容寻址：同一个 id 指向的内容不变，可以长缓存
                        "public, max-age=31536000, immutable"
                    };
                    let ctype = if meta.mime.is_empty() {
                        "application/octet-stream"
                    } else {
                        meta.mime.as_str()
                    };
                    let _ = req.respond(
                        Response::from_data(data)
                            .with_status_code(StatusCode(200))
                            .with_header(header(b"Content-Type", ctype.as_bytes()))
                            .with_header(header(b"Cache-Control", cache.as_bytes())),
                    );
                }
                Ok(None) => send_error(req, 404, "not found"),
                Err(e) => send_error(req, 500, &e),
            },
            _ => send_error(req, 404, "not found"),
        }
        return;
    }

    // ---------- /db/staging/* ----------
    if path == "/db/staging/clear" && method == Method::Post {
        match st.clear_staging() {
            Ok(()) => send_no_content(req),
            Err(e) => send_error(req, 500, &e),
        }
        return;
    }

    if let Some(raw_id) = path.strip_prefix("/db/staging/session/") {
        if method != Method::Put || raw_id.is_empty() {
            send_error(req, 404, "not found");
            return;
        }
        let id = urldecode(raw_id);
        let body = match read_body(&mut req) {
            Ok(b) => b,
            Err(e) => {
                send_error(req, 400, &e);
                return;
            }
        };
        match st.add_staging_session(&id, &body) {
            Ok(()) => send_no_content(req),
            Err(e) => send_error(req, 500, &e),
        }
        return;
    }

    if let Some(raw_id) = path.strip_prefix("/db/staging/file/") {
        if method != Method::Put || raw_id.is_empty() {
            send_error(req, 404, "not found");
            return;
        }
        let id = urldecode(raw_id);
        let mime = req
            .headers()
            .iter()
            .find(|h| h.field.equiv("Content-Type"))
            .map(|h| h.value.as_str().to_string())
            .unwrap_or_else(|| "application/octet-stream".into());
        let image = req
            .headers()
            .iter()
            .any(|h| h.field.equiv("X-Image") && h.value.as_str() == "1")
            || mime.starts_with("image/");
        let body = match read_body(&mut req) {
            Ok(b) => b,
            Err(e) => {
                send_error(req, 400, &e);
                return;
            }
        };
        match st.add_staging_file(&id, &body, &mime, image) {
            Ok(()) => send_no_content(req),
            Err(e) => send_error(req, 500, &e),
        }
        return;
    }

    if path == "/db/staging/filetypes" && method == Method::Post {
        let body = match read_body(&mut req) {
            Ok(b) => b,
            Err(e) => {
                send_error(req, 400, &e);
                return;
            }
        };
        #[derive(serde::Deserialize)]
        struct TypesBody {
            #[serde(default)]
            types: std::collections::HashMap<String, String>,
        }
        let types = serde_json::from_slice::<TypesBody>(&body)
            .map(|b| b.types)
            .unwrap_or_default();
        match st.update_staging_types(&types) {
            Ok(()) => send_no_content(req),
            Err(e) => send_error(req, 500, &e),
        }
        return;
    }

    if path == "/db/staging/commit" && method == Method::Post {
        match st.commit_staging() {
            Ok(list) => send_json(req, 200, &list),
            Err(e) => send_error(req, 500, &e),
        }
        return;
    }

    // ---------- /local/config ----------
    if path == "/local/config" {
        match method {
            Method::Get => send_json(req, 200, &store_cfg(st)),
            Method::Put => {
                let body = match read_body(&mut req) {
                    Ok(b) => b,
                    Err(e) => {
                        send_error(req, 400, &e);
                        return;
                    }
                };
                let patch: serde_json::Map<String, serde_json::Value> =
                    serde_json::from_slice(&body).unwrap_or_default();
                match st.save_config(&patch) {
                    Ok(cfg) => send_json(req, 200, &cfg),
                    Err(e) => send_error(req, 500, &e),
                }
            }
            _ => send_error(req, 404, "not found"),
        }
        return;
    }

    // ---------- /local/apikey ----------
    if path == "/local/apikey" {
        match method {
            Method::Get => {
                let key = st.api_key();
                send_json(
                    req,
                    200,
                    &serde_json::json!({
                        "configured": !key.is_empty(),
                        "hint": key_hint(&key),
                    }),
                );
            }
            Method::Put => {
                let body = match read_body(&mut req) {
                    Ok(b) => b,
                    Err(e) => {
                        send_error(req, 400, &e);
                        return;
                    }
                };
                #[derive(serde::Deserialize)]
                struct KeyBody {
                    #[serde(rename = "apiKey", default)]
                    api_key: String,
                }
                let key = serde_json::from_slice::<KeyBody>(&body)
                    .map(|b| b.api_key.trim().to_string())
                    .unwrap_or_default();
                if key.is_empty() {
                    send_error(req, 400, "apiKey 不能为空");
                    return;
                }
                match st.save_api_key(&key) {
                    Ok(()) => send_json(
                        req,
                        200,
                        &serde_json::json!({"configured": true, "hint": key_hint(&key)}),
                    ),
                    Err(e) => send_error(req, 500, &e),
                }
            }
            Method::Delete => match st.save_api_key("") {
                Ok(()) => send_no_content(req),
                Err(e) => send_error(req, 500, &e),
            },
            _ => send_error(req, 404, "not found"),
        }
        return;
    }

    // ---------- /local/usage ----------
    if path == "/local/usage" && method == Method::Get {
        let sid = query_param(&url, "session").unwrap_or_default();
        send_json(req, 200, &aggregate::usage(st, &sid));
        return;
    }

    // ---------- /local/balance ----------
    if path == "/local/balance" && method == Method::Get {
        send_json(req, 200, &fetch_balance(st));
        return;
    }

    // ---------- /local/search ----------
    if path == "/local/search" && method == Method::Get {
        let q = query_param(&url, "q").unwrap_or_default();
        let limit = query_param(&url, "limit")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(50);
        send_json(req, 200, &aggregate::search(st, &q, limit));
        return;
    }

    // ---------- /local/archived ----------
    if path == "/local/archived" && method == Method::Get {
        send_json(req, 200, &aggregate::list_archived(st));
        return;
    }

    // ---------- /local/restore/<id> ----------
    if let Some(raw_id) = path.strip_prefix("/local/restore/") {
        if method != Method::Post || raw_id.is_empty() {
            send_error(req, 404, "not found");
            return;
        }
        let id = urldecode(raw_id);
        match aggregate::restore_archived(st, &id) {
            Ok(()) => send_json(req, 200, &serde_json::json!({"restored": id})),
            Err(e) => send_error(req, 404, &e),
        }
        return;
    }

    // ---------- /local/delete（批量删）----------
    if path == "/local/delete" && method == Method::Post {
        let body = match read_body(&mut req) {
            Ok(b) => b,
            Err(e) => {
                send_error(req, 400, &e);
                return;
            }
        };
        #[derive(serde::Deserialize)]
        struct DelBody {
            #[serde(default)]
            ids: Vec<String>,
        }
        let ids = serde_json::from_slice::<DelBody>(&body)
            .map(|b| b.ids)
            .unwrap_or_default();
        let mut done = 0usize;
        for id in ids {
            if id.is_empty() {
                continue;
            }
            // 归档项：从索引移除 + 写 archiveDeleted（不写会话墓碑）
            match aggregate::delete_archived(st, &id) {
                Ok(true) => {
                    done += 1;
                    continue;
                }
                Ok(false) => {}
                Err(e) => {
                    // 索引写不了不阻断，但要让用户知道
                    eprintln!("删除归档项 {id} 失败: {e}");
                }
            }
            // 普通会话：写墓碑
            if st.delete_session(&id).is_ok() {
                done += 1;
            }
        }
        send_json(req, 200, &serde_json::json!({ "deleted": done }));
        return;
    }

    // ---------- 其余交给静态文件 ----------
    if method == Method::Get {
        serve_static(req, www, path.trim_start_matches('/'));
    } else {
        send_error(req, 404, "not found");
    }
}

/// 最小 percent-decoding：只处理 %XX，够用于会话 id 与文件名。
fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(b) = u8::from_str_radix(hex, 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 从 query string 里取一个参数（已 urldecode）。
fn query_param(url: &str, key: &str) -> Option<String> {
    let q = url.split_once('?')?.1;
    for pair in q.split('&') {
        // 没有 '=' 的片段直接跳过，不能让一个脏片段毁掉整串查询
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        if k == key {
            return Some(urldecode(v));
        }
    }
    None
}

/// 配置响应：直接返回当前配置（含默认值）。
fn store_cfg(st: &Store) -> serde_json::Value {
    serde_json::Value::Object(st.config())
}

/// Key 提示：只露最后 4 位，形如 “…abcd”。
fn key_hint(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() > 4 {
        format!("…{}", chars[chars.len() - 4..].iter().collect::<String>())
    } else {
        String::new()
    }
}

/// 把上游余额响应整理成前端要的形状。
fn build_balance(raw: &serde_json::Value) -> serde_json::Value {
    let infos = raw
        .get("balance_infos")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    // 优先人民币，没有就取第一条（与 Python 版一致）
    let pick = infos
        .iter()
        .find(|i| i.get("currency").and_then(|c| c.as_str()) == Some("CNY"))
        .or_else(|| infos.first())
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    serde_json::json!({
        "configured": true,
        "available": raw.get("is_available"),
        "currency": pick.get("currency"),
        "total": pick.get("total_balance"),
        "granted": pick.get("granted_balance"),
        "toppedUp": pick.get("topped_up_balance"),
        "at": crate::store::now_ms(),
    })
}

/// 查余额。没有 Key 时返回 configured:false；出错把原因带回去，不报 500。
///
/// 错误也回 200 + error 字段：前端 UsageCard 就是按这个形状渲染
/// （报 500 会让整张卡变成“查询失败”，看不出是没配 Key 还是 Key 失效）。
fn fetch_balance(st: &Store) -> serde_json::Value {
    let key = st.api_key();
    if key.is_empty() {
        return serde_json::json!({ "configured": false });
    }
    if let Some(cached) = st.cached_balance() {
        return cached;
    }

    let result = match ureq::get("https://api.deepseek.com/user/balance")
        .set("Authorization", &format!("Bearer {key}"))
        .set("Accept", "application/json")
        .timeout(std::time::Duration::from_secs(8))
        .call()
    {
        Ok(resp) => match resp.into_json::<serde_json::Value>() {
            Ok(raw) => build_balance(&raw),
            Err(e) => {
                serde_json::json!({ "configured": true, "error": format!("响应解析失败: {e}") })
            }
        },
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            // 按字符截断，避免切到多字节中间导致 panic
            let brief: String = body.chars().take(300).collect();
            serde_json::json!({
                "configured": true,
                "error": format!("HTTP {code}: {brief}")
            })
        }
        Err(e) => serde_json::json!({ "configured": true, "error": format!("{e}") }),
    };
    st.cache_balance(Some(result.clone()));
    result
}
