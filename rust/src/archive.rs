//! 归档、图片重压缩、孤儿附件统计。
//!
//! 归档文件是 `.jsonl.gz`，每行一个 `{"archivedAt":..,"session":{..}}`。
//! 与 Python 版一致：追加写（多个 gzip member 串联），读取时 MultiGzDecoder 自动接续。

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};

use serde::{Deserialize, Serialize};

use crate::billing;
use crate::store::{atomic_write, now_ms, Store};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchivedMeta {
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

fn write_index(store: &Store, idx: &serde_json::Value) -> Result<(), String> {
    let out = serde_json::to_vec(idx).map_err(|e| e.to_string())?;
    atomic_write(&store.dir().join("meta").join("index.json"), &out)
        .map_err(|e| format!("写入索引失败: {e}"))
}

/// 读一个归档文件里的全部会话文档。
pub fn read_archive_docs(store: &Store, name: &str) -> Vec<(i64, serde_json::Value)> {
    let path = store.dir().join("archive").join(name);
    let Ok(file) = fs::File::open(&path) else {
        return Vec::new();
    };
    let mut decoder = flate2::read::MultiGzDecoder::new(file);
    let mut text = String::new();
    if decoder.read_to_string(&mut text).is_err() {
        return Vec::new();
    }
    text.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|env| {
            let at = env.get("archivedAt").and_then(|v| v.as_i64()).unwrap_or(0);
            env.get("session").cloned().map(|doc| (at, doc))
        })
        .collect()
}

/// 把超期会话搬进归档。返回搬走的条数。
///
/// 跳过还没定稿的会话：状态里有 pending/streaming 就不动，
/// 否则会把半截回答归档走，前端再打开就看不到了。
pub fn archive_once(store: &Store) -> Result<usize, String> {
    let cfg = store.config();
    let days = cfg
        .get("archiveDays")
        .and_then(|v| v.as_i64())
        .unwrap_or(7)
        .max(1);
    let cutoff = now_ms() - days * 24 * 3600 * 1000;

    let mut by_month: HashMap<String, Vec<(serde_json::Value, String)>> = HashMap::new();
    for meta in store.summaries().unwrap_or_default() {
        if meta.updated_at > cutoff {
            continue;
        }
        let Some(raw) = store.load_session(&meta.id).ok().flatten() else {
            continue;
        };
        let text = String::from_utf8_lossy(&raw);
        if text.contains("\"status\":\"pending\"") || text.contains("\"status\":\"streaming\"") {
            continue;
        }
        let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&raw) else {
            continue;
        };
        let month = billing::bj_month_key(meta.updated_at);
        by_month
            .entry(month)
            .or_default()
            .push((doc, meta.title.clone()));
    }
    if by_month.is_empty() {
        return Ok(0);
    }

    let mut total = 0usize;
    let mut moved: Vec<String> = Vec::new();
    let mut idx = read_index(store);
    if !idx.is_object() {
        idx = serde_json::json!({});
    }
    let mut archive_arr: Vec<serde_json::Value> = idx
        .get("archive")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    for (month, items) in by_month {
        let gz_path = store
            .dir()
            .join("archive")
            .join(format!("archive_{month}.jsonl.gz"));
        let md_path = store
            .dir()
            .join("archive")
            .join(format!("archive_{month}.md"));

        let mut lines = String::new();
        let mut md = String::new();
        let mut metas: Vec<ArchivedMeta> = Vec::new();

        for (doc, title) in &items {
            let Some(sid) = doc.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            let envelope = serde_json::json!({ "archivedAt": now_ms(), "session": doc });
            lines.push_str(&envelope.to_string());
            lines.push('\n');

            let msgs = doc
                .get("messages")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let last_user = msgs
                .iter()
                .rev()
                .find_map(|m| {
                    if m.get("role").and_then(|v| v.as_str()) != Some("user") {
                        return None;
                    }
                    m.get("content")
                        .and_then(|v| v.as_str())
                        .map(|c| c.replace('\n', " ").chars().take(160).collect::<String>())
                })
                .unwrap_or_default();
            let updated = doc.get("updatedAt").and_then(|v| v.as_i64()).unwrap_or(0);
            md.push_str(&format!(
                "## {title}\n\n- id: `{sid}`\n- 最后活动: {}\n- 消息数: {}\n- 最后提问: {last_user}\n\n",
                billing::bj_date_key(updated),
                msgs.len()
            ));
            metas.push(ArchivedMeta {
                id: sid.to_string(),
                month: month.clone(),
                title: title.clone(),
                updated_at: updated,
                msg_count: msgs.len() as i64,
            });
        }

        // 追加一个 gzip member（与 Python 版的 "ab" + gzip.compress 一致）
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&gz_path)
            .map_err(|e| format!("打开归档失败: {e}"))?;
        {
            let mut enc = flate2::write::GzEncoder::new(&mut file, flate2::Compression::default());
            enc.write_all(lines.as_bytes())
                .map_err(|e| format!("写归档失败: {e}"))?;
            enc.finish().map_err(|e| format!("写归档失败: {e}"))?;
        }

        let mut md_file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&md_path)
            .map_err(|e| format!("打开摘要失败: {e}"))?;
        md_file
            .write_all(md.as_bytes())
            .map_err(|e| format!("写摘要失败: {e}"))?;

        // 先进索引，**先不删**活跃文件：
        // 反过来的话，一旦索引写失败，就会出现「会话文件没了、归档列表里也没有」，
        // 数据只能靠解压 .jsonl.gz 手救。
        for m in &metas {
            archive_arr.retain(|a| a.get("id").and_then(|v| v.as_str()) != Some(m.id.as_str()));
            archive_arr.push(serde_json::to_value(m).map_err(|e| e.to_string())?);
        }
        total += metas.len();
        moved.extend(metas.into_iter().map(|m| m.id));
    }

    if let Some(obj) = idx.as_object_mut() {
        obj.insert("archive".to_string(), serde_json::Value::Array(archive_arr));
    }
    write_index(store, &idx)?;

    // 索引落盘成功之后，才把已归档的会话从活跃目录移走。
    // 再万一删不掉，也只是多一份重复（/local/archive/now 会去重），不会丢数据。
    for sid in &moved {
        let _ = fs::remove_file(store.dir().join("sessions").join(format!("{sid}.json")));
    }
    Ok(total)
}

