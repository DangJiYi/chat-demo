//! 同步：合并逻辑、拉取、同步端口服务、局域网发现与配对。
//!
//! 契约与 server.py 一致。最容易搞错的是 merge 的语义：
//!   「回答完成不推进 updatedAt」，所以不能只看时间戳决定谁新，
//!   否则对端补上的回答会被本地旧副本盖掉（这正是之前修过的 bug）。

use std::io::Read;
use std::net::{ToSocketAddrs, UdpSocket};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::store::{now_ms, Store};

/// 拉取时向前回看的窗口。
///
/// 前端只在「发问」时推进 updatedAt，回答完成时不动这个字段，
/// 所以对端可能先拉到「只有问题」的快照，之后回答落盘但时间戳没变。
/// 回看 + 幂等合并才能把回答同步过去。
pub const SYNC_LOOKBACK_MS: i64 = 15 * 60 * 1000;

/// UDP 发现端口与协议标记（与 Python 版一致）。
pub const DISCOVERY_PORT: u16 = 8792;
const DISCOVER_MAGIC: &[u8] = b"DSH-CHAT-DISCOVER/1";
const DISCOVER_REPLY: &[u8] = b"DSH-CHAT-HERE/1";

/// 同步运行状态，供 /local/sync/status 展示。
///
/// 字段都是 Default 类型（bool / String / i64），所以直接 derive：
/// 手写 impl 只会多一份需要同步维护的代码。
#[derive(Debug, Clone, Default, Serialize)]
pub struct SyncState {
    pub enabled: bool,
    pub peer: String,
    #[serde(rename = "lastOk")]
    pub last_ok: i64,
    #[serde(rename = "lastTry")]
    pub last_try: i64,
    #[serde(rename = "lastErr")]
    pub last_err: String,
    pub pulled: i64,
    pub cursor: i64,
}

/// 待确认的配对请求（像蓝牙那样：对方请求，本机点接受才生效）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairRequest {
    pub ip: String,
    pub port: u16,
    pub name: String,
    pub at: i64,
}

// ------------------------------------------------------------------ 合并

/// 助手消息的终态；用户消息没有 status，也算定稿。
fn is_final(msg: &serde_json::Value) -> bool {
    match msg.get("status") {
        None => true,
        Some(v) => match v.as_str() {
            Some(s) => matches!(s, "done" | "stopped" | "error"),
            // status 不是字符串（null 等）时按未定稿处理
            None => false,
        },
    }
}

/// new 是否比 cur 更完整：已定稿胜过未定稿，否则比内容块多少。
fn more_complete(cur: &serde_json::Value, new: &serde_json::Value) -> bool {
    let cur_final = is_final(cur);
    let new_final = is_final(new);
    if !cur_final && new_final {
        return true;
    }
    if cur_final && !new_final {
        return false;
    }
    let cur_len = cur
        .get("blocks")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let new_len = new
        .get("blocks")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    new_len > cur_len
}

