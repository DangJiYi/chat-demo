//! HTTP 层：路由分发、JSON 响应、静态文件。
//!
//! 分工：真正的数据逻辑在 store.rs，这里只做「解析请求 → 调用 → 写响应」。

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use tiny_http::{Header, Method, Request, Response, StatusCode};

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
