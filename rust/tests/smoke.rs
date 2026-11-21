//! 端到端冒烟测试：启动真实二进制，用 HTTP 请求验证契约。
//!
//! 把这些断言放在 Rust 里而不是 CI 的 YAML 里，有两个原因：
//!   1. 断言和实现同仓同语言，改接口时 cargo test 立刻报错
//!   2. YAML 里的 PowerShell 断言容易写错类型（byte[] vs string、@() 包数组），
//!      而且只有推上去跑 CI 才知道对不对
//!
//! 每个测试起一个独立实例（独立端口 + 独立临时目录），互不影响。

use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

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
        format!("http://127.0.0.1:{}{}", self.port, path)
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
    let dir = std::env::temp_dir().join(format!(
        "dsh-rust-test-{}-{}",
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

    // 等就绪：轮询 /local/status，最多 10 秒
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(resp) = ureq::get(&server.url("/local/status")).call() {
            if resp.status() == 200 {
                return server;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("服务未在 10 秒内就绪（端口 {port}）");
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
    assert_eq!(body["version"], "0.2.0");
    assert_eq!(body["port"], s.port);
    assert!(body["dataDir"].as_str().unwrap().contains("data"));
    assert!(body["platform"].is_string());
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
    assert_eq!(tomb["deleted"], true, "应写入墓碑标记");

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
    assert_eq!(st["configured"], false, "初始不应有 Key");

    // 用一个明显不是真凭据的字符串，只验证格式与存储位置
    let resp: serde_json::Value = ureq::put(&s.url("/local/apikey"))
        .set("Content-Type", "application/json")
        .send_string(r#"{"apiKey":"SYNTHETIC-0000"}"#)
        .unwrap()
        .into_json()
        .unwrap();
    assert_eq!(resp["configured"], true);
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
    assert_eq!(st2["configured"], false, "删除后应回到未配置");
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