/// 把对端会话合入本地。返回是否真的改了本地。
pub fn merge_remote(store: &Store, remote: &serde_json::Value) -> Result<bool, String> {
    let Some(rid) = remote.get("id").and_then(|v| v.as_str()) else {
        return Ok(false);
    };
    if rid.is_empty() {
        return Ok(false);
    }
    let rid = rid.to_string();

    let local_raw = store.load_session(&rid).map_err(|e| e.to_string())?;
    let Some(local_raw) = local_raw else {
        // 本地没有：直接落盘
        let raw = serde_json::to_vec(remote).map_err(|e| e.to_string())?;
        store.save_session(&rid, &raw)?;
        return Ok(true);
    };

    let mut local: serde_json::Value =
        serde_json::from_slice(&local_raw).map_err(|e| format!("本地会话损坏: {e}"))?;

    let local_del = local
        .get("deleted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let remote_del = remote
        .get("deleted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // 墓碑优先：删掉的会话不能被旧副本复活
    if remote_del && !local_del {
        let raw = serde_json::to_vec(remote).map_err(|e| e.to_string())?;
        store.save_session(&rid, &raw)?;
        return Ok(true);
    }
    if local_del && !remote_del {
        return Ok(false);
    }

    let lt = local
        .get("updatedAt")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let rt = remote
        .get("updatedAt")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if rt > lt {
        // 对端确实更新，整份替换
        let raw = serde_json::to_vec(remote).map_err(|e| e.to_string())?;
        store.save_session(&rid, &raw)?;
        return Ok(true);
    }

    // rt <= lt：逐条按完整度合并，不能整份丢弃
    let mut remote_by_id: std::collections::HashMap<String, serde_json::Value> =
        std::collections::HashMap::new();
    if let Some(arr) = remote.get("messages").and_then(|v| v.as_array()) {
        for msg in arr {
            if let Some(mid) = msg.get("id").and_then(|v| v.as_str()) {
                remote_by_id.insert(mid.to_string(), msg.clone());
            }
        }
    }

    let mut changed = false;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<serde_json::Value> = Vec::new();

    let local_msgs = local
        .get("messages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for msg in &local_msgs {
        let mid = msg
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        match mid {
            Some(id) if remote_by_id.contains_key(&id) => {
                seen.insert(id.clone());
                let better = remote_by_id.get(&id).map(|r| more_complete(msg, r));
                if better == Some(true) {
                    out.push(remote_by_id[&id].clone());
                    changed = true;
                } else {
                    out.push(msg.clone());
                }
            }
            _ => out.push(msg.clone()),
        }
    }
    // 追加本地没有的
    if let Some(arr) = remote.get("messages").and_then(|v| v.as_array()) {
        for msg in arr {
            let Some(mid) = msg.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            if seen.contains(mid) {
                continue;
            }
            out.push(msg.clone());
            seen.insert(mid.to_string());
            changed = true;
        }
    }

    if let Some(obj) = local.as_object_mut() {
        obj.insert("messages".to_string(), serde_json::Value::Array(out));
        // 树结构字段本地缺就从对端补，否则新追加的消息接不上
        for key in ["rootMessageId", "selectedChildId"] {
            let missing = obj
                .get(key)
                .map(|v| v.is_null() || v.as_str() == Some(""))
                .unwrap_or(true);
            if missing {
                if let Some(v) = remote.get(key) {
                    if !(v.is_null() || v.as_str() == Some("")) {
                        obj.insert(key.to_string(), v.clone());
                        changed = true;
                    }
                }
            }
        }
        let no_title = obj
            .get("title")
            .and_then(|v| v.as_str())
            .map(|s| s.is_empty())
            .unwrap_or(true);
        if no_title {
            if let Some(t) = remote.get("title").and_then(|v| v.as_str()) {
                if !t.is_empty() {
                    obj.insert("title".to_string(), serde_json::Value::String(t.to_string()));
                    changed = true;
                }
            }
        }
    }

    if !changed {
        return Ok(false);
    }
    // 刻意不推进 updatedAt：靠回看窗口重新拉取，合并幂等，不会来回覆盖
    let raw = serde_json::to_vec(&local).map_err(|e| e.to_string())?;
    store.save_session(&rid, &raw)?;
    Ok(true)
}

// ------------------------------------------------------------------ 网络

/// 把配置里的 peer 规范成完整 URL。
///
/// "192.168.1.5" → "http://192.168.1.5:8791"
/// "192.168.1.5:9000" → "http://192.168.1.5:9000"
pub fn peer_url(peer: &str) -> String {
    let peer = peer.trim();
    if peer.is_empty() {
        return String::new();
    }
    let with_scheme = if peer.starts_with("http://") || peer.starts_with("https://") {
        peer.to_string()
    } else {
        format!("http://{peer}")
    };
    let rest = with_scheme.split_once("://").map(|(_, r)| r).unwrap_or("");
    if !rest.contains(':') {
        return format!("{with_scheme}:8791");
    }
    with_scheme.trim_end_matches('/').to_string()
}

/// 是否局域网内的地址（配对请求只接受这些来源）。
///
/// 同步端口听 `0.0.0.0`，如果这台机器直接挂在公网上，任何人都会发配对请求。
/// 虽然最终要本机用户点「接受」才会生效，但没必要让请求随便进来。
///
/// `Ipv4Addr::is_private` 覆盖 10/8、172.16/12、192.168/16；
/// `is_link_local` 是 169.254/16（自配地址）；回环也算——那是同机的另一个进程。
pub fn is_lan_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        std::net::IpAddr::V6(v6) => v6.is_loopback(),
    }
}

