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
    read_body_upto(req, MAX_BODY)
}

/// 读请求体，上限可指定（转发附件上传时要比会话大得多）。
fn read_body_upto(req: &mut Request, limit: u64) -> Result<Vec<u8>, String> {
    let len = req.body_length().unwrap_or(0) as u64;
    if len > limit {
        return Err("请求体过大".into());
    }
    let mut buf = Vec::with_capacity(len.min(1 << 20) as usize);
    req.as_reader()
        .take(limit + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("读取请求体失败: {e}"))?;
    if buf.len() as u64 > limit {
        return Err("请求体过大".into());
    }
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

/// UI 端口的请求处理。
///
/// `st` 取 `&'static Store`：部分端点要把存储交给后台线程（立即同步、归档），
/// 用 'static 引用能直接 move 进线程，不需要 unsafe transmute。
/// 这也符合事实：Store 建在 main 里，存活期就是整个进程。
pub fn handle(mut req: Request, st: &'static Store, www: &Path, version: &str) {
    let method = req.method().clone();
    let url = req.url().to_string();
    let raw_path = url.split('?').next().unwrap_or("/").to_string();

    // 前端把「数据层」接口挂在 /local/db/*（见 src/services/chat/db.ts 里的 BASE），
    // 而「本机设置」接口是 /local/*（见 src/local/api.ts）。Python 版在分发前
    // 统一剥掉 /local 前缀，这里对数据层做同样的事。
    //
    // 不做这一步的后果不是 404 那么无害：db.ts 对 404 刻意不抛错（用来表达
    // “这条会话不存在”），于是 404 的错误体被当成数组去 .map()，界面报的是
    // “对话列表加载失败”——排查时完全看不出根因在路由前缀上。
    //
    // 用 strip_prefix 而不是 starts_with + 切片：后者会被 clippy 判为
    // manual_strip。两个分支都不会 move raw_path（是 clone），无借用冲突。
    //
    // 注意要检查边界：`/local/dbfoo` 不该被改写成 `/dbfoo`。
    const DB_PREFIX: &str = "/local/db";
    let path = match raw_path.strip_prefix(DB_PREFIX) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("/db{rest}"),
        _ => raw_path.clone(),
    };

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
            /// 同步端口是否真的在监听。
            /// 界面拿它判断“本端地址”能不能对外宣称。
            #[serde(rename = "syncListening")]
            sync_listening: bool,
            /// 绑定失败的原因（没失败就是空串）。
            #[serde(rename = "syncErr")]
            sync_err: String,
            #[serde(rename = "uptimeSec")]
            uptime_secs: u64,
            platform: String,
            // 界面要拿这两个告诉用户「对端该填什么」和「本机叫什么」
            #[serde(rename = "localIps")]
            local_ips: Vec<String>,
            #[serde(rename = "deviceName")]
            device_name: String,
        }
        let cfg = st.config();
        // 报**实际**在监听的端口，而不是配置值。
        // 配置值只是“想要哪个”，端口被占时会退到别的端口；
        // 用配置值的话，界面会要用户把这个连不上的端口告诉对端。
        let bound = st.sync_bound();
        let sync_port = if bound != 0 {
            bound
        } else {
            cfg.get("syncPort")
                .and_then(|v| v.as_i64())
                .unwrap_or(st.port() as i64 + 1)
                .clamp(1024, 65535) as u16
        };
        let device_name = cfg
            .get("deviceName")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        send_json(
            req,
            200,
            &Status {
                version: version.to_string(),
                data_dir: st.dir().display().to_string(),
                port: st.port(),
                sync_port,
                sync_listening: bound != 0,
                sync_err: st.sync_bind_err(),
                uptime_secs: st.uptime_secs(),
                platform: std::env::consts::OS.to_string(),
                local_ips: crate::sync::local_ips(),
                device_name,
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
        let archived = aggregate::list_archived(st).len();
        let log_path = st.dir().join("app.log");
        send_json(
            req,
            200,
            &Storage {
                active_sessions: count,
                archived_sessions: archived,
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

    // ---------- /local/sync/status ----------
    if path == "/local/sync/status" && method == Method::Get {
        send_json(req, 200, &st.sync_state());
        return;
    }

    // ---------- /local/sync/now ----------
    if path == "/local/sync/now" && method == Method::Post {
        let peer = st
            .config()
            .get("peer")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if peer.is_empty() {
            send_error(req, 400, "还没填对端地址");
            return;
        }
        // st 是 &'static store，可以直接 move 进线程
        std::thread::spawn(move || crate::sync::pull_once(st));
        send_json(req, 200, &serde_json::json!({ "started": true }));
        return;
    }

    // ---------- /local/discover ----------
    if path == "/local/discover" && method == Method::Get {
        let peers = crate::sync::discover_peers(st, std::time::Duration::from_secs(2));
        let cfg = st.config();
        let name = cfg
            .get("deviceName")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let port = st.sync_port_advertised();
        send_json(
            req,
            200,
            &serde_json::json!({
                "peers": peers,
                "self": {
                    "name": name,
                    "ips": crate::sync::local_ips(),
                    "port": port,
                }
            }),
        );
        return;
    }

    // ---------- /local/pair ----------
    if path == "/local/pair" && method == Method::Post {
        let body = match read_body(&mut req) {
            Ok(b) => b,
            Err(e) => {
                send_error(req, 400, &e);
                return;
            }
        };
        #[derive(serde::Deserialize)]
        struct PairBody {
            #[serde(default)]
            ip: String,
            port: Option<u16>,
        }
        let parsed = serde_json::from_slice::<PairBody>(&body).unwrap_or(PairBody {
            ip: String::new(),
            port: None,
        });
        let ip = parsed.ip.trim().to_string();
        if ip.is_empty() {
            send_error(req, 400, "缺少 ip");
            return;
        }
        let port = parsed.port.unwrap_or(8791);
        let peer = format!("{ip}:{port}");
        let patch = serde_json::json!({ "peer": peer })
            .as_object()
            .cloned()
            .unwrap_or_default();
        let _ = st.save_config(&patch);

        // 反向：请对端也指向本机。对端只会入待确认队列，不直接生效。
        let mut reverse = "skipped".to_string();
        if let Some(mine) = crate::sync::local_ips().into_iter().next() {
            let my_port = st.sync_port_advertised();
            let my_name = st
                .config()
                .get("deviceName")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let payload = serde_json::json!({ "ip": mine, "port": my_port, "name": my_name });
            let url = format!("http://{ip}:{port}/peer");
            match ureq::post(&url)
                .timeout(std::time::Duration::from_secs(6))
                .send_json(payload)
            {
                Ok(_) => reverse = "pending".to_string(),
                Err(e) => reverse = format!("failed: {e}"),
            }
        }

        std::thread::spawn(move || crate::sync::pull_once(st));
        send_json(
            req,
            200,
            &serde_json::json!({ "peer": peer, "reverse": reverse }),
        );
        return;
    }

    // ---------- /local/pair/requests ----------
    if path == "/local/pair/requests" && method == Method::Get {
        send_json(req, 200, &st.pair_requests());
        return;
    }

    // ---------- /local/pair/accept ----------
    if path == "/local/pair/accept" && method == Method::Post {
        let body = match read_body(&mut req) {
            Ok(b) => b,
            Err(e) => {
                send_error(req, 400, &e);
                return;
            }
        };
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::json!({}));
        let ip = v
            .get("ip")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let port = v.get("port").and_then(|x| x.as_u64()).unwrap_or(8791) as u16;
        if ip.is_empty() {
            send_error(req, 400, "缺少 ip");
            return;
        }
        st.remove_pair_request(&ip, port);
        let peer = format!("{ip}:{port}");
        let patch = serde_json::json!({ "peer": peer })
            .as_object()
            .cloned()
            .unwrap_or_default();
        let _ = st.save_config(&patch);

        std::thread::spawn(move || crate::sync::pull_once(st));
        send_json(req, 200, &serde_json::json!({ "peer": peer }));
        return;
    }

    // ---------- /local/pair/reject ----------
    if path == "/local/pair/reject" && method == Method::Post {
        let body = match read_body(&mut req) {
            Ok(b) => b,
            Err(e) => {
                send_error(req, 400, &e);
                return;
            }
        };
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::json!({}));
        let ip = v
            .get("ip")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let port = v.get("port").and_then(|x| x.as_u64()).unwrap_or(8791) as u16;
        st.remove_pair_request(&ip, port);
        send_json(req, 200, &serde_json::json!({ "ok": true }));
        return;
    }

    // ---------- /local/archive/now ----------
    if path == "/local/archive/now" && method == Method::Post {
        // 与同步共用一把锁：两边都改 meta/index.json（一个写 archive，一个写 files）
        let _guard = st.lock_pull();
        // 用户点「立即整理」才去重：显式动作，不在后台自动删东西
        let (dropped, freed) = crate::archive::compact_archive(st).unwrap_or((0, 0));
        match crate::archive::archive_once(st) {
            Ok(n) => send_json(
                req,
                200,
                &serde_json::json!({
                    "archived": n,
                    "dedupDropped": dropped,
                    "dedupSavedBytes": freed,
                }),
            ),
            Err(e) => send_error(req, 500, &e),
        }
        return;
    }

    // ---------- /local/images/now ----------
    if path == "/local/images/now" && method == Method::Post {
        match crate::archive::compact_images(st) {
            Ok(r) => send_json(req, 200, &r),
            Err(e) => send_error(req, 500, &e),
        }
        return;
    }

    // ---------- /local/orphans ----------
    if path == "/local/orphans" && method == Method::Get {
        send_json(req, 200, &crate::archive::orphan_media(st));
        return;
    }

    // ---------- 反向代理：/models、/anthropic/*、/v1/* ----------
    //
    // 本地构建时前端**不直连上游**（`__API_ORIGIN__` 为空，见 rspack.config.ts）：
    // 聊天、附件上传、语音合成、Key 校验全部走同源请求，由这里转发到
    // api.deepseek.com 并注入存在服务端的 Key。
    //
    // 少了这一段，界面表现是「无法对话」：请求发到同源路径，服务端只认
    // /local/* 和 /db/*，于是回 404 {"error":"not found"}，前端把它当成
    // 上游错误，提示「API Key 无效或已被删除」——与真实原因毫无关系。
    if is_proxy_path(&path) {
        // Method 没有 Copy，用 PartialEq 判断，避免把 method 移走（后面还要用）
        let allowed = method == Method::Get
            || method == Method::Post
            || method == Method::Put
            || method == Method::Delete;
        if allowed {
            proxy(req, &url, st);
        } else {
            send_error(req, 404, "not found");
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

/// 前端在本地模式下会请求这些同源路径，由服务端转发到上游。
///
/// 与 Python 版的 `UIServer.dispatch` 保持一致：`/models`、`/anthropic/*`、`/v1/*`。
fn is_proxy_path(path: &str) -> bool {
    path == "/models"
        || path.starts_with("/models/")
        || path.starts_with("/anthropic/")
        || path.starts_with("/v1/")
}

/// 上游主机。余额、聊天、模型列表都是它。
const UPSTREAM_HOST: &str = "api.deepseek.com";
/// 前端 localStorage 里的占位值，表示「用服务端保存的 Key」。
const KEY_SENTINEL: &str = "__server__";

/// 把请求转发到上游，**流式**回传响应。
///
/// 必须流式：聊天走 SSE，如果先收完整个响应再返回，界面要等到回答结束
/// 才会出现文字，流式体验全没了。
fn proxy(mut req: Request, url: &str, st: &Store) {
    let method = req.method().clone();

    // 只挑需要的头：不盲目透传 Host/Content-Length/Expect 这些
    let mut in_key = String::new();
    let mut in_auth = String::new();
    let mut content_type = String::new();
    let mut accept = String::new();
    let mut anth_beta = String::new();
    for h in req.headers() {
        if h.field.equiv("x-api-key") {
            in_key = h.value.as_str().trim().to_string();
        } else if h.field.equiv("authorization") {
            in_auth = h.value.as_str().trim().to_string();
        } else if h.field.equiv("content-type") {
            content_type = h.value.as_str().to_string();
        } else if h.field.equiv("accept") {
            accept = h.value.as_str().to_string();
        } else if h.field.equiv("anthropic-beta") {
            anth_beta = h.value.as_str().to_string();
        }
    }

    // 凭据优先级：浏览器带来的真凭据 > 服务端保存的 Key。
    //
    // 哨兵值不算凭据。这一步很关键：verifyApiKey() 只带 Authorization、
    // 不带 x-api-key，若按「任一为空就用服务端 Key」判定，
    // 验证新 Key 时会被旧 Key 顶替，用户永远验证不过、也换不掉 Key。
    let sentinel_auth = format!("Bearer {KEY_SENTINEL}");
    let in_key = if in_key == KEY_SENTINEL {
        String::new()
    } else {
        in_key
    };
    let in_auth = if in_auth == sentinel_auth {
        String::new()
    } else {
        in_auth
    };

    let (out_key, out_auth) = if !in_key.is_empty() || !in_auth.is_empty() {
        (in_key, in_auth)
    } else {
        let server_key = st.api_key();
        if server_key.is_empty() {
            // 把 "__server__" 发给上游会得到一个语焉不详的 401（“Key 无效”），
            // 让人以为 Key 失效了，实际是没配置。
            send_error(
                req,
                400,
                "服务端尚未配置 API Key：请先在设置里填入（保存在 data/meta/secret.json）",
            );
            return;
        }
        let auth = format!("Bearer {server_key}");
        (server_key, auth)
    };

    // 上传附件（/anthropic/v1/files）比会话大得多，用附件上限
    let body = if method == Method::Get {
        Vec::new()
    } else {
        match read_body_upto(&mut req, 128 << 20) {
            Ok(b) => b,
            Err(e) => {
                send_error(req, 413, &e);
                return;
            }
        }
    };

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(10))
        // 读超时按“多久没收到数据”算，聊天流只要还在出字就不会被切断
        .timeout_read(std::time::Duration::from_secs(600))
        .timeout_write(std::time::Duration::from_secs(60))
        // 跟 Python 版一致：读 HTTP_PROXY/HTTPS_PROXY/ALL_PROXY。
        // 用系统代理的用户，不读这两个变量的话所有请求都直连——
        // 表现就是“浏览器能用、客户端不能用”。
        .try_proxy_from_env(true)
        // 不跟重定向：上游若有跳转，交给前端看到真实状态码，避免静默改路由
        .redirects(0)
        .build();

    let target = format!("https://{UPSTREAM_HOST}{url}");
    let mut call = agent.request(method.as_str(), &target);
    if !out_key.is_empty() {
        call = call.set("x-api-key", &out_key);
    }
    if !out_auth.is_empty() {
        call = call.set("Authorization", &out_auth);
    }
    if !content_type.is_empty() {
        call = call.set("Content-Type", &content_type);
    }
    if !accept.is_empty() {
        call = call.set("Accept", &accept);
    }
    if !anth_beta.is_empty() {
        call = call.set("anthropic-beta", &anth_beta);
    }

    let sent = if method == Method::Get {
        call.call()
    } else {
        call.send_bytes(&body)
    };

    let resp = match sent {
        Ok(r) => r,
        // 上游 4xx/5xx 也是有效响应：状态码与响应体必须原样交给前端，
        // 否则界面只会看到一句“网络错误”，看不到上游真正说了什么。
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => {
            send_error(req, 502, &format!("转发到 {UPSTREAM_HOST} 失败: {e}"));
            return;
        }
    };

    let status = resp.status();
    let mut headers: Vec<Header> = Vec::new();
    for name in resp.headers_names() {
        let lower = name.to_ascii_lowercase();
        // 这三个由 tiny_http 自己管：长度未知时它会用 chunked，
        // 同时带上去会让客户端解析错乱。
        if matches!(
            lower.as_str(),
            "transfer-encoding" | "connection" | "content-length"
        ) {
            continue;
        }
        if let Some(v) = resp.header(&name) {
            if let Ok(h) = Header::from_bytes(name.as_bytes(), v.as_bytes()) {
                headers.push(h);
            }
        }
    }

    // data_length = None：长度未知，tiny_http 用 chunked 边收边发。
    // 这就是 SSE 能逐块到达界面的原因。
    let reader = resp.into_reader();
    let _ = req.respond(Response::new(
        StatusCode(status),
        headers,
        reader,
        None,
        None,
    ));
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
///
/// **只缓存成功结果。** Python 版就是这么做的，这里一度把失败也缓存 60 秒，
/// 后果是：用户换好 Key、修好网络后，界面仍然要等一分钟才恢复。
fn fetch_balance(st: &Store) -> serde_json::Value {
    let key = st.api_key();
    if key.is_empty() {
        return serde_json::json!({ "configured": false });
    }
    if let Some(cached) = st.cached_balance() {
        return cached;
    }

    const URL: &str = "https://api.deepseek.com/user/balance";
    let result = match ureq::get(URL)
        .set("Authorization", &format!("Bearer {key}"))
        .set("Accept", "application/json")
        .timeout(std::time::Duration::from_secs(8))
        .call()
    {
        Ok(resp) => match resp.into_json::<serde_json::Value>() {
            Ok(raw) => build_balance(&raw),
            Err(e) => balance_error(format!("响应解析失败: {e}")),
        },
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            // 按字符截断，避免切到多字节中间导致 panic
            let brief: String = body.chars().take(300).collect();
            let hint = match code {
                401 | 403 => "（Key 无效、已删或类型不对）",
                // 余额接口没有区域/版本分支；404 基本只说明请求没到 DeepSeek 官方，
                // 而是被中转/代理/本地 hosts 接走了。
                404 => {
                    "（上游不存在该接口：余额固定走 api.deepseek.com/user/balance，\
                        中转或代理地址不支持查余额）"
                }
                _ => "",
            };
            balance_error(format!("HTTP {code}{hint}：{brief}"))
        }
        Err(e) => balance_error(format!("连不上 {URL}：{e}")),
    };

    // 失败只记日志、不入缓存：下一轮请求会重新尝试，
    // 否则用户改完 Key 或网络还得等一分钟才看到正确结果。
    if result.get("error").is_some() {
        let msg = result
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        st.note_balance_err(&msg);
    } else {
        st.cache_balance(Some(result.clone()));
        st.clear_balance_err();
    }
    result
}

fn balance_error(detail: String) -> serde_json::Value {
    serde_json::json!({ "configured": true, "error": detail })
}

// ------------------------------------------------------------------ 同步端口

/// 同步端口的请求处理。
///
/// 这里专处理「对端拉数据」与「配对请求」，**不暴露** UI 端口的那些能力：
///   - 对端只知道我们的同步端口，不知道 UI 端口
///   - 局域网内无鉴权，所以这里只给读同步数据 + 入队配对请求，
///     改配置仍然要本机用户在界面上确认
pub fn handle_sync(mut req: Request, st: &'static Store, version: &str) {
    let method = req.method().clone();
    let url = req.url().to_string();
    let path = url.split('?').next().unwrap_or("/").to_string();

    if path == "/health" && method == Method::Get {
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

    if path == "/sync/since" && method == Method::Get {
        let since = query_param(&url, "since")
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0);

        let mut sessions: Vec<serde_json::Value> = Vec::new();
        for meta in st.summaries().unwrap_or_default() {
            if meta.updated_at <= since {
                continue;
            }
            if let Ok(Some(raw)) = st.load_session(&meta.id) {
                if let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&raw) {
                    sessions.push(doc);
                }
            }
        }
        // 墓碑也要发，否则对端不知道“这条被删了”
        for meta in st.deleted_summaries().unwrap_or_default() {
            if meta.updated_at <= since {
                continue;
            }
            let mut doc = match st.load_session(&meta.id) {
                Ok(Some(raw)) => serde_json::from_slice::<serde_json::Value>(&raw)
                    .unwrap_or_else(|_| serde_json::json!({})),
                _ => serde_json::json!({
                    "id": meta.id,
                    "title": meta.title,
                    "createdAt": meta.created_at,
                    "messages": [],
                }),
            };
            if let Some(obj) = doc.as_object_mut() {
                obj.insert("deleted".to_string(), serde_json::Value::Bool(true));
                obj.insert(
                    "updatedAt".to_string(),
                    serde_json::Value::from(meta.updated_at),
                );
            }
            sessions.push(doc);
        }

        let files: Vec<serde_json::Value> = st
            .file_metas()
            .into_iter()
            .filter(|f| f.created_at > since)
            .filter_map(|f| serde_json::to_value(f).ok())
            .collect();

        send_json(
            req,
            200,
            &serde_json::json!({ "now": crate::store::now_ms(), "sessions": sessions, "files": files }),
        );
        return;
    }

    if path == "/sync/merge" && method == Method::Post {
        let body = match read_body(&mut req) {
            Ok(b) => b,
            Err(e) => {
                send_error(req, 400, &e);
                return;
            }
        };
        let payload: serde_json::Value =
            serde_json::from_slice(&body).unwrap_or(serde_json::json!({}));
        let mut accepted = 0i64;
        if let Some(arr) = payload.get("sessions").and_then(|v| v.as_array()) {
            // 与 pull_once 共用一把锁：两边都在改会话文件，
            // 不串行就会出现「刚合并完的又被对方写回去」。
            let _guard = st.lock_pull();
            for doc in arr {
                if let Ok(true) = crate::sync::merge_remote(st, doc) {
                    accepted += 1;
                }
            }
        }
        send_json(
            req,
            200,
            &serde_json::json!({ "accepted": accepted, "missing": [] }),
        );
        return;
    }

    // 配对请求：只入待确认队列，**不直接改配置**。
    if (path == "/peer" || path == "/local/peer") && method == Method::Post {
        let body = match read_body(&mut req) {
            Ok(b) => b,
            Err(e) => {
                send_error(req, 400, &e);
                return;
            }
        };
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::json!({}));
        let ip = v
            .get("ip")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if ip.is_empty() {
            send_error(req, 400, "缺少 ip");
            return;
        }
        let port = v.get("port").and_then(|x| x.as_u64()).unwrap_or(8791) as u16;
        let name: String = v
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or(&ip)
            .chars()
            .take(64)
            .collect();
        st.add_pair_request(crate::sync::PairRequest {
            ip,
            port,
            name,
            at: crate::store::now_ms(),
        });
        send_json(req, 200, &serde_json::json!({ "pending": true }));
        return;
    }

    // 按 sha 取原件（对端拉附件用）
    if let Some(sha) = path.strip_prefix("/file/") {
        if method != Method::Get || sha.is_empty() {
            send_error(req, 404, "not found");
            return;
        }
        // 只接受 64 位十六进制，避免路径穿越（对端传来的 sha 不可信）
        if sha.len() != 64 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            send_error(req, 400, "非法 sha");
            return;
        }
        match std::fs::read(st.dir().join("media").join(sha)) {
            Ok(data) => {
                let _ = req.respond(
                    Response::from_data(data)
                        .with_status_code(StatusCode(200))
                        .with_header(header(b"Content-Type", b"application/octet-stream"))
                        .with_header(header(
                            b"Cache-Control",
                            b"public, max-age=31536000, immutable",
                        )),
                );
            }
            Err(_) => send_error(req, 404, "not found"),
        }
        return;
    }

    send_error(req, 404, "not found");
}
