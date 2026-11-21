//! app.log：把启动、同步、归档的关键事件写进 `data/app.log`。
//!
//! 为什么需要它：Windows 版默认隐藏控制台（托盘图标代替黑框），
//! 出问题时用户看不到任何输出。日志留在数据目录里，界面上的
//! 「存储占用」也会显示它的大小，不需要额外的诊断工具。
//!
//! 超过 1 MB 就轮转一次，只留 `app.log.1`：日志是用来「看刚才发生了什么」的，
//! 不是归档，无限增长会挤占用户的存储配额。

use std::fs::OpenOptions;
use std::io::Write;

use crate::billing;
use crate::store::{now_ms, Store};

/// 单个日志文件上限（1 MB）。
const MAX_LOG: u64 = 1 << 20;

/// 追加一行日志。写失败静默忽略——日志不该让主流程失败。
pub fn write(store: &Store, msg: &str) {
    let path = store.dir().join("app.log");
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > MAX_LOG {
            let _ = std::fs::rename(&path, store.dir().join("app.log.1"));
        }
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{}] {msg}", billing::bj_time_key(now_ms()));
    }
}
