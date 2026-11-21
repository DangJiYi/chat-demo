//! 同步端到端测试：起两个实例，让 A 去拉 B 的数据。
//!
//! 这里覆盖的是最容易悄悄坏掉、又最难在单测里发现的东西：
//!   - 对端「回答」能一起同步过来（曾经只同步到提问）
//!   - 附件同步后要登记进索引（只落 media/ 的话界面按 id 取会 404）
//!   - 配对请求只入待确认队列，等本机点接受才改配置
//!   - 归档能把旧会话搬走并从活跃列表里摘掉
//!
//! 放在 Rust 测试里而不是 CI 的 YAML 里：断言和实现同仓，改接口就立刻红。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

struct Node {
    child: Child,
    port: u16,
    /// **实际在监听**的同步端口，从 `/local/status` 读，不是 `port + 1`。
    ///
    /// 之前这里硬算 `port + 1`，结果端口绑不上时测试报的是“连接被拒”，
    /// 完全看不出是服务端根本没监听——诊断成本极高。
    sync_port: u16,
    dir: PathBuf,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Node {
    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// 同步端口：用服务端实际监听的那个。
    fn sync_url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.sync_port)
    }

    fn data(&self) -> PathBuf {
        self.dir.join("data")
    }
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr").port()
}

/// 找一个自己和一个后继端口都空闲的端口：同步端口是 port + 1。
fn free_port_pair() -> u16 {
    for _ in 0..100 {
        let p = free_port();
        // 同步端口是 p + 1，不能溢出（65535 是 u16 上限）。
        // 用 checked_add 而不是 `p >= 65535`：后者会被 clippy 的
        // absurd_extreme_comparisons 拦下，而且意图也不如这里直白。
        let Some(next) = p.checked_add(1) else {
            continue;
        };
        if TcpListener::bind(("127.0.0.1", next)).is_ok() {
            return p;
        }
    }
    panic!("找不到连续两个空闲端口");
}

fn start(tag: &str) -> Node {
    // 端口有极小竞态（另一个测试也在这时候取端口），所以失败就换个端口重来
    for attempt in 0..3 {
        if let Some(node) = try_start(tag, attempt) {
            return node;
        }
    }
    panic!("服务三次都未在 10 秒内就绪");
}

