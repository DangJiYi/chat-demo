//! app.log：把启动、同步、归档、HTTP 拒绝、前端上报的关键事件写进
//! `data/app.log`。
//!
//! 为什么需要它：Windows 版默认隐藏控制台（托盘图标代替黑框），安卓上没有
//! logcat。出问题时用户看不到任何输出，只能描述“不行”。日志留在数据目录里，
//! 界面上的诊断面板能直接读它。
//!
//! 超过 1 MB 就轮转一次，只留 `app.log.1`：日志是用来「看刚才发生了什么」的，
//! 不是归档，无限增长会挤占用户的存储配额。
//!
//! 两种入口：
//!   - `write(store, msg)` —— 手上有 Store 时用（启动、同步、归档）
//!   - `write_global(msg)` —— 没有 Store 时用（HTTP 4xx、前端上报）
//!
//! 后者依赖启动时 `init()` 设好的全局路径；没 init 就静默丢弃，
//! 绝不让日志反过来把主流程搞挂。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::billing;
use crate::store::{now_ms, Store};

/// 单个日志文件上限（1 MB）。
const MAX_LOG: u64 = 1 << 20;

/// 全局日志目录。`serve()` 启动时设一次。
static LOG_DIR: OnceLock<PathBuf> = OnceLock::new();

/// 记下日志目录，供 `write_global` / `tail` 使用。
pub fn init(dir: &Path) {
    let _ = LOG_DIR.set(dir.to_path_buf());
}

fn append(dir: &Path, msg: &str) {
    let path = dir.join("app.log");
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > MAX_LOG {
            let _ = std::fs::rename(&path, dir.join("app.log.1"));
        }
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{}] {msg}", billing::bj_time_key(now_ms()));
    }
}

/// 追加一行日志。写失败静默忽略——日志不该让主流程失败。
pub fn write(store: &Store, msg: &str) {
    append(store.dir(), msg);
}

/// 追加一行日志，用手上的全局目录（没有则丢弃）。
pub fn write_global(msg: &str) {
    if let Some(dir) = LOG_DIR.get() {
        append(dir, msg);
    }
}

/// 读最后 `max_lines` 行。文件不存在或读失败都返回空——调用方不需要区别对待。
pub fn tail_from(dir: &Path, max_lines: usize) -> Vec<String> {
    let path = dir.join("app.log");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].iter().map(|s| s.to_string()).collect()
}

/// 同上，用手上的全局目录。
pub fn tail(max_lines: usize) -> Vec<String> {
    match LOG_DIR.get() {
        Some(dir) => tail_from(dir, max_lines),
        None => Vec::new(),
    }
}