/// 字符串版的 [`is_lan_ip`]：解析不了就当不是（宁严勿松）。
pub fn is_lan_ip_str(ip: &str) -> bool {
    ip.parse::<std::net::IpAddr>().map(is_lan_ip).unwrap_or(false)
}

/// 本机在局域网里的 IPv4（不含回环）。
///
/// 主要手段是 UDP connect 取出口地址：不真的发包，只让系统选默认网卡。
/// 探多个目标，是因为接 VPN / 多网卡时不同路由会选到不同源地址；
/// 只试着连一个 223.5.5.5，会漏掉「对端实际能连上的那个地址」。
///
/// 没有默认路由（离线、纯局域网、刚开热点）时探不出东西，退到解析主机名——
/// 与 Python 版一致。这正是「本端地址显示不出来」的那个场景。
///
/// 结果缓 30 秒：`/local/status` 是界面会反复调的接口，
/// 而主机名解析可能卡在 NetBIOS/DNS 上，不能让每个请求都去等。
pub fn local_ips() -> Vec<String> {
    static CACHE: std::sync::Mutex<Option<(std::time::Instant, Vec<String>)>> =
        std::sync::Mutex::new(None);

    if let Ok(guard) = CACHE.lock() {
        if let Some((at, ips)) = guard.as_ref() {
            if at.elapsed() < Duration::from_secs(30) && !ips.is_empty() {
                return ips.clone();
            }
        }
    }

    let ips = probe_local_ips();
    if let Ok(mut guard) = CACHE.lock() {
        *guard = Some((std::time::Instant::now(), ips.clone()));
    }
    ips
}

fn probe_local_ips() -> Vec<String> {
    let probes = [
        "223.5.5.5:80",
        "1.1.1.1:80",
        "8.8.8.8:80",
        "192.168.255.255:80",
        "10.255.255.255:80",
    ];
    let mut out: Vec<String> = Vec::new();
    for probe in probes {
        let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else {
            continue;
        };
        if sock.connect(probe).is_err() {
            continue;
        }
        let Ok(addr) = sock.local_addr() else {
            continue;
        };
        let ip = addr.ip().to_string();
        // 0.0.0.0 表示还没选到网卡，127.* 是本机回环，对端都连不上
        if ip.starts_with("127.") || ip == "0.0.0.0" {
            continue;
        }
        if !out.contains(&ip) {
            out.push(ip);
        }
    }
    if !out.is_empty() {
        return out;
    }

    // 退路：解析主机名（Windows 上通常走 hosts/NetBIOS，不走公网 DNS）
    let host = crate::store::hostname();
    if let Ok(addrs) = (host.as_str(), 0u16).to_socket_addrs() {
        for addr in addrs {
            let ip = addr.ip();
            if ip.is_ipv4() && !ip.is_loopback() && !ip.is_unspecified() {
                let s = ip.to_string();
                if !out.contains(&s) {
                    out.push(s);
                }
            }
        }
    }
    out
}

/// 广播目标：受限广播 + 按本机网卡推算的子网广播。
fn broadcast_targets() -> Vec<String> {
    let mut targets = vec!["255.255.255.255".to_string()];
    for ip in local_ips() {
        let parts: Vec<&str> = ip.split('.').collect();
        if parts.len() == 4 {
            targets.push(format!("{}.{}.{}.255", parts[0], parts[1], parts[2]));
        }
    }
    targets
}

#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredPeer {
    pub name: String,
    pub ip: String,
    pub port: u16,
}

