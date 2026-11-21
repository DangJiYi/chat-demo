//! 端到端冒烟测试：启动真实二进制，用 HTTP 请求验证契约。
//!
//! 把这些断言放在 Rust 里而不是 CI 的 YAML 里，有两个原因：
//!   1. 断言和实现同仓同语言，改接口时 cargo test 立刻报错
//!   2. YAML 里的 PowerShell 断言容易写错类型（byte[] vs string、@() 包数组），
//!      而且只有推上去跑 CI 才知道对不对
//!
//! 每个测试起一个独立实例（独立端口 + 独立临时目录），互不影响。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// 每个测试实例的目录号。仅靠时间戳不够（Windows 时钟精度粗），见 try_start。
static DIR_SEQ: AtomicU64 = AtomicU64::new(0);

/// 一个跑起来的测试实例。Drop 时杀掉进程并清理目录。
struct TestServer {
    child: Child,
    port: u16,
    dir: PathBuf,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl TestServer {
    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// 数据目录。
    fn data(&self) -> PathBuf {
        self.dir.join("data")
    }
}

/// 取一个空闲端口：绑定 0 让系统分配，然后立刻释放。
/// 有极小的竞态窗口，但比硬编码端口可靠得多。
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr").port()
}

/// 启动实例。`www` 为 Some 时建一个最小静态站。
fn start(with_www: bool) -> TestServer {
    for attempt in 0..3u32 {
        if let Some(s) = try_start(with_www, attempt) {
            return s;
        }
    }
    panic!("服务三次都未在 10 秒内就绪");
}

