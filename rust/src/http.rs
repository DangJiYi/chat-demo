//! HTTP 层：路由分发 + JSON 响应。
//!
//! 刻意保持极薄——真正的逻辑在 store.rs。

use serde::Serialize;
use tiny_http::{Header, Method, Request, Response, StatusCode};

use crate::store::Store;

#[derive(Serialize)]
struct Status {
    version: String,
    #[serde(rename = "dataDir")]
    data_dir: String,
    port: u16,
    #[serde(rename = "syncPort")]
    sync_port: u16,
    #[serde(rename = "uptimeSec")]
    uptime_sec: u64,
    platform: String,
}

fn json_header() -> Header {
    Header::from_bytes(&b"Content-Type"[..], &b"application/json; charset=utf-8"[..])
        .expect("static header")
}

fn no_store_header() -> Header {
    Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]).expect("static header")
}

/// 发一个 JSON 响应。序列化失败时退化成 500 + 纯文本错误。
pub fn send_json<T: Serialize>(req: Request, code: u16, value: &T) {
    match serde_json::to_vec(value) {
        Ok(body) => {
            let resp = Response::from_data(body)
                .with_status_code(StatusCode(code))
                .with_header(json_header())
                .with_header(no_store_header());
            let _ = req.respond(resp);
        }
        Err(e) => {
            let _ = req.respond(
                Response::from_string(format!("{{\"error\":\"序列化失败: {e}\"}}"))
                    .with_status_code(StatusCode(500))
                    .with_header(json_header()),
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

pub fn handle(req: Request, st: &Store, version: &str) {
    let method = req.method().clone();
    let url = req.url().to_string();
    // 只取路径，丢掉 query（骨架阶段用不到）
    let path = url.split('?').next().unwrap_or("/").to_string();

    match (method, path.as_str()) {
        (Method::Get, "/local/status") => {
            let body = Status {
                version: version.to_string(),
                data_dir: st.dir().display().to_string(),
                port: st.port(),
                sync_port: 8791,
                uptime_sec: st.uptime_secs(),
                platform: std::env::consts::OS.to_string(),
            };
            send_json(req, 200, &body);
        }
        (Method::Get, "/db/summaries") => match st.summaries() {
            Ok(list) => send_json(req, 200, &list),
            Err(e) => send_error(req, 500, &format!("读取会话失败: {e}")),
        },
        (Method::Get, "/health") => {
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
        }
        _ => send_error(req, 404, "not found"),
    }
}
