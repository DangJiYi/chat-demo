//! 把前端产物 `chat/www/` 打进二进制，做成「一个 exe 就能跑」。
//!
//! 为什么用 build.rs 而不是 `include_dir` 之类的 crate：
//! 只需要列个文件名表 + `include_bytes!`，std 就够，不必为这点事
//! 多背一个依赖（也就少一次供应链风险）。
//!
//! 生成的 `embedded_www.rs`（在 OUT_DIR 里）长这样：
//! ```ignore
//! pub static EMBEDDED_WWW: &[(&str, &[u8])] = &[
//!     ("index.html", include_bytes!("D:\\...\\www\\index.html")),
//!     ...
//! ];
//! ```
//! 路径用绝对路径：`include_bytes!` 是相对「包含它的文件」解析的，
//! 而那个文件在 OUT_DIR 里，相对路径会指错地方。

use std::path::{Path, PathBuf};

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    // rust/../www == chat/www
    let www = manifest.parent().unwrap_or(&manifest).join("www");
    println!("cargo:rerun-if-changed={}", www.display());

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    if www.is_dir() {
        collect(&www, &www, &mut files);
    }
    // 排序让输出稳定：同一份源码每次生成的二进制字节一致，便于比对
    files.sort_by(|a, b| a.0.cmp(&b.0));

    // 让 cargo 在这些文件变动时重跑：
    //   - 目录：新增/删除文件要重新生成清单（rustc 的 dep-info 只跟踪
    //     已 include 的文件，新文件不在里面）
    //   - 每个文件：确保内容变动也触发（双保险，代价只是一行 cargo 指令）
    for (_, abs) in &files {
        println!("cargo:rerun-if-changed={}", abs.display());
    }

    let mut src = String::new();
    src.push_str("// 由 build.rs 生成，请勿手改。\n");
    src.push_str("pub static EMBEDDED_WWW: &[(&str, &[u8])] = &[\n");
    for (rel, abs) in &files {
        // {:?} 会产出合法的 Rust 字符串字面量（反斜杠会转义），正好给 include_bytes!
        src.push_str(&format!("    ({:?}, include_bytes!({:?})),\n", rel, abs));
    }
    src.push_str("];\n");

    let dest = out_dir.join("embedded_www.rs");
    std::fs::write(&dest, src).expect("写入 embedded_www.rs");

    // 前端没构建过时不该让编译失败（后端开发时可能还没 pnpm build），
    // 但要让用户知道为什么界面 404。
    if files.is_empty() {
        println!("cargo:warning=没有找到前端产物 {}，exe 将不含界面", www.display());
    }
}

/// 递归收集所有普通文件，路径转成 `/` 分隔的相对路径（HTTP 用）。
fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, out);
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel = rel
            .components()
            .filter_map(|c| match c {
                std::path::Component::Normal(s) => Some(s.to_string_lossy().to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/");
        if rel.is_empty() {
            continue;
        }
        out.push((rel, path));
    }
}