fn try_start(tag: &str, attempt: u32) -> Option<Node> {
    let dir = std::env::temp_dir().join(format!(
        "dsh-sync-{tag}-{}-{}-{attempt}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    for sub in ["data/sessions", "data/media", "data/meta", "www"] {
        std::fs::create_dir_all(dir.join(sub)).expect("mkdir");
    }
    std::fs::write(dir.join("www/index.html"), b"<!doctype html><div id=root></div>")
        .expect("write index");

    let port = free_port_pair();
    // 子进程的输出落到文件里：出问题时能直接看到它为什么没起来。
    // （之前用 Stdio::null()，绑定失败那句 eprintln! 被吞了，
    //   只能看到“连接被拒”，白跑一轮 CI。）
    let log_path = dir.join("out.log");
    let out = std::fs::File::create(&log_path).expect("create log");
    let err = out.try_clone().expect("clone log");
    let child = Command::new(env!("CARGO_BIN_EXE_dsh-chat"))
        .arg("--port")
        .arg(port.to_string())
        .arg("--data")
        .arg(dir.join("data"))
        .arg("--www")
        .arg(dir.join("www"))
        // 无界面运行：不弹浏览器、不建托盘
        .arg("--no-open")
        .arg("--console")
        .stdout(std::process::Stdio::from(out))
        .stderr(std::process::Stdio::from(err))
        .spawn()
        .expect("spawn dsh-chat");

    // UI 先就绪
    let mut node = Node {
        child,
        port,
        sync_port: 0,
        dir,
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut ready = false;
    while Instant::now() < deadline {
        if let Ok(resp) = ureq::get(&node.url("/local/status")).call() {
            if resp.status() == 200 {
                ready = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // 再等同步端口真的开始监听。
    // 服务端在主线程里同步完成绑定才进 UI 循环，所以这一步基本是瞬间的；
    // 留着重试是为了不把一个已知的时序差异变成 flake。
    if ready {
        let sync_deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < sync_deadline {
            if let Ok(resp) = ureq::get(&node.url("/local/status")).call() {
                if let Ok(st) = resp.into_json::<serde_json::Value>() {
                    if st["syncListening"].as_bool().unwrap_or(false) {
                        if let Some(p) = st["syncPort"].as_u64() {
                            node.sync_port = p as u16;
                            return Some(node);
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let detail = std::fs::read_to_string(&log_path).unwrap_or_default();
        eprintln!("─ 同步端口未监听，子进程输出：\n{detail}");
    } else {
        let detail = std::fs::read_to_string(&log_path).unwrap_or_default();
        eprintln!("─ 服务未就绪，子进程输出：\n{detail}");
    }

    // 没起来：清干净再重试
    let _ = node.child.kill();
    let _ = node.child.wait();
    let _ = std::fs::remove_dir_all(&node.dir);
    None
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn write_file(path: &Path, data: &[u8]) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    let mut f = std::fs::File::create(path).expect("create");
    f.write_all(data).expect("write");
}

fn get_json(url: &str) -> serde_json::Value {
    ureq::get(url)
        .timeout(Duration::from_secs(10))
        .call()
        .expect("GET")
        .into_json()
        .expect("json")
}

/// 轮询直到某个附件能按 id 读回。
///
/// 不能“等到了会话就以为附件也到了”：`/local/sync/now` 是异步的
/// （`thread::spawn`），而 `pull_once` 里**先合并会话、后下载附件**。
/// 会话落盘是本地写，附件要额外跑一趟 HTTP，两者之间有真实时间差。
/// 直接断言就是赌那个差值足够小——CI 一慢就红。
fn wait_file(url: &str, want: &[u8]) -> Option<Vec<u8>> {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Ok(resp) = ureq::get(url).timeout(Duration::from_secs(2)).call() {
            if resp.status() == 200 {
                let mut body = Vec::new();
                if resp.into_reader().read_to_end(&mut body).is_ok() && body == want {
                    return Some(body);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

/// 轮询直到 /db/session/<id> 能读到，返回文档。
fn wait_session(url: &str) -> Option<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(resp) = ureq::get(url).timeout(Duration::from_secs(2)).call() {
            if resp.status() == 200 {
                if let Ok(v) = resp.into_json::<serde_json::Value>() {
                    return Some(v);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

#[test]
fn sync_pulls_answer_attachment_pairing_and_archive() {
    // ---------- 对端 B：一条近期会话 + 一个附件 ----------
    let b = start("b");
    let stamp = now_ms();
    let doc = serde_json::json!({
        "id": "p1",
        "title": "对端会话",
        "createdAt": stamp,
        "updatedAt": stamp,
        "messages": [
            { "id": "m1", "role": "user", "content": "你好" },
            {
                "id": "m2",
                "role": "assistant",
                "status": "done",
                "blocks": [{ "type": "text", "text": "回答正文" }]
            }
        ]
    });
    write_file(
        &b.data().join("sessions/p1.json"),
        doc.to_string().as_bytes(),
    );

    let payload = b"hello-sync";
    let sha = format!("{:x}", Sha256::digest(payload));
    write_file(&b.data().join("media").join(&sha), payload);
    let index = serde_json::json!({
        "files": {
            "pf1": {
                "id": "pf1",
                "sha": &sha,
                "name": "note.txt",
                "size": payload.len(),
                "mime": "text/plain",
                "isImage": false,
                "createdAt": stamp
            }
        },
        "archive": []
    });
    write_file(
        &b.data().join("meta/index.json"),
        index.to_string().as_bytes(),
    );

    // ---------- 本机 A：一条 1970 年的会话（给归档用） ----------
    let a = start("a");
    write_file(
        &a.data().join("sessions/old1.json"),
        r#"{"id":"old1","title":"旧会话","createdAt":1000,"updatedAt":1000,"messages":[{"id":"om","role":"user","content":"很久以前"}]}"#
            .as_bytes(),
    );

    // ---------- 同步端口本身 ----------
    let health = get_json(&b.sync_url("/health"));
    assert!(
        health["ok"].as_bool().unwrap_or(false),
        "同步端口 /health 异常: {health}"
    );

    let since = get_json(&b.sync_url("/sync/since?since=0"));
    assert!(
        since["sessions"].as_array().map(|a| a.len()).unwrap_or(0) >= 1,
        "同步端口未返回会话: {since}"
    );
    assert!(
        since["files"].as_array().map(|a| a.len()).unwrap_or(0) >= 1,
        "同步端口未返回附件元数据: {since}"
    );
    assert!(since["now"].as_i64().unwrap_or(0) > 0, "缺少游标 now");

    // ---------- A 指向 B 并拉一次 ----------
    let patch = serde_json::json!({ "peer": format!("127.0.0.1:{}", b.sync_port) });
    ureq::put(&a.url("/local/config"))
        .timeout(Duration::from_secs(5))
        .send_json(patch)
        .expect("PUT config");
    ureq::post(&a.url("/local/sync/now"))
        .timeout(Duration::from_secs(5))
        .send_string("")
        .expect("POST sync/now");

    let pulled = wait_session(&a.url("/db/session/p1")).expect("同步后本机没有 p1 会话");
    assert_eq!(pulled["title"], "对端会话", "同步后标题不对");
    let msgs = pulled["messages"].as_array().expect("messages 数组");
    assert_eq!(msgs.len(), 2, "同步后消息数不对: {}", msgs.len());
    // 关键回归：回答必须一起过来
    assert_eq!(msgs[1]["role"], "assistant", "第二条不是回答");
    assert_eq!(
        msgs[1]["blocks"][0]["text"], "回答正文",
        "回答内容丢失: {}",
        msgs[1]
    );

    // ---------- 附件：落盘 + 登记进索引 ----------
    // 等它真的到（会话先到不代表附件也到了），再断言内容。
    wait_file(&a.url("/db/file/pf1"), payload).expect("同步过来的附件读不回来");
    assert!(
        a.data().join("media").join(&sha).is_file(),
        "附件原件未按 sha 落盘"
    );

    // ---------- 归档 ----------
    let ar = ureq::post(&a.url("/local/archive/now"))
        .timeout(Duration::from_secs(30))
        .send_string("")
        .expect("archive/now")
        .into_json::<serde_json::Value>()
        .expect("json");
    assert!(
        ar["archived"].as_i64().unwrap_or(0) >= 1,
        "归档条数不对: {ar}"
    );
    assert!(
        !a.data().join("sessions/old1.json").is_file(),
        "归档后活跃会话文件应已移走"
    );
    let archived: Vec<serde_json::Value> = ureq::get(&a.url("/local/archived"))
        .call()
        .expect("archived")
        .into_json()
        .expect("json");
    assert!(
        archived.iter().any(|x| x["id"] == "old1"),
        "归档列表里没有 old1: {archived:?}"
    );
    let storage = get_json(&a.url("/local/storage"));
    assert!(
        storage["archivedSessions"].as_i64().unwrap_or(0) >= 1,
        "storage 未统计到归档: {storage}"
    );

    // ---------- 配对：请求只入队，本机接受才生效 ----------
    let before = get_json(&a.url("/local/config"));
    let pair_body = serde_json::json!({ "ip": "10.0.0.9", "port": 9999, "name": "手机" });
    ureq::post(&a.sync_url("/peer"))
        .timeout(Duration::from_secs(5))
        .send_json(pair_body)
        .expect("POST /peer");

    let reqs: Vec<serde_json::Value> = ureq::get(&a.url("/local/pair/requests"))
        .call()
        .expect("pair/requests")
        .into_json()
        .expect("json");
    assert_eq!(reqs.len(), 1, "待确认配对应为 1 条: {reqs:?}");
    assert_eq!(reqs[0]["ip"], "10.0.0.9");
    assert_eq!(reqs[0]["name"], "手机");

    let untouched = get_json(&a.url("/local/config"));
    assert_eq!(
        untouched["peer"], before["peer"],
        "收到配对请求就改了配置（应该等本机接受）"
    );

    ureq::post(&a.url("/local/pair/accept"))
        .timeout(Duration::from_secs(10))
        .send_json(serde_json::json!({ "ip": "10.0.0.9", "port": 9999 }))
        .expect("pair/accept");
    let after = get_json(&a.url("/local/config"));
    assert_eq!(after["peer"], "10.0.0.9:9999", "接受配对后配置未生效");
    let reqs: Vec<serde_json::Value> = ureq::get(&a.url("/local/pair/requests"))
        .call()
        .expect("pair/requests")
        .into_json()
        .expect("json");
    assert!(reqs.is_empty(), "接受后待确认队列未清空: {reqs:?}");
}