/// 广播询问局域网里还有谁，收集应答。
pub fn discover_peers(store: &Store, timeout: Duration) -> Vec<DiscoveredPeer> {
    let mut found: std::collections::HashMap<String, DiscoveredPeer> =
        std::collections::HashMap::new();

    let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else {
        return Vec::new();
    };
    if sock.set_broadcast(true).is_err() {
        return Vec::new();
    }
    let _ = sock.set_read_timeout(Some(Duration::from_millis(400)));

    let cfg = store.config();
    let name = cfg
        .get("deviceName")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let sync_port = store.sync_port_advertised();
    let payload = format!(
        "{} {}",
        String::from_utf8_lossy(DISCOVER_MAGIC),
        serde_json::json!({ "name": name, "port": sync_port })
    );

    for target in broadcast_targets() {
        let _ = sock.send_to(payload.as_bytes(), (target.as_str(), DISCOVERY_PORT));
    }

    let deadline = std::time::Instant::now() + timeout;
    let mut buf = [0u8; 2048];
    while std::time::Instant::now() < deadline {
        let Ok((n, addr)) = sock.recv_from(&mut buf) else {
            continue;
        };
        let data = &buf[..n];
        if !data.starts_with(DISCOVER_REPLY) {
            continue;
        }
        let Some(space) = data.iter().position(|b| *b == b' ') else {
            continue;
        };
        let Ok(info) = serde_json::from_slice::<serde_json::Value>(&data[space + 1..]) else {
            continue;
        };
        let ip = addr.ip().to_string();
        let port = info.get("port").and_then(|v| v.as_u64()).unwrap_or(8791) as u16;
        let pname = info
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or(&ip)
            .to_string();
        // 同一台机器会从多个网卡各回一次，按设备名去重
        let key = if pname == ip {
            format!("{ip}:{port}")
        } else {
            pname.clone()
        };
        found.entry(key).or_insert(DiscoveredPeer {
            name: pname,
            ip,
            port,
        });
    }

    found.into_values().collect()
}

/// 后台线程：监听发现广播并应答。绑定失败不致命（端口被占）。
pub fn discovery_server(store: &'static Store) {
    let sock = match UdpSocket::bind(("0.0.0.0", DISCOVERY_PORT)) {
        Ok(s) => s,
        Err(_) => return, // 端口被占：静默放弃，不影响主功能
    };
    let _ = sock.set_read_timeout(Some(Duration::from_secs(2)));
    let mut buf = [0u8; 2048];
    loop {
        let Ok((n, addr)) = sock.recv_from(&mut buf) else {
            continue;
        };
        if !buf[..n].starts_with(DISCOVER_MAGIC) {
            continue;
        }
        let cfg = store.config();
        let name = cfg
            .get("deviceName")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let sync_port = store.sync_port_advertised();
        let reply = format!(
            "{} {}",
            String::from_utf8_lossy(DISCOVER_REPLY),
            serde_json::json!({ "name": name, "port": sync_port })
        );
        let _ = sock.send_to(reply.as_bytes(), addr);
    }
}

// ------------------------------------------------------------------ 拉取

/// 访问局域网对端用的 agent。
///
/// **刻意关掉系统代理。** 对端在局域网内（10.x / 192.168.x），
/// 走 HTTP_PROXY 会把请求送到公司代理/科学上网代理上，那里当然找不到对端；
/// Python 版的 urllib 就会读环境变量，所以“浏览器能配同步、命令行不行”这类
/// 怪现象往往出在这里。
///
/// 仍然带上 TLS 连接器：万一用户给对端写了 https://，不至于报
/// “no TLS backend is configured” 这种看不出原因的错。
pub(crate) fn lan_agent(timeout: Duration) -> ureq::Agent {
    let builder = ureq::AgentBuilder::new()
        .timeout(timeout)
        .try_proxy_from_env(false)
        .redirects(0);
    crate::http::with_tls(builder).build()
}

/// 拉一次对端数据。失败只记录在状态里，不抛错（同步是尽力而为）。
///
/// 拿不到锁时会等（手动点「立即同步」就应该真的跑一趟）。
pub fn pull_once(store: &Store) {
    let _guard = store.lock_pull();
    pull_locked(store);
}

