//! 归档索引、用量统计、全文搜索。
//!
//! 从 store.rs 拆出来：store 负责「会话与附件的落盘」，
//! 这里负责「跨会话的聚合读取」（归档列表、计费汇总、搜索）。
//! 口径必须与 server.py 一致。

use std::collections::HashSet;
use std::fs;
use std::io::Read;

use serde::Serialize;

use crate::billing;
use crate::store::{now_ms, Store};

/// 归档条目（index.json 里 archive 数组的一项）。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct ArchivedItem {
    pub id: String,
    #[serde(default)]
    pub month: String,
    #[serde(default)]
    pub title: String,
    #[serde(rename = "updatedAt", default)]
    pub updated_at: i64,
    #[serde(rename = "msgCount", default)]
    pub msg_count: i64,
}

fn read_index(store: &Store) -> serde_json::Value {
    match fs::read(store.dir().join("meta").join("index.json")) {
        Ok(raw) => serde_json::from_slice(&raw).unwrap_or_else(|_| serde_json::json!({})),
        Err(_) => serde_json::json!({}),
    }
}

fn archived_ids(store: &Store) -> Vec<ArchivedItem> {
    let idx = read_index(store);
    idx.get("archive")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| serde_json::from_value::<ArchivedItem>(v.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// 读归档 JSONL 里的所有会话文档。
///
/// 归档是「追加 gzip member」写的，一个文件里可能有同一会话的多份；
/// 调用方需要时自己按 archivedAt 去重。
fn read_archive_docs(store: &Store, name: &str) -> Vec<serde_json::Value> {
    let path = store.dir().join("archive").join(name);
    let file = match fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    // 多个 gzip member 串联的流，MultiGzDecoder 会一直读到 EOF
    let mut decoder = flate2::read::MultiGzDecoder::new(file);
    let mut text = String::new();
    if decoder.read_to_string(&mut text).is_err() {
        return Vec::new();
    }
    text.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|env| env.get("session").cloned())
        .collect()
}

/// 归档列表（不含墓碑），按 updatedAt 倒序。
pub fn list_archived(store: &Store) -> Vec<ArchivedItem> {
    let idx = read_index(store);
    let deleted: HashSet<String> = idx
        .get("archiveDeleted")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let mut items: Vec<ArchivedItem> = archived_ids(store)
        .into_iter()
        .filter(|a| !deleted.contains(&a.id))
        .collect();
    items.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    items
}

/// 把归档里的某个会话恢复成普通会话。返回是否成功与原因。
pub fn restore_archived(store: &Store, sid: &str) -> Result<(), String> {
    let items = archived_ids(store);
    let Some(item) = items.iter().find(|a| a.id == sid) else {
        return Err("归档里没有这条会话".into());
    };
    let path = store
        .dir()
        .join("archive")
        .join(format!("archive_{}.jsonl.gz", item.month));
    let file = fs::File::open(&path).map_err(|_| "归档文件打不开".to_string())?;
    let mut decoder = flate2::read::MultiGzDecoder::new(file);
    let mut text = String::new();
    decoder
        .read_to_string(&mut text)
        .map_err(|_| "归档文件打不开".to_string())?;

    for line in text.lines() {
        let Ok(env) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(doc) = env.get("session") else { continue };
        if doc.get("id").and_then(|v| v.as_str()) != Some(sid) {
            continue;
        }
        let raw = serde_json::to_vec(doc).map_err(|e| e.to_string())?;
        store.save_session(sid, &raw)?;
        // 从归档索引里移除
        let mut idx = read_index(store);
        if let Some(arr) = idx.get_mut("archive").and_then(|v| v.as_array_mut()) {
            arr.retain(|a| a.get("id").and_then(|v| v.as_str()) != Some(sid));
        }
        let out = serde_json::to_vec(&idx).map_err(|e| e.to_string())?;
        crate::store::atomic_write(&store.dir().join("meta").join("index.json"), &out)
            .map_err(|e| format!("写入失败: {e}"))?;
        return Ok(());
    }
    Err("归档文件里没找到".into())
}

/// 删归档条目：从 archive 数组移出，并记入 archiveDeleted（防止同步把旧副本拉回）。
/// 返回是否命中。
pub fn delete_archived(store: &Store, sid: &str) -> Result<bool, String> {
    let mut idx = read_index(store);
    // index.json 可能被坏数据写成非对象，IndexMut 在非对象上会 panic
    if !idx.is_object() {
        idx = serde_json::json!({});
    }
    let mut hit = false;
    if let Some(arr) = idx.get_mut("archive").and_then(|v| v.as_array_mut()) {
        let before = arr.len();
        arr.retain(|a| a.get("id").and_then(|v| v.as_str()) != Some(sid));
        hit = arr.len() != before;
    }
    if !hit {
        return Ok(false);
    }
    // 墓碑：同步时告诉对端“这条删了”，否则对端会把旧归档又推回来
    let has_tomb = matches!(idx.get("archiveDeleted"), Some(v) if v.is_array());
    if !has_tomb {
        idx["archiveDeleted"] = serde_json::json!([]);
    }
    if let Some(tomb) = idx.get_mut("archiveDeleted").and_then(|v| v.as_array_mut()) {
        if !tomb.iter().any(|v| v.as_str() == Some(sid)) {
            tomb.push(serde_json::Value::String(sid.to_string()));
        }
    }
    let out = serde_json::to_vec(&idx).map_err(|e| e.to_string())?;
    crate::store::atomic_write(&store.dir().join("meta").join("index.json"), &out)
        .map_err(|e| format!("写入失败: {e}"))?;
    Ok(true)
}

/// 用量汇总（会话 / 今日 / 本月）。
#[derive(Debug, Default, Clone, Serialize)]
pub struct UsageTotals {
    pub calls: i64,
    #[serde(rename = "hitTokens")]
    pub hit_tokens: i64,
    #[serde(rename = "missTokens")]
    pub miss_tokens: i64,
    #[serde(rename = "outTokens")]
    pub out_tokens: i64,
    pub cost: f64,
    #[serde(rename = "cacheSaved")]
    pub cache_saved: f64,
    #[serde(rename = "offPeakSaving")]
    pub off_peak_saving: f64,
}

#[derive(Debug, Serialize)]
pub struct PeakNow {
    pub peak: bool,
    pub label: String,
    #[serde(rename = "nextLabel")]
    pub next_label: String,
    pub until: i64,
    pub since: i64,
}

#[derive(Debug, Serialize)]
pub struct UsageResponse {
    pub now: PeakNow,
    pub session: UsageTotals,
    pub today: UsageTotals,
    pub month: UsageTotals,
    pub currency: String,
}

/// 统计用量。遍历所有活跃会话的助手消息，按 usage 字段累加费用。
///
/// 与 server.py 的 app_usage 逐字段对齐：
///   hit  = cache_read_input_tokens
///   miss = input_tokens + cache_creation_input_tokens
///   out  = output_tokens
///   cost = (hit*ph + miss*pm + out*po) * 峰谷系数 / 1e6
pub fn usage(store: &Store, session_id: &str) -> UsageResponse {
    let now = now_ms();
    let today_key = billing::bj_date_key(now);
    let month_key = billing::bj_month_key(now);

    let mut acc_session = UsageTotals::default();
    let mut acc_today = UsageTotals::default();
    let mut acc_month = UsageTotals::default();

    for meta in store.summaries().unwrap_or_default() {
        let Some(raw) = store.load_session(&meta.id).ok().flatten() else {
            continue;
        };
        let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&raw) else {
            continue;
        };
        let Some(msgs) = doc.get("messages").and_then(|m| m.as_array()) else {
            continue;
        };
        for msg in msgs {
            if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
                continue;
            }
            let Some(u) = msg.get("usage") else { continue };
            let hit = u
                .get("cache_read_input_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let miss = u.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0)
                + u.get("cache_creation_input_tokens")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
            let out = u.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
            if hit + miss + out == 0 {
                continue;
            }
            let model = msg
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("deepseek-flash");
            let ts = msg
                .get("createdAt")
                .and_then(|v| v.as_i64())
                .unwrap_or(now);
            let cost = billing::cost_of(model, ts, hit, miss, out);
            let factor = if billing::is_peak(ts) { 1.0 } else { 0.5 };
            // 只需要「命中」与「未命中」两个单价来算缓存省下多少；
            // 输出单价不参与，不要解构它（之前多写了一个 _ 导致元组长度不符）
            let (ph, pm) = match model {
                "deepseek-v4-pro" => (0.30, 9.00),
                _ => (0.04, 2.00),
            };
            let cache_saved = hit as f64 * (pm - ph) * factor / 1e6;
            let off_peak_saving = (cost - billing::offpeak_cost(model, hit, miss, out)).max(0.0);

            let ts_date = billing::bj_date_key(ts);
            let ts_month = billing::bj_month_key(ts);
            for (acc, on) in [
                (&mut acc_session, doc.get("id").and_then(|v| v.as_str()) == Some(session_id)),
                (&mut acc_today, ts_date == today_key),
                (&mut acc_month, ts_month == month_key),
            ] {
                if !on {
                    continue;
                }
                acc.calls += 1;
                acc.hit_tokens += hit;
                acc.miss_tokens += miss;
                acc.out_tokens += out;
                acc.cost += cost;
                acc.cache_saved += cache_saved;
                acc.off_peak_saving += off_peak_saving;
            }
        }
    }

    let peak = billing::is_peak(now);
    UsageResponse {
        now: PeakNow {
            peak,
            label: if peak { "峰时" } else { "闲时" }.into(),
            next_label: if peak { "闲时" } else { "峰时" }.into(),
            until: billing::next_boundary(now),
            since: billing::prev_boundary(now),
        },
        session: acc_session,
        today: acc_today,
        month: acc_month,
        currency: "CNY".into(),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub title: String,
    #[serde(rename = "messageId")]
    pub message_id: String,
    pub role: String,
    pub ts: i64,
    pub snippet: String,
    pub offset: usize,
    pub length: usize,
    pub archived: bool,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub query: String,
    pub hits: Vec<SearchHit>,
    pub scanned: usize,
    #[serde(rename = "tookMs")]
    pub took_ms: i64,
}

/// 按字符切出命中上下文（左右各 40 字）。
fn snippet_around(text: &str, char_index: usize, length: usize) -> (String, usize) {
    let chars: Vec<char> = text.chars().collect();
    let radius = 40;
    let start = char_index.saturating_sub(radius);
    let end = (char_index + length + radius).min(chars.len());
    let mut snippet: String = chars[start..end].iter().collect();
    let mut offset = char_index - start;
    if start > 0 {
        snippet.insert(0, '…');
        offset += 1;
    }
    if end < chars.len() {
        snippet.push('…');
    }
    (snippet, offset)
}

/// 从一条消息里取出可搜索的文本片段。
fn message_texts(msg: &serde_json::Value) -> Vec<String> {
    let mut parts = Vec::new();
    if let Some(c) = msg.get("content").and_then(|v| v.as_str()) {
        if !c.is_empty() {
            parts.push(c.to_string());
        }
    }
    if let Some(blocks) = msg.get("blocks").and_then(|v| v.as_array()) {
        for blk in blocks {
            if let Some(t) = blk.get("text").and_then(|v| v.as_str()) {
                parts.push(t.to_string());
            }
            if let Some(t) = blk.get("thinking").and_then(|v| v.as_str()) {
                parts.push(t.to_string());
            }
        }
    }
    parts
}

/// 全文搜索：活跃库 + 归档，线性扫描。
///
/// 与 Python 版一致：每条消息只报一次命中（取第一个命中的片段）。
pub fn search(store: &Store, query: &str, limit: usize) -> SearchResponse {
    let started = std::time::Instant::now();
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return SearchResponse {
            query: query.to_string(),
            hits: Vec::new(),
            scanned: 0,
            took_ms: 0,
        };
    }
    let limit = limit.clamp(1, 500);
    let mut hits = Vec::new();
    let mut scanned = 0usize;

    let mut scan_doc = |doc: &serde_json::Value, archived: bool| {
        scanned += 1;
        let sid = doc.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let title = doc.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let Some(msgs) = doc.get("messages").and_then(|m| m.as_array()) else {
            return;
        };
        for msg in msgs {
            let mid = msg.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
            let ts = msg.get("createdAt").and_then(|v| v.as_i64()).unwrap_or(0);
            for text in message_texts(msg) {
                // Python 用 str.lower().find()，这里同样先转小写再找；
                // 字符下标用 chars 算，保证与 Python 的字符偏移一致。
                let lower: Vec<char> = text.chars().flat_map(|c| c.to_lowercase()).collect();
                let hay: String = lower.iter().collect();
                let Some(byte_idx) = hay.find(&needle) else {
                    continue;
                };
                // byte_idx 是字节偏移，换算成字符偏移
                let char_idx = hay[..byte_idx].chars().count();
                let (snippet, offset) = snippet_around(&text, char_idx, needle.chars().count());
                hits.push(SearchHit {
                    session_id: sid.to_string(),
                    title: title.to_string(),
                    message_id: mid.to_string(),
                    role: role.to_string(),
                    ts,
                    snippet,
                    offset,
                    length: needle.chars().count(),
                    archived,
                });
                break; // 同一条消息只报一次
            }
        }
    };

    for meta in store.summaries().unwrap_or_default() {
        if let Ok(Some(raw)) = store.load_session(&meta.id) {
            if let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&raw) {
                scan_doc(&doc, false);
            }
        }
    }

    // 归档：跳过已经在活跃库里的
    let active: HashSet<String> = store
        .summaries()
        .unwrap_or_default()
        .into_iter()
        .map(|m| m.id)
        .collect();
    if let Ok(entries) = fs::read_dir(store.dir().join("archive")) {
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
            .filter(|n| n.ends_with(".jsonl.gz"))
            .collect();
        names.sort();
        for name in names {
            for doc in read_archive_docs(store, &name) {
                let sid = doc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                if active.contains(sid) {
                    continue;
                }
                scan_doc(&doc, true);
            }
        }
    }

    hits.sort_by_key(|h| std::cmp::Reverse(h.ts));
    hits.truncate(limit);
    SearchResponse {
        query: query.to_string(),
        hits,
        scanned,
        took_ms: started.elapsed().as_millis() as i64,
    }
}