/// 归档去重：同一会话只保留 archivedAt 最新的一份。
///
/// 只删「同一会话的旧副本」，不会丢掉任何会话，也不动附件原件。
/// 返回 (丢掉的副本数, 省下的字节数)。
pub fn compact_archive(store: &Store) -> Result<(usize, u64), String> {
    let dir = store.dir().join("archive");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok((0, 0));
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
        .filter(|n| n.ends_with(".jsonl.gz"))
        .collect();
    names.sort();

    let mut dropped_total = 0usize;
    let mut saved_bytes = 0u64;
    for name in names {
        let docs = read_archive_docs(store, &name);
        let original = docs.len();
        let mut latest: HashMap<String, (i64, serde_json::Value)> = HashMap::new();
        for (at, doc) in docs {
            let Some(sid) = doc.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            let keep = match latest.get(sid) {
                Some((prev, _)) => at >= *prev,
                None => true,
            };
            if keep {
                latest.insert(sid.to_string(), (at, doc));
            }
        }
        if latest.is_empty() || original <= latest.len() {
            continue; // 没有重复，不动它
        }

        let mut lines = String::new();
        let mut md = String::new();
        for (sid, (at, doc)) in &latest {
            let envelope = serde_json::json!({ "archivedAt": at, "session": doc });
            lines.push_str(&envelope.to_string());
            lines.push('\n');
            let msgs = doc
                .get("messages")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let title = doc.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let updated = doc.get("updatedAt").and_then(|v| v.as_i64()).unwrap_or(*at);
            md.push_str(&format!(
                "## {title}\n\n- id: `{sid}`\n- 最后活动: {}\n- 消息数: {msgs}\n\n",
                billing::bj_date_key(updated)
            ));
        }

        let gz_path = dir.join(&name);
        let before = fs::metadata(&gz_path).map(|m| m.len()).unwrap_or(0);
        let mut buf = Vec::new();
        {
            let mut enc = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
            enc.write_all(lines.as_bytes())
                .map_err(|e| format!("压缩失败: {e}"))?;
            enc.finish().map_err(|e| format!("压缩失败: {e}"))?;
        }
        atomic_write(&gz_path, &buf).map_err(|e| format!("写归档失败: {e}"))?;
        let after = fs::metadata(&gz_path).map(|m| m.len()).unwrap_or(0);
        saved_bytes += before.saturating_sub(after);
        let md_name = name.trim_end_matches(".gz").trim_end_matches(".jsonl").to_string();
        atomic_write(&dir.join(format!("{md_name}.md")), md.as_bytes())
            .map_err(|e| format!("写摘要失败: {e}"))?;
        dropped_total += original - latest.len();
    }
    Ok((dropped_total, saved_bytes))
}