/// 后台循环用：已有一轮在跑就跳过本轮，不叠线程。返回是否真的跑了一轮。
pub fn try_pull_once(store: &Store) -> bool {
    let Some(_guard) = store.try_lock_pull() else {
        return false;
    };
    pull_locked(store);
    true
}

/// 真正干活的拉取。调用方必须已持拉取锁。
fn pull_locked(store: &Store) {
    let cfg = store.config();
    let peer = cfg
        .get("peer")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let base = peer_url(&peer);

    store.set_sync_state(|s| {
        s.last_try = now_ms();
        s.peer = peer.clone();
        s.enabled = !base.is_empty();
    });

    if base.is_empty() {
        return;
    }

    let cursor = store.sync_state().cursor;
    let since = (cursor - SYNC_LOOKBACK_MS).max(0);
    let url = format!("{base}/sync/since?since={since}");

    let resp = lan_agent(Duration::from_secs(8)).get(&url).call();
    let data = match resp {
        Ok(r) => match r.into_json::<serde_json::Value>() {
            Ok(v) => v,
            Err(e) => {
                note_err(store, format!("响应解析失败: {e}"));
                return;
            }
        },
        Err(e) => {
            note_err(store, format!("{e}"));
            return;
        }
    };

    let mut pulled = 0i64;
    if let Some(sessions) = data.get("sessions").and_then(|v| v.as_array()) {
        for doc in sessions {
            if let Ok(true) = merge_remote(store, doc) {
                pulled += 1;
            }
        }
    }

    // 附件：本机没有的就拉下来，并且**登记进索引**。
    //
    // 只把原件写进 media/<sha> 是不够的：界面是按 id 取附件的
    // （GET /db/file/<id> 查 index.json 里的 files[id]），没登记就 404，
    // 表现就是「同步过来的消息里附件点不开」。
    if let Some(files) = data.get("files").and_then(|v| v.as_array()) {
        for fm in files {
            let Ok(meta) = serde_json::from_value::<crate::store::FileMeta>(fm.clone()) else {
                continue;
            };
            if meta.id.is_empty() || meta.sha.is_empty() || store.has_file(&meta.id) {
                continue;
            }
            let bytes = if store.has_media(&meta.sha) {
                // 原件已在（另一个 id 用过同一份），直接登记
                std::fs::read(store.dir().join("media").join(&meta.sha)).ok()
            } else {
                let file_url = format!("{base}/file/{}", meta.sha);
                match lan_agent(Duration::from_secs(30)).get(&file_url).call() {
                    Ok(r) => {
                        let mut buf = Vec::new();
                        match r.into_reader().read_to_end(&mut buf) {
                            Ok(_) => Some(buf),
                            Err(_) => None,
                        }
                    }
                    Err(_) => None,
                }
            };
            if let Some(buf) = bytes {
                // 网络拉取失败不记日志：下一轮会重试，而且对端离线时
                // 每 3 秒写一行会把 app.log 刷满。
                // 但**本地写入失败**是真问题（磁盘满、权限、路径异常），
                // 不会自愈，必须留痕。
                if let Err(e) = store.put_file(&meta.id, &buf, &meta.mime, &meta.name, meta.is_image)
                {
                    crate::applog::write(
                        store,
                        &format!("附件 {} 写入失败: {e}", meta.id),
                    );
                }
            }
        }
    }

    let now = data.get("now").and_then(|v| v.as_i64()).unwrap_or(0);
    let recovered = !store.sync_state().last_err.is_empty();
    store.set_sync_state(|s| {
        s.last_ok = now_ms();
        s.last_err.clear();
        s.cursor = now;
        s.pulled += pulled;
    });
    if recovered {
        crate::applog::write(store, "同步已恢复");
    }
}

