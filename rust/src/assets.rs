//! 内嵌的前端静态资源。
//!
//! `build.rs` 把 `chat/www/` 下的每个文件变成 `include_bytes!`，
//! 于是 exe 自带界面，不需要旁边放一个 `www` 目录。
//!
//! 磁盘上的 `www/` 仍然优先（见 `http::serve_static`）：
//! 开发时改前端不用重新编译 exe，`--www` 也能指向别处。

// 生成到 OUT_DIR/embedded_www.rs，内容是 EMBEDDED_WWW: &[(&str, &[u8])]
include!(concat!(env!("OUT_DIR"), "/embedded_www.rs"));

/// 按相对路径取内嵌资源。
///
/// 路径统一成 `/` 分隔、去掉前缀斜杠，这样 `/static/js/a.js` 与
/// `static/js/a.js` 都能命中。
pub fn lookup(rel: &str) -> Option<&'static [u8]> {
    let want = rel.trim_start_matches('/');
    // 文件数一百出头，线性扫足够快（只在磁盘也没命中时才走到这里）。
    // 写 `e.0` / `e.1` 而不是模式解构：后者在 `&&(&str, &[u8])` 上依赖
    // match ergonomics 的多层自动解引用，读起来容易歧义。
    EMBEDDED_WWW.iter().find(|e| e.0 == want).map(|e| e.1)
}

/// 内嵌资源是否为空（前端没构建过）。
pub fn is_empty() -> bool {
    EMBEDDED_WWW.is_empty()
}

/// 内嵌文件数，启动日志里报一下，便于确认打包进去的是哪一份。
pub fn count() -> usize {
    EMBEDDED_WWW.len()
}