fn try_start(with_www: bool, attempt: u32) -> Option<TestServer> {
    // 目录名必须真正唯一。
    //
    // 只用 pid + 纳秒是不行的：Windows 上 SystemTime 的精度很粗（历史上
    // 有 15.6ms 的刻度），而测试是并行跑的，两个测试可能拿到同一个时间戳，
    // 于是**共用同一个 data 目录**：两个服务器同时写 meta/secret.json，
    // fs::rename 撞上 Access Denied，PUT /local/apikey 就返回 500。
    // 这种失败只在并行、只在 Windows 上偶发，看起来像业务 bug。
    // 加一个进程内原子序号就彻底断了这条路。
    let seq = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "dsh-rust-test-{}-{}-{seq}-{attempt}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(dir.join("data/sessions")).expect("mkdir data");
    std::fs::create_dir_all(dir.join("www/static/js")).expect("mkdir www");
    if with_www {
        std::fs::write(
            dir.join("www/index.html"),
            b"<!doctype html><div id=root></div>",
        )
        .expect("write index");
        std::fs::write(dir.join("www/static/js/app.js"), b"console.log(1)")
            .expect("write js");
    }

    let port = free_port();
    let child = Command::new(env!("CARGO_BIN_EXE_dsh-chat"))
        .arg("--port")
        .arg(port.to_string())
        .arg("--data")
        .arg(dir.join("data"))
        .arg("--www")
        .arg(dir.join("www"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn dsh-chat");

    let server = TestServer { child, port, dir };

    // 等就绪，并确认这个端口上跑的**确实是本测试刚起的那个进程**。
    //
    // free_port 先绑后放，中间有窗口：两个测试可能拿到同一个端口，
    // 先起的那个占着，后起的那个直接退出（监听失败 → process::exit(1)）。
    // 这时后一个测试的请求全部会打到前一个测试的服务器上——
    // 数据目录不同、状态互相污染，报出来的错跟真实原因毫无关系。
    // 比对 dataDir 就能一眼识破，然后换个端口重来。
    let want_dir = server.data().display().to_string();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(resp) = ureq::get(&server.url("/local/status")).call() {
            if resp.status() == 200 {
                if let Ok(st) = resp.into_json::<serde_json::Value>() {
                    if st["dataDir"].as_str() == Some(want_dir.as_str()) {
                        return Some(server);
                    }
                    eprintln!("端口 {port} 上是别的实例（{}），换个端口重试", st["dataDir"]);
                    return None; // Drop 会杀掉子进程并清目录
                }
                return Some(server);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!("服务未在 10 秒内就绪（端口 {port}）");
    None // Drop 负责收尾
}

/// 断言目录里出现某个文件。
fn assert_file(dir: &Path, rel: &str) {
    let p = dir.join(rel);
    assert!(p.is_file(), "期望文件存在: {}", p.display());
}

/// 取出 HTTP 状态码；网络错误返回 0。
fn status(result: Result<ureq::Response, ureq::Error>) -> u16 {
    match result {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(_) => 0,
    }
}

/// 写一个会话文件。
fn write_session(dir: &Path, id: &str, title: &str, updated_at: i64) {
    let doc = format!(
        r#"{{"id":"{id}","title":"{title}","createdAt":1000,"updatedAt":{updated_at},"messages":[]}}"#
    );
    let mut f = std::fs::File::create(dir.join(format!("sessions/{id}.json"))).expect("create");
    f.write_all(doc.as_bytes()).expect("write");
}

// ------------------------------------------------------------------ 基础

#[test]
fn status_reports_version_and_dirs() {
    let s = start(true);
    let resp = ureq::get(&s.url("/local/status")).call().expect("status");
    let body: serde_json::Value = resp.into_json().expect("json");
    assert_eq!(body["version"], "0.2.1");
    assert_eq!(body["port"], s.port);
    assert!(body["dataDir"].as_str().unwrap().contains("data"));
    assert!(body["platform"].is_string());

    // 下面四个字段是界面「本机」区块的全部数据源。
    // 少了任何一个，界面就只剩“未获取到（没连局域网？）”——
    // 而那可能是服务端压根没返回这个字段，不是网络问题。
    assert!(
        body["localIps"].is_array(),
        "localIps 必须是数组（界面拿 [0] 拼本端地址）: {body}"
    );
    let name = body["deviceName"].as_str();
    assert!(
        name.map(|n| !n.is_empty()).unwrap_or(false),
        "deviceName 必须是非空字符串: {body}"
    );
    let sync_port = body["syncPort"].as_u64().expect("syncPort 必须是数字");
    assert!(
        (1024..=65535).contains(&sync_port),
        "syncPort 越界: {sync_port}"
    );
    // 服务启动后同步端口就该在监听；没监听说明绑定失败，
    // 界面会把一个连不上的地址告诉对端。
    assert!(
        body["syncListening"].as_bool().unwrap_or(false),
        "同步端口未监听: {body}"
    );
}

#[test]
fn summaries_sorted_desc_and_camel_case() {
    let s = start(true);
    write_session(&s.data(), "s1", "会话一", 2000);
    write_session(&s.data(), "s2", "会话二", 3000);

    let resp = ureq::get(&s.url("/db/summaries")).call().expect("summaries");
    // 必须直接是数组（前端按数组消费）
    let text = resp.into_string().expect("text");
    assert!(text.trim_start().starts_with('['), "应返回数组，实际: {text}");
    let list: Vec<serde_json::Value> = serde_json::from_str(&text).expect("parse");

    assert_eq!(list.len(), 2);
    assert_eq!(list[0]["id"], "s2", "应按 updatedAt 倒序，s2 在前");
    assert_eq!(list[1]["id"], "s1");
    assert_eq!(list[0]["title"], "会话二", "UTF-8 标题应完好");
    // 字段名必须是 camelCase——写成 created_at 前端会静默读到 undefined
    assert_eq!(list[0]["updatedAt"], 3000);
    assert_eq!(list[0]["createdAt"], 1000);
    assert!(list[0].get("updated_at").is_none(), "不应出现 snake_case 字段");
}

// ------------------------------------------------------------------ 静态文件

#[test]
fn static_serves_index_and_assets() {
    let s = start(true);

    let idx = ureq::get(&s.url("/")).call().expect("index");
    assert_eq!(idx.status(), 200);
    assert!(idx.header("Content-Type").unwrap_or("").contains("text/html"));
    assert!(idx.into_string().unwrap().contains("id=root"));

    let js = ureq::get(&s.url("/static/js/app.js")).call().expect("js");
    assert_eq!(js.status(), 200);
    assert!(
        js.header("Content-Type").unwrap_or("").contains("javascript"),
        "js 的 Content-Type 不对: {:?}",
        js.header("Content-Type")
    );
    assert!(
        js.header("Cache-Control").unwrap_or("").contains("immutable"),
        "static/ 下应长缓存，实际 {:?}",
        js.header("Cache-Control")
    );
}

#[test]
fn spa_fallback_but_404_for_missing_asset() {
    let s = start(true);

    // 无扩展名的未知路径 → 回退 index.html（前端路由需要）
    let fb = ureq::get(&s.url("/some/route")).call().expect("fallback");
    assert_eq!(fb.status(), 200);
    assert!(fb.into_string().unwrap().contains("id=root"));

    // 带扩展名却找不到 → 404（不能兜底成首页，否则前端会拿到 HTML 当 JS 用）
    assert_eq!(status(ureq::get(&s.url("/nope.js")).call()), 404);
}

#[test]
fn directory_traversal_rejected() {
    let s = start(true);
    // 用编码形式，避免客户端先规范化掉 ..
    let code = status(ureq::get(&s.url("/%2e%2e%2fCargo.toml")).call());
    assert_ne!(code, 200, "目录穿越必须被拒绝");
}

// ------------------------------------------------------------------ 会话

#[test]
fn session_crud_and_tombstone() {
    let s = start(true);
    write_session(&s.data(), "s1", "原有", 2000);

    // 读
    let one: serde_json::Value = ureq::get(&s.url("/db/session/s1"))
        .call()
        .expect("get")
        .into_json()
        .unwrap();
    assert_eq!(one["id"], "s1");

    // 写
    let doc = r#"{"id":"s9","title":"新的","createdAt":5000,"updatedAt":5000,"messages":[]}"#;
    let code = status(
        ureq::put(&s.url("/db/session/s9"))
            .set("Content-Type", "application/json")
            .send_string(doc),
    );
    assert_eq!(code, 204, "PUT 应返回 204");
    assert_file(&s.data(), "sessions/s9.json");

    // id 与路径不一致 → 500，且错误里说明原因
    let bad = ureq::put(&s.url("/db/session/wrong"))
        .set("Content-Type", "application/json")
        .send_string(doc);
    match bad {
        Err(ureq::Error::Status(code, resp)) => {
            assert_eq!(code, 500);
            let body = resp.into_string().unwrap();
            assert!(body.contains("id 不一致"), "错误应说明原因，实际: {body}");
        }
        _ => panic!("id 不一致应被拒绝"),
    }

    // 改名
    let code = status(
        ureq::post(&s.url("/db/session/s9/rename"))
            .set("Content-Type", "application/json")
            .send_string(r#"{"title":"改名了"}"#),
    );
    assert_eq!(code, 204);
    let after: serde_json::Value = ureq::get(&s.url("/db/session/s9"))
        .call()
        .unwrap()
        .into_json()
        .unwrap();
    assert_eq!(after["title"], "改名了");

    // 删除是写墓碑，不删文件
    let code = status(ureq::delete(&s.url("/db/session/s9")).call());
    assert_eq!(code, 204);
    assert_file(&s.data(), "sessions/s9.json");
    let tomb: serde_json::Value =
        serde_json::from_slice(&std::fs::read(s.data().join("sessions/s9.json")).unwrap()).unwrap();
    assert!(
        tomb["deleted"].as_bool().unwrap_or(false),
        "应写入墓碑标记"
    );

    // 墓碑不出现在列表里
    let list: Vec<serde_json::Value> = ureq::get(&s.url("/db/summaries"))
        .call()
        .unwrap()
        .into_json()
        .unwrap();
    assert!(list.iter().all(|x| x["id"] != "s9"), "墓碑不应出现在摘要列表");
}

// ------------------------------------------------------------------ 附件

#[test]
fn file_content_addressed_and_mime_preserved() {
    let s = start(true);
    let payload = b"hello-attachment";

    let code = status(
        ureq::put(&s.url("/db/file/att1"))
            .set("Content-Type", "text/plain")
            .set("X-File-Name", "note.txt")
            .send_bytes(payload),
    );
    assert_eq!(code, 204);

    let resp = ureq::get(&s.url("/db/file/att1")).call().expect("get file");
    assert_eq!(resp.status(), 200);
    assert!(resp.header("Content-Type").unwrap_or("").contains("text/plain"));

    // 原件按 sha256 命名的
    let digest = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(payload);
        format!("{:x}", h.finalize())
    };
    assert_file(&s.data(), &format!("media/{digest}"));

    // index.json 里要有 files.att1.sha
    let idx: serde_json::Value =
        serde_json::from_slice(&std::fs::read(s.data().join("meta/index.json")).unwrap()).unwrap();
    assert_eq!(idx["files"]["att1"]["sha"], digest);

    assert_eq!(status(ureq::get(&s.url("/db/file/nope")).call()), 404);
}

#[test]
fn staging_commit_replaces_sessions() {
    let s = start(true);
    write_session(&s.data(), "old1", "旧的", 1000);

    assert_eq!(
        status(ureq::post(&s.url("/db/staging/clear")).send_string("")),
        204
    );

    let doc = r#"{"id":"imp1","title":"导入的","createdAt":7000,"updatedAt":7000,"messages":[{"id":"m1","role":"user","content":"hi","files":[{"id":"sf1","mimeType":"image/png"}]}]}"#;
    assert_eq!(
        status(
            ureq::put(&s.url("/db/staging/session/imp1"))
                .set("Content-Type", "application/json")
                .send_string(doc)
        ),
        204
    );
    assert_eq!(
        status(
            ureq::put(&s.url("/db/staging/file/sf1"))
                .set("Content-Type", "image/png")
                .send_bytes(b"staged-file")
        ),
        204
    );

    let resp = ureq::post(&s.url("/db/staging/commit"))
        .send_string("")
        .expect("commit");
    let list: Vec<serde_json::Value> = resp.into_json().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], "imp1");

    // commit 后：新会话到位、旧会话被替换、暂存清空
    assert_file(&s.data(), "sessions/imp1.json");
    assert!(
        !s.data().join("sessions/old1.json").exists(),
        "commit 应替换旧会话"
    );
    let staged = std::fs::read_dir(s.data().join("staged/sessions"))
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(staged, 0, "暂存会话应清空");

    // 导入的附件能读回，且 MIME 保留（否则“压缩图片”认不出它是图片）
    let f = ureq::get(&s.url("/db/file/sf1")).call().expect("staged file");
    assert!(f.header("Content-Type").unwrap_or("").contains("image/png"));
}

// ------------------------------------------------------------------ 配置

#[test]
fn config_defaults_and_filtering() {
    let s = start(true);
    let cfg: serde_json::Value = ureq::get(&s.url("/local/config"))
        .call()
        .unwrap()
        .into_json()
        .unwrap();
    assert_eq!(cfg["intervalSec"], 3, "默认 intervalSec");
    assert_eq!(cfg["archiveDays"], 7, "默认 archiveDays");
    assert!(!cfg["deviceName"].as_str().unwrap().is_empty());

    let updated: serde_json::Value = ureq::put(&s.url("/local/config"))
        .set("Content-Type", "application/json")
        .send_string(r#"{"intervalSec":5,"deviceName":"测试机"}"#)
        .unwrap()
        .into_json()
        .unwrap();
    assert_eq!(updated["intervalSec"], 5);
    assert_eq!(updated["deviceName"], "测试机");

    // 未知字段不该被持久化（与 Python 版 save_config 的过滤行为一致）
    let _ = ureq::put(&s.url("/local/config"))
        .set("Content-Type", "application/json")
        .send_string(r#"{"bogusKey":"x"}"#);
    let after: serde_json::Value = ureq::get(&s.url("/local/config"))
        .call()
        .unwrap()
        .into_json()
        .unwrap();
    assert!(after.get("bogusKey").is_none(), "未知字段不应落盘");
}

#[test]
fn apikey_lifecycle_and_hint() {
    let s = start(true);
    let st: serde_json::Value = ureq::get(&s.url("/local/apikey"))
        .call()
        .unwrap()
        .into_json()
        .unwrap();
    assert!(
        !st["configured"].as_bool().unwrap_or(true),
        "初始不应有 Key"
    );

    // 用一个明显不是真凭据的字符串，只验证格式与存储位置。
    // 不直接 .unwrap()：非 2xx 时 ureq 报的是 Status(500, ...)，
    // 看不到服务端写了什么原因，排查时等于没有信息。
    let put = ureq::put(&s.url("/local/apikey"))
        .set("Content-Type", "application/json")
        .send_string(r#"{"apiKey":"SYNTHETIC-0000"}"#);
    let resp: serde_json::Value = match put {
        Ok(r) => r.into_json().expect("json"),
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            panic!("PUT /local/apikey 返回 {code}：{body}");
        }
        Err(e) => panic!("PUT /local/apikey 失败：{e}"),
    };
    assert!(resp["configured"].as_bool().unwrap_or(false), "存 Key 后应 configured=true");
    assert_eq!(resp["hint"], "…0000", "hint 只露末 4 位");
    assert_file(&s.data(), "meta/secret.json");

    // 空 Key 应 400
    assert_eq!(
        status(
            ureq::put(&s.url("/local/apikey"))
                .set("Content-Type", "application/json")
                .send_string(r#"{"apiKey":"   "}"#)
        ),
        400
    );

    assert_eq!(status(ureq::delete(&s.url("/local/apikey")).call()), 204);
    let st2: serde_json::Value = ureq::get(&s.url("/local/apikey"))
        .call()
        .unwrap()
        .into_json()
        .unwrap();
    assert!(
        !st2["configured"].as_bool().unwrap_or(true),
        "删除后应回到未配置"
    );
}

// ------------------------------------------------------------------ 聚合

#[test]
fn usage_has_all_frontend_fields() {
    let s = start(true);
    let u: serde_json::Value = ureq::get(&s.url("/local/usage"))
        .call()
        .unwrap()
        .into_json()
        .unwrap();
    assert_eq!(u["currency"], "CNY");
    assert!(u["now"]["until"].is_i64(), "now.until 缺失");
    assert!(u["now"]["peak"].is_boolean(), "now.peak 缺失");
    for key in [
        "calls",
        "hitTokens",
        "missTokens",
        "outTokens",
        "cost",
        "cacheSaved",
        "offPeakSaving",
    ] {
        assert!(!u["today"][key].is_null(), "usage.today.{key} 缺失");
    }
}

#[test]
fn search_finds_message_text() {
    let s = start(true);
    let doc = r#"{"id":"q1","title":"搜索","createdAt":1,"updatedAt":2,"messages":[{"id":"m1","role":"user","content":"今天天气怎么样","files":[]}]}"#;
    std::fs::write(s.data().join("sessions/q1.json"), doc).unwrap();

    let r: serde_json::Value = ureq::get(&s.url("/local/search?q=%E5%A4%A9%E6%B0%94"))
        .call()
        .unwrap()
        .into_json()
        .unwrap();
    assert_eq!(r["query"], "天气", "query 应回显解码后的值");
    let hits = r["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "应命中 1 条");
    assert_eq!(hits[0]["sessionId"], "q1");
    assert!(hits[0]["offset"].is_number(), "offset 应为数字");

    let empty: serde_json::Value = ureq::get(&s.url("/local/search?q="))
        .call()
        .unwrap()
        .into_json()
        .unwrap();
    assert_eq!(empty["hits"].as_array().unwrap().len(), 0);
}

#[test]
fn archived_empty_then_restore() {
    let s = start(true);

    let arch: Vec<serde_json::Value> = ureq::get(&s.url("/local/archived"))
        .call()
        .unwrap()
        .into_json()
        .unwrap();
    assert!(arch.is_empty(), "无归档时应是空数组");

    // 造一个归档条目：gzip 的 jsonl + index.json 里的 archive 数组
    let doc = r#"{"id":"arch1","title":"归档的","createdAt":100,"updatedAt":200,"messages":[]}"#;
    let line = format!(r#"{{"archivedAt":300,"session":{doc}}}"#);
    {
        let file = std::fs::File::create(s.data().join("archive/archive_2026-11.jsonl.gz")).unwrap();
        let mut gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        gz.write_all(line.as_bytes()).unwrap();
        gz.finish().unwrap();
    }
    let idx = serde_json::json!({
        "sessions": {},
        "files": {},
        "archive": [{
            "id": "arch1", "month": "2026-11", "title": "归档的",
            "updatedAt": 200, "msgCount": 0
        }],
        "archiveDeleted": []
    });
    std::fs::write(
        s.data().join("meta/index.json"),
        serde_json::to_vec(&idx).unwrap(),
    )
    .unwrap();

    let arch2: Vec<serde_json::Value> = ureq::get(&s.url("/local/archived"))
        .call()
        .unwrap()
        .into_json()
        .unwrap();
    assert_eq!(arch2.len(), 1);
    assert_eq!(arch2[0]["id"], "arch1");

    assert_eq!(
        status(ureq::post(&s.url("/local/restore/arch1")).send_string("")),
        200
    );
    assert_file(&s.data(), "sessions/arch1.json");
    let arch3: Vec<serde_json::Value> = ureq::get(&s.url("/local/archived"))
        .call()
        .unwrap()
        .into_json()
        .unwrap();
    assert!(arch3.is_empty(), "恢复后归档列表应清空");

    // 恢复不存在的 id → 404
    assert_eq!(
        status(ureq::post(&s.url("/local/restore/nope")).send_string("")),
        404
    );
}

#[test]
fn unknown_path_falls_back_to_spa() {
    let s = start(true);
    // 未知的无扩展名路径会回退到 index.html——这是 SPA 的预期行为，不是 404。
    // 真正该 404 的是「带扩展名却不存在」，那条在 spa_fallback_but_404_for_missing_asset 里验。
    let resp = ureq::get(&s.url("/local/nonexistent")).call().expect("fallback");
    assert_eq!(resp.status(), 200);
    assert!(resp.into_string().unwrap().contains("id=root"));
}

/// 前端真实使用的路径是 `/local/db/*`（见 `src/services/chat/db.ts` 的 `BASE`），
/// 不是 `/db/*`。
///
/// 之前所有断言都直接打 `/db/*`，所以「数据层整个 404」这种致命问题
/// 一路绿着进了产物：界面上表现为「对话列表加载失败」，看不出是前缀问题。
/// 这组断言就盯住这条契约。
#[test]
fn frontend_local_db_prefix_is_served() {
    let s = start(true);
    write_session(&s.data(), "p1", "本地会话", 2000);

    // 列表
    let raw = ureq::get(&s.url("/local/db/summaries"))
        .call()
        .expect("GET /local/db/summaries")
        .into_string()
        .expect("text");
    assert!(
        raw.trim_start().starts_with('['),
        "应返回数组，实际: {raw}"
    );
    let list: Vec<serde_json::Value> = serde_json::from_str(&raw).expect("parse");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], "p1");

    // 读单条
    let one = ureq::get(&s.url("/local/db/session/p1"))
        .call()
        .expect("GET /local/db/session/p1")
        .into_json::<serde_json::Value>()
        .expect("json");
    assert_eq!(one["title"], "本地会话");

    // 写单条
    let doc = r#"{"id":"p2","title":"新建","createdAt":3000,"updatedAt":3000,"messages":[]}"#;
    let resp = ureq::put(&s.url("/local/db/session/p2"))
        .send_string(doc)
        .expect("PUT");
    assert_eq!(resp.status(), 204);
    assert_file(&s.data(), "sessions/p2.json");

    // 附件：PUT 后按 id 读回
    ureq::put(&s.url("/local/db/file/att9"))
        .set("Content-Type", "text/plain")
        .send_bytes(b"attachment-bytes")
        .expect("PUT file");
    let mut body = Vec::new();
    ureq::get(&s.url("/local/db/file/att9"))
        .call()
        .expect("GET file")
        .into_reader()
        .read_to_end(&mut body)
        .expect("read");
    assert_eq!(body, b"attachment-bytes".to_vec());

    // 重命名（db.ts 的 renameSession 走这条）
    let resp = ureq::post(&s.url("/local/db/session/p2/rename"))
        .send_string(r#"{"title":"改过了"}"#)
        .expect("rename");
    assert_eq!(resp.status(), 204);
    let after = ureq::get(&s.url("/local/db/session/p2"))
        .call()
        .unwrap()
        .into_json::<serde_json::Value>()
        .unwrap();
    assert_eq!(after["title"], "改过了");

    // 删除写墓碑，且从列表消失
    assert_eq!(
        status(ureq::delete(&s.url("/local/db/session/p2")).call()),
        204
    );
    let raw = ureq::get(&s.url("/local/db/summaries"))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(!raw.contains("\"p2\""), "墓碑不应出现在列表里: {raw}");

    // 导入暂存：清空 / 暂存 / 提交
    assert_eq!(
        status(ureq::post(&s.url("/local/db/staging/clear")).send_string("")),
        204
    );
    let stg = r#"{"id":"imp2","title":"导入的","createdAt":4000,"updatedAt":4000,"messages":[]}"#;
    assert_eq!(
        status(ureq::put(&s.url("/local/db/staging/session/imp2")).send_string(stg)),
        204
    );
    let committed = ureq::post(&s.url("/local/db/staging/commit"))
        .send_string("")
        .expect("commit")
        .into_json::<Vec<serde_json::Value>>()
        .expect("json");
    assert_eq!(committed.len(), 1);
    assert_file(&s.data(), "sessions/imp2.json");

    // 不在 /local 下的老路径仍然可用（Python 版两条都通，别把它弄坏）
    let legacy = ureq::get(&s.url("/db/summaries"))
        .call()
        .expect("GET /db/summaries");
    assert_eq!(legacy.status(), 200);
}

/// 余额接口的形状契约。
///
/// 不断言真实余额：那要联网打 api.deepseek.com，CI 里既慢又不稳。
/// 这里只锁住“没配 Key 时必须是 configured:false 且没有 error”——
/// 界面靠这两个字段区分「未配置 Key」和「查询失败」。
/// 把没配 Key 报成 error，用户就会看到一个猜不透的“查询失败”。
#[test]
fn balance_without_key_is_not_configured() {
    let s = start(true);
    let body: serde_json::Value = ureq::get(&s.url("/local/balance"))
        .call()
        .expect("GET /local/balance")
        .into_json()
        .expect("json");
    assert_eq!(
        body["configured"].as_bool(),
        Some(false),
        "没配 Key 时 configured 必须是 false: {body}"
    );
    assert!(
        body.get("error").is_none(),
        "没配 Key 不是错误，不该带 error: {body}"
    );
}

/// 反向代理的路由契约。
///
/// 本地构建下前端把聊天、模型列表、附件上传全部发到同源路径
/// （`__API_ORIGIN__` 为空），由服务端转发到 api.deepseek.com。
/// 如果服务端不认这三个前缀，请求会落到静态文件的 404，
/// 界面报“无法对话”/“API Key 无效”，与真实原因完全无关。
///
/// 这里不断言真的转发（那要联网）：只锁住
/// 「这三个路径被当作代理入口，而不是未知路径」。
#[test]
fn proxy_paths_are_routed_not_404() {
    let s = start(true);

    // 没配 Key 时，代理必须明確告诉调用方“缺 Key”，
    // 而不是把哨兵值发给上游换一个语焉不详的 401。
    for path in ["/models", "/anthropic/v1/messages", "/v1/tools"] {
        let resp = ureq::get(&s.url(path)).call();
        let code = status(resp);
        assert_eq!(
            code, 400,
            "{path} 应被当作代理入口（没 Key → 400），实际 {code}"
        );
    }

    // POST 同样要进代理，不能被“非 GET → 404”吃掉
    let code = status(
        ureq::post(&s.url("/anthropic/v1/messages"))
            .set("Content-Type", "application/json")
            .send_string(r#"{"messages":[]}"#),
    );
    assert_eq!(code, 400, "POST /anthropic/v1/messages 应进代理（400），实际 {code}");

    // 反过来：真的未知路径不能被误当代理（仍走 SPA 回退）
    let code = status(ureq::get(&s.url("/modelsx")).call());
    assert_eq!(code, 200, "/modelsx 不是代理路径，应走 SPA 回退，实际 {code}");
}
