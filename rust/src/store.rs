//! 存储层：data/ 目录结构、会话读写、附件索引。
//!
//! 目录结构与 Python 版完全一致，两边能读同一份数据：
//!   data/sessions/<id>.json     会话全文
//!   data/media/<sha256>         附件字节（内容寻址）
//!   data/archive/*.jsonl.gz     归档
//!   data/meta/index.json        索引（可删，会重建）

use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 单个会话文件的大小上限，与 Python 版一致（32 MB）。
const MAX_SESSION: u64 = 32 << 20;
/// 单个附件上限，与 Python 版 MAX_FILE 一致（128 MB）。
const MAX_FILE: u64 = 128 << 20;

/// 附件元数据，字段名与 Python 版 index.json 里的 files[fid] 一致。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMeta {
    pub id: String,
    pub sha: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub size: u64,
    #[serde(rename = "mime", default)]
    pub mime: String,
    #[serde(rename = "isImage", default)]
    pub is_image: bool,
    #[serde(rename = "createdAt", default)]
    pub created_at: i64,
}

/// 内容寻址：hex(sha256(data))。与 Python 版 hashlib.sha256().hexdigest() 一致。
pub fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 会话摘要：字段名用 camelCase，前端直接消费。
/// 与 server.py `parse_session()` 产出的 meta 一致。
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

/// 会话全文里我们关心的字段。
///
/// 不定义完整结构：messages 里那些复杂内容（blocks / usage / 分支）直接忽略，
/// Python 版将来加字段也不会让 Rust 解析失败。只保留判断用得到的部分。
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
    #[serde(default)]
    messages: Vec<MessageDoc>,
}

#[derive(Debug, Deserialize)]
struct MessageDoc {
    #[serde(default)]
    role: String,
    #[serde(default)]
    files: Vec<FileRef>,
}

#[derive(Debug, Deserialize)]
struct FileRef {
    #[serde(default)]
    id: String,
}

/// parse_session 的结果：摘要 + 该会话引用的附件 id。
struct Parsed {
    summary: SessionSummary,
    /// 该会话引用的附件 id（按出现顺序去重）。
    ///
    /// 目前还没消费方：附件 id → sha 的映射在 index.json 里，
    /// 等实现 /db/file 时一并接上。在此之前保留字段不漏算。
    #[allow(dead_code)]
    file_ids: Vec<String>,
    deleted: bool,
}

/// 解析会话 JSON。与 Python 版一致：解析失败返回 None，不抛错。
fn parse_session(raw: &[u8]) -> Option<Parsed> {
    let doc: SessionDoc = serde_json::from_slice(raw).ok()?;
    if doc.id.is_empty() {
        return None;
    }
    // 按出现顺序去重，与 Python 版 file_ids 的构造方式一致
    let mut seen = HashSet::new();
    let mut file_ids = Vec::new();
    for msg in &doc.messages {
        if msg.role != "user" {
            continue;
        }
        for f in &msg.files {
            if !f.id.is_empty() && seen.insert(f.id.clone()) {
                file_ids.push(f.id.clone());
            }
        }
    }
    Some(Parsed {
        summary: SessionSummary {
            id: doc.id,
            title: doc.title,
            created_at: doc.created_at,
            updated_at: doc.updated_at,
        },
        file_ids,
        deleted: doc.deleted,
    })
}