/// 记下同步错误，**只在错误变化时**写日志。
///
/// 同步是每几秒一轮的后台动作，对端离线时会持续失败；
/// 每轮都写日志会把 app.log 刷满，也看不出是哪天出的问题。
fn note_err(store: &Store, msg: String) {
    let changed = store.sync_state().last_err != msg;
    if changed {
        crate::applog::write(store, &format!("同步失败: {msg}"));
    }
    store.set_sync_state(|s| s.last_err = msg);
}

/// 后台同步循环：有对端就按间隔拉，没对端就慢速轮询。
pub fn sync_loop(store: &'static Store) {
    loop {
        let cfg = store.config();
        let peer = cfg
            .get("peer")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let interval = cfg
            .get("intervalSec")
            .and_then(|v| v.as_i64())
            .unwrap_or(3)
            .clamp(1, 60) as u64;
        if peer.is_empty() {
            std::thread::sleep(Duration::from_secs(2));
        } else {
            // try_ 而不是阻塞：本轮已有拉取在跑（手动点、或上一轮卡在超时）
            // 就跳过，下一轮再来。
            try_pull_once(store);
            std::thread::sleep(Duration::from_secs(interval));
        }
    }
}

// ------------------------------------------------------------------ 启动归档线程

/// 启动后延迟整理：归档 + 去重（无损），不做任何删除性动作。
pub fn archive_tick(store: &'static Store) {
    std::thread::sleep(Duration::from_secs(30));
    // 与同步共用一把锁：归档写 index.json 的 archive 键，
    // 同步写同一个文件的 files 键，而两边都是「读整个文件 → 改一个键 → 写回」。
    // 不串行的话，后写的会把先写的那个键整片抹掉——
    // 表现是「附件全部 404」或「归档列表空了」，而且只在并发时偶发。
    let _guard = store.lock_pull();
    match crate::archive::archive_once(store) {
        Ok(0) => {}
        Ok(n) => {
            crate::applog::write(store, &format!("归档 {n} 个会话"));
        }
        Err(e) => {
            crate::applog::write(store, &format!("归档失败: {e}"));
            eprintln!("归档失败: {e}");
        }
    }
    let _ = crate::archive::compact_archive(store);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    fn lan(ip: &str) -> bool {
        is_lan_ip(ip.parse::<IpAddr>().expect("ip"))
    }

    /// 配对闸门：只有局域网来源才允许入队。
    ///
    /// 这条 e2e 跑不到（本机只能从回环发），而它直接决定「别人能不能
    /// 给你的配对队列塞东西」，所以单独钉住。
    #[test]
    fn lan_ip_gate() {
        // 三类私有网段 + 自配地址 + 回环：都应该放行
        for ip in [
            "192.168.1.5",
            "10.0.0.9",
            "10.255.255.254",
            "172.16.3.4",
            "172.31.255.1",
            "169.254.10.20",
            "127.0.0.1",
            "::1",
        ] {
            assert!(lan(ip), "应该当作局域网: {ip}");
        }

        // 公网 / 特殊地址：必须拒绝
        for ip in [
            "8.8.8.8",
            "1.1.1.1",
            // 172.32 已经出了 172.16/12 的范围，这是最容易写错边界的那个
            "172.32.0.1",
            "0.0.0.0",
            "224.0.0.1",
        ] {
            assert!(!lan(ip), "不该当作局域网: {ip}");
        }

        // 解析不了的字符串一律拒（宁严勿松）
        assert!(!is_lan_ip_str(""));
        assert!(!is_lan_ip_str("not-an-ip"));
        assert!(!is_lan_ip_str("192.168.1.5:8791"));
    }

    /// peer_url 的规范化：几种写法要落到同一个形状。
    #[test]
    fn peer_url_normalizes() {
        assert_eq!(peer_url("192.168.1.5"), "http://192.168.1.5:8791");
        assert_eq!(peer_url("192.168.1.5:9000"), "http://192.168.1.5:9000");
        assert_eq!(peer_url("http://192.168.1.5"), "http://192.168.1.5:8791");
        assert_eq!(
            peer_url("http://192.168.1.5:9000/"),
            "http://192.168.1.5:9000"
        );
        assert_eq!(peer_url(""), "");
        assert_eq!(peer_url("   "), "");
    }
}
