//! 存储层：data/ 目录结构与会话摘要。
//!
//! 目录结构必须与 Python 版一致，这样两边可以读同一份数据：
//!   data/sessions/<id>.json     会话全文
//!   data/media/<sha256>         附件字节
//!   data/archive/*.jsonl.gz     归档
//!   data/meta/index.json        索引（可删，会重建）

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// 会话摘要：与 server.py `parse_session()` 产出的 meta 字段一致。
/// 字段名用 camelCase，前端直接消费。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(rename = "createdAt", default)]
    pub created_at: i64,
    #[serde(rename = "updatedAt", default)]
    pub updated_at: i64,
}

/// 会话全文里我们只需要这几个字段。
/// 不定义完整结构：多出来的字段（messages 等）直接忽略，
/// 将来 Python 版加字段也不会让 Rust 解析失败。
#[derive(Debug, Deserialize)]
struct SessionDoc {
    id: String,
    #[serde(default)]
    title: String,
    #[serde(rename = "createdAt", default)]
    created_at: i64,
    #[serde(rename = "updatedAt", default)]
    updated_at: i64,
    #[serde(default)]
    deleted: bool,
}

pub struct Store {
    dir: PathBuf,
    port: u16,
    started: Instant,
}

impl Store {
    /// 建好目录结构。已存在则什么也不做（与 Python 版一致）。
    pub fn open(data_dir: &str, port: u16) -> io::Result<Self> {
        let dir = PathBuf::from(data_dir);
        for sub in [
            "sessions",
            "meta",
            "media",
            "staged/sessions",
            "staged/files",
            "archive",
        ] {
            fs::create_dir_all(dir.join(sub))?;
        }
        Ok(Store {
            dir,
            port,
            started: Instant::now(),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn uptime_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    /// 列出会话摘要，按 updatedAt 倒序。
    ///
    /// 直接扫目录而不是读 index.json：
    ///   - index.json 是缓存，可能不存在或过期
    ///   - 个人规模几百个文件，扫一遍是毫秒级
    ///   - 少一处「索引与文件不一致」的故障面
    /// 单个文件解析失败不影响整体——与 Python 版的容错行为一致。
    pub fn summaries(&self) -> io::Result<Vec<SessionSummary>> {
        let mut out = Vec::new();
        let dir = self.dir.join("sessions");
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e),
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let raw = match fs::read(&path) {
                Ok(b) => b,
                Err(_) => continue, // 读不到就跳过，不让一个坏文件拖垮整个列表
            };
            let doc: SessionDoc = match serde_json::from_slice(&raw) {
                Ok(d) => d,
                Err(_) => continue, // 半写入/损坏的 JSON 同理
            };
            if doc.deleted {
                continue; // 墓碑不出现在列表里
            }
            out.push(SessionSummary {
                id: doc.id,
                title: doc.title,
                created_at: doc.created_at,
                updated_at: doc.updated_at,
            });
        }

        // 按 updatedAt 倒序；相同时间戳时按 id 排，保证输出稳定
        out.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(out)
    }
}