/// 原子写入：先写 .tmp 再 rename，避免断电/崩溃留下半个文件。
/// 与 Python 版 atomic_write 语义一致。
pub fn atomic_write(path: &Path, data: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    // std::fs::rename 在 Windows 上用 MOVEFILE_REPLACE_EXISTING，会覆盖已存在的目标
    fs::rename(&tmp, path)
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

    fn session_path(&self, sid: &str) -> PathBuf {
        self.dir.join("sessions").join(format!("{sid}.json"))
    }

    /// 列出会话摘要，按 updatedAt 倒序。
    ///
    /// 直接扫目录而不是读 index.json：
    ///   - index.json 是缓存，可能不存在或过期
    ///   - 个人规模几百个文件，扫一遍是毫秒级
    ///   - 少一处「索引与文件不一致」的故障面
    ///
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
            let parsed = match parse_session(&raw) {
                Some(p) => p,
                None => continue, // 半写入/损坏的 JSON 同理
            };
            if parsed.deleted {
                continue; // 墓碑不出现在列表里
            }
            out.push(parsed.summary);
        }

        // 按 updatedAt 倒序；相同时间戳时按 id 排，保证输出稳定
        out.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(out)
    }

    /// 读会话原文。不存在返回 Ok(None)。
    pub fn load_session(&self, sid: &str) -> io::Result<Option<Vec<u8>>> {
        match fs::read(self.session_path(sid)) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// 保存会话。校验：JSON 可解析、id 与路径一致（与 Python 版一致）。
    ///
    /// 返回 Err 里的字符串直接回给前端，所以要写清原因。
    pub fn save_session(&self, sid: &str, raw: &[u8]) -> Result<(), String> {
        if raw.len() as u64 > MAX_SESSION {
            return Err("会话过大".into());
        }
        let parsed = parse_session(raw).ok_or_else(|| "会话 JSON 无法解析".to_string())?;
        if parsed.summary.id != sid {
            return Err(format!(
                "会话 id 不一致：路径是 {sid}，文档里是 {}",
                parsed.summary.id
            ));
        }
        atomic_write(&self.session_path(sid), raw).map_err(|e| format!("写入失败: {e}"))
    }

    /// 改标题。保留原文其余字段（只替换 title）。
    pub fn rename_session(&self, sid: &str, title: &str) -> Result<(), String> {
        let raw = match self.load_session(sid).map_err(|e| e.to_string())? {
            Some(r) => r,
            None => return Err("not found".into()),
        };
        let mut doc: serde_json::Value =
            serde_json::from_slice(&raw).map_err(|e| format!("解析失败: {e}"))?;
        if let Some(obj) = doc.as_object_mut() {
            obj.insert("title".into(), serde_json::Value::String(title.into()));
        }
        let out = serde_json::to_vec(&doc).map_err(|e| format!("序列化失败: {e}"))?;
        atomic_write(&self.session_path(sid), &out).map_err(|e| format!("写入失败: {e}"))
    }

    /// 删除会话：写墓碑（不删文件），并清理不再被引用的附件原件。
    ///
    /// 与 Python 版一致——墓碑是为了让同步知道「这条被删了」。
    pub fn delete_session(&self, sid: &str) -> Result<(), String> {
        let raw = match self.load_session(sid).map_err(|e| e.to_string())? {
            Some(r) => r,
            None => return Ok(()), // 不存在就当成功，幂等
        };
        let mut doc: serde_json::Value =
            serde_json::from_slice(&raw).map_err(|e| format!("解析失败: {e}"))?;
        let obj = doc
            .as_object_mut()
            .ok_or_else(|| "会话不是 JSON 对象".to_string())?;
        obj.insert("deleted".into(), serde_json::Value::Bool(true));
        obj.insert("updatedAt".into(), serde_json::Value::from(now_ms()));
        let out = serde_json::to_vec(&doc).map_err(|e| format!("序列化失败: {e}"))?;
        atomic_write(&self.session_path(sid), &out).map_err(|e| format!("写入失败: {e}"))?;

        // 附件清理暂不做：附件 id 到 sha 的映射存在 index.json 里，
        // 而 index.json 是缓存。等实现 /db/file 时一并把索引逻辑搬过来再清，
        // 否则会误删「其它会话还在用」的原件。宁可留垃圾，不误删数据。
        Ok(())
    }

    /// 各子目录占用，给 /local/storage 用。
    pub fn subdir_bytes(&self, name: &str) -> u64 {
        dir_size(&self.dir.join(name))
    }

    // ---------------------------------------------------------------- 附件

    /// 读文件索引（data/meta/index.json 里的 files 字段）。
    ///
    /// 刻意与 Python 共用同一份 index.json：Rust 只改 files 键、保留其余，
    /// 这样两个版本能读同一个 data/ 目录，迁移期间可以对照。
    fn files_index_path(&self) -> PathBuf {
        self.dir.join("meta").join("index.json")
    }

    fn load_files_index(&self) -> serde_json::Map<String, serde_json::Value> {
        let raw = match fs::read(self.files_index_path()) {
            Ok(r) => r,
            Err(_) => return serde_json::Map::new(),
        };
        let doc: serde_json::Value = match serde_json::from_slice(&raw) {
            Ok(d) => d,
            Err(_) => return serde_json::Map::new(),
        };
        doc.get("files")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default()
    }

    /// 只覆盖 index.json 的 files 键，保留 sessions / archive 等。
    fn save_files_index(
        &self,
        files: &serde_json::Map<String, serde_json::Value>,
    ) -> io::Result<()> {
        let path = self.files_index_path();
        let mut doc: serde_json::Value = match fs::read(&path) {
            Ok(raw) => serde_json::from_slice(&raw).unwrap_or_else(|_| serde_json::json!({})),
            Err(_) => serde_json::json!({}),
        };
        if !doc.is_object() {
            doc = serde_json::json!({});
        }
        if let Some(obj) = doc.as_object_mut() {
            obj.insert("files".into(), serde_json::Value::Object(files.clone()));
        }
        let out = serde_json::to_vec_pretty(&doc).map_err(io::Error::other)?;
        atomic_write(&path, &out)
    }

    /// 存附件：按内容寻址写 media/<sha256>，再记录 id → 元数据。
    pub fn put_file(
        &self,
        fid: &str,
        data: &[u8],
        mime: &str,
        name: &str,
        image: bool,
    ) -> Result<FileMeta, String> {
        if data.len() as u64 > MAX_FILE {
            return Err("附件过大".into());
        }
        let digest = sha256_hex(data);
        atomic_write(&self.dir.join("media").join(&digest), data)
            .map_err(|e| format!("写入失败: {e}"))?;
        let meta = FileMeta {
            id: fid.to_string(),
            sha: digest,
            name: name.to_string(),
            size: data.len() as u64,
            mime: mime.to_string(),
            is_image: image,
            created_at: now_ms(),
        };
        let mut files = self.load_files_index();
        files.insert(
            fid.to_string(),
            serde_json::to_value(&meta).map_err(|e| e.to_string())?,
        );
        self.save_files_index(&files).map_err(|e| e.to_string())?;
        Ok(meta)
    }

    /// 读附件：返回 (元数据, 字节)。
    pub fn get_file(&self, fid: &str) -> Result<Option<(FileMeta, Vec<u8>)>, String> {
        let files = self.load_files_index();
        let Some(raw) = files.get(fid) else {
            return Ok(None);
        };
        let meta: FileMeta =
            serde_json::from_value(raw.clone()).map_err(|e| format!("附件元数据损坏: {e}"))?;
        match fs::read(self.dir.join("media").join(&meta.sha)) {
            Ok(data) => Ok(Some((meta, data))),
            // 元数据在但原件不在：报 404（与 Python 版返回 (fm, None) 的语义一致）
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("读取失败: {e}")),
        }
    }

    // ---------------------------------------------------------------- 导入暂存

    fn staged_sessions_dir(&self) -> PathBuf {
        self.dir.join("staged").join("sessions")
    }

    fn staged_files_dir(&self) -> PathBuf {
        self.dir.join("staged").join("files")
    }

    fn staged_files_meta_path(&self) -> PathBuf {
        self.dir.join("staged").join("files.json")
    }

    /// 清空暂存。导入开始前清残留，导入失败后清本次写入。
    pub fn clear_staging(&self) -> Result<(), String> {
        for dir in [self.staged_sessions_dir(), self.staged_files_dir()] {
            if let Ok(entries) = fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
        let _ = fs::remove_file(self.staged_files_meta_path());
        Ok(())
    }

    /// 暂存一条会话。不校验 id 与文件名是否一致：导入的数据可能不规范，
    /// 真正的校验留到 commit 时做。
    pub fn add_staging_session(&self, sid: &str, raw: &[u8]) -> Result<(), String> {
        atomic_write(&self.staged_sessions_dir().join(format!("{sid}.json")), raw)
            .map_err(|e| format!("写入暂存失败: {e}"))
    }

    /// 暂存一个附件原件。
    pub fn add_staging_file(
        &self,
        fid: &str,
        data: &[u8],
        mime: &str,
        image: bool,
    ) -> Result<(), String> {
        if data.len() as u64 > MAX_FILE {
            return Err("附件过大".into());
        }
        atomic_write(&self.staged_files_dir().join(fid), data)
            .map_err(|e| format!("写入暂存失败: {e}"))?;
        let mut metas: serde_json::Map<String, serde_json::Value> =
            match fs::read(self.staged_files_meta_path()) {
                Ok(raw) => serde_json::from_slice(&raw).unwrap_or_default(),
                Err(_) => serde_json::Map::new(),
            };
        // 保留 MIME 与图片标记：导入的图片后续要能被“压缩图片”识别
        metas.insert(
            fid.to_string(),
            serde_json::json!({ "id": fid, "size": data.len(), "mime": mime, "isImage": image }),
        );
        let out =
            serde_json::to_vec(&serde_json::Value::Object(metas)).map_err(|e| e.to_string())?;
        atomic_write(&self.staged_files_meta_path(), &out).map_err(|e| format!("写入失败: {e}"))
    }

    /// 回填暂存会话里的附件 MIME（图片字节在会话 JSON 之后才读到）。
    pub fn update_staging_types(
        &self,
        types: &std::collections::HashMap<String, String>,
    ) -> Result<(), String> {
        let entries = match fs::read_dir(self.staged_sessions_dir()) {
            Ok(e) => e,
            Err(_) => return Ok(()),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let raw = match fs::read(&path) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let mut doc: serde_json::Value = match serde_json::from_slice(&raw) {
                Ok(d) => d,
                Err(_) => continue,
            };
            let mut changed = false;
            if let Some(msgs) = doc.get_mut("messages").and_then(|m| m.as_array_mut()) {
                for msg in msgs.iter_mut() {
                    if msg.get("role").and_then(|r| r.as_str()) != Some("user") {
                        continue;
                    }
                    if let Some(files) = msg.get_mut("files").and_then(|f| f.as_array_mut()) {
                        for f in files.iter_mut() {
                            let Some(fid) = f.get("id").and_then(|v| v.as_str()) else {
                                continue;
                            };
                            if let Some(want) = types.get(fid) {
                                if f.get("mimeType").and_then(|v| v.as_str()) != Some(want) {
                                    f["mimeType"] = serde_json::Value::String(want.clone());
                                    changed = true;
                                }
                            }
                        }
                    }
                }
            }
            if changed {
                let out = serde_json::to_vec(&doc).map_err(|e| e.to_string())?;
                atomic_write(&path, &out).map_err(|e| format!("写入失败: {e}"))?;
            }
        }
        Ok(())
    }

    /// 用暂存替换正式数据，返回被提交的会话摘要列表。
    ///
    /// 与 Python 版的差别（刻意的）：Python 先删光 sessions 再写，中途失败数据就没了。
    /// 这里先把暂存会话全部写入 sessions，最后才删「不属于新集合」的旧文件。
    /// 要么新数据已完整落盘，要么旧数据还在，不会出现半截状态。
    pub fn commit_staging(&self) -> Result<Vec<SessionSummary>, String> {
        let mut accepted = Vec::new();
        let mut keep: HashSet<String> = HashSet::new();

        let entries = match fs::read_dir(self.staged_sessions_dir()) {
            Ok(e) => e,
            Err(_) => return Ok(accepted), // 没有暂存：什么都不做
        };
        // 排序保证结果稳定（与 Python 版 sorted(os.listdir()) 一致）
        let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();

        for path in paths {
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let raw = match fs::read(&path) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let Some(parsed) = parse_session(&raw) else {
                continue; // 解析不了就跳过，不把坏数据写进正式库
            };
            let sid = parsed.summary.id.clone();
            atomic_write(&self.session_path(&sid), &raw).map_err(|e| format!("写入失败: {e}"))?;
            keep.insert(sid);
            accepted.push(parsed.summary);
        }

        // 附件：从暂存搬到 media，按内容寻址记录
        let mut files: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
        if let Ok(raw) = fs::read(self.staged_files_meta_path()) {
            if let Ok(serde_json::Value::Object(metas)) = serde_json::from_slice(&raw) {
                for (fid, meta) in metas {
                    let data = match fs::read(self.staged_files_dir().join(&fid)) {
                        Ok(d) => d,
                        Err(_) => continue,
                    };
                    let digest = sha256_hex(&data);
                    atomic_write(&self.dir.join("media").join(&digest), &data)
                        .map_err(|e| format!("写入失败: {e}"))?;
                    let mime = meta
                        .get("mime")
                        .and_then(|v| v.as_str())
                        .unwrap_or("application/octet-stream")
                        .to_string();
                    let is_image = meta
                        .get("isImage")
                        .and_then(|v| v.as_bool())
                        .unwrap_or_else(|| mime.starts_with("image/"));
                    let record = FileMeta {
                        id: fid.clone(),
                        sha: digest,
                        name: meta
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        size: data.len() as u64,
                        mime,
                        is_image,
                        created_at: now_ms(),
                    };
                    files.insert(
                        fid,
                        serde_json::to_value(&record).map_err(|e| e.to_string())?,
                    );
                }
            }
        }

        // 新数据全部落盘之后，才删旧会话文件
        if let Ok(entries) = fs::read_dir(self.dir.join("sessions")) {
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                if !keep.contains(stem) {
                    let _ = fs::remove_file(&path);
                }
            }
        }

        self.save_files_index(&files).map_err(|e| e.to_string())?;
        self.clear_staging()?;
        Ok(accepted)
    }
}

/// 递归统计目录占用。
pub fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    let entries = match fs::read_dir(path) {
        Ok(e) => e,
        Err(_) => return 0,
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            total += dir_size(&p);
        } else if let Ok(m) = entry.metadata() {
            total += m.len();
        }
    }
    total
}