/// 重新压缩 PNG 的 IDAT（像素完全不变）。返回 None 表示放弃（不是 PNG 或解析失败）。
fn recompress_png(data: &[u8]) -> Option<Vec<u8>> {
    if !data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return None;
    }
    let mut out: Vec<u8> = data[..8].to_vec();
    let mut pos = 8usize;
    while pos + 8 <= data.len() {
        if pos + 12 > data.len() {
            return None;
        }
        let len = u32::from_be_bytes([
            data[pos],
            data[pos + 1],
            data[pos + 2],
            data[pos + 3],
        ]) as usize;
        if pos + 12 + len > data.len() {
            return None;
        }
        let ctype = &data[pos + 4..pos + 8];
        let payload = &data[pos + 8..pos + 8 + len];
        pos += 12 + len;

        let new_payload: Vec<u8> = if ctype == b"IDAT" {
            let mut raw = Vec::new();
            if flate2::read::ZlibDecoder::new(payload)
                .read_to_end(&mut raw)
                .is_err()
            {
                return None;
            }
            let mut packed = Vec::new();
            {
                let mut enc =
                    flate2::write::ZlibEncoder::new(&mut packed, flate2::Compression::best());
                if enc.write_all(&raw).is_err() {
                    return None;
                }
                if enc.finish().is_err() {
                    return None;
                }
            }
            packed
        } else {
            payload.to_vec()
        };

        out.extend_from_slice(&(new_payload.len() as u32).to_be_bytes());
        out.extend_from_slice(ctype);
        out.extend_from_slice(&new_payload);
        let mut crc_input = ctype.to_vec();
        crc_input.extend_from_slice(&new_payload);
        out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
    }
    Some(out)
}

/// 标准 CRC-32（PNG 用的那个多项式）。
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for byte in data {
        crc ^= *byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// 重压缩超期图片。只处理 PNG；JPEG 没有编码器，原样保留。
#[derive(Debug, Default, Serialize)]
pub struct CompactResult {
    pub scanned: i64,
    pub shrunk: i64,
    #[serde(rename = "savedBytes")]
    pub saved_bytes: i64,
    pub skipped: i64,
}

pub fn compact_images(store: &Store) -> Result<CompactResult, String> {
    let cfg = store.config();
    let days = cfg.get("imageDays").and_then(|v| v.as_i64()).unwrap_or(14);
    let cutoff = if days > 0 {
        now_ms() - days * 24 * 3600 * 1000
    } else {
        now_ms()
    };

    let mut out = CompactResult::default();
    let files = store.file_metas();
    for fm in files {
        if !fm.is_image || fm.created_at > cutoff {
            continue;
        }
        out.scanned += 1;
        let path = store.dir().join("media").join(&fm.sha);
        let Ok(data) = fs::read(&path) else {
            out.skipped += 1;
            continue;
        };
        let Some(new) = recompress_png(&data) else {
            out.skipped += 1;
            continue;
        };
        if new.len() >= data.len() {
            out.skipped += 1;
            continue;
        }
        let saved = data.len() - new.len();
        atomic_write(&path, &new).map_err(|e| format!("写入失败: {e}"))?;
        out.shrunk += 1;
        out.saved_bytes += saved as i64;
    }
    Ok(out)
}

/// 统计没有任何会话引用的附件（只统计，不删除）。
#[derive(Debug, Default, Serialize)]
pub struct OrphanResult {
    pub count: i64,
    pub bytes: i64,
}

pub fn orphan_media(store: &Store) -> OrphanResult {
    let mut referenced: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut collect = |doc: &serde_json::Value| {
        let Some(msgs) = doc.get("messages").and_then(|v| v.as_array()) else {
            return;
        };
        for msg in msgs {
            let Some(files) = msg.get("files").and_then(|v| v.as_array()) else {
                continue;
            };
            for f in files {
                if let Some(id) = f.get("id").and_then(|v| v.as_str()) {
                    referenced.insert(id.to_string());
                }
            }
        }
    };

    for meta in store.summaries().unwrap_or_default() {
        if let Ok(Some(raw)) = store.load_session(&meta.id) {
            if let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&raw) {
                collect(&doc);
            }
        }
    }
    if let Ok(entries) = fs::read_dir(store.dir().join("archive")) {
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
            .filter(|n| n.ends_with(".jsonl.gz"))
            .collect();
        names.sort();
        for name in names {
            for (_, doc) in read_archive_docs(store, &name) {
                collect(&doc);
            }
        }
    }

    let mut count = 0i64;
    let mut bytes = 0i64;
    for fm in store.file_metas() {
        if referenced.contains(&fm.id) {
            continue;
        }
        // 另一个记录可能指向同一份原件（内容寻址），还有引用就不算孤儿
        if store.media_referenced_by_other(&fm.id, &fm.sha) {
            continue;
        }
        if let Ok(m) = fs::metadata(store.dir().join("media").join(&fm.sha)) {
            bytes += m.len() as i64;
            count += 1;
        }
    }
    OrphanResult { count, bytes }
}
