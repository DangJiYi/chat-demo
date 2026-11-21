//! 构建脚本，做两件事：
//!
//! 1. 把前端产物 `chat/www/` 打进二进制，做成「一个 exe 就能跑」
//! 2. 把 `rust/assets/app.ico` 嵌进构建（托盘图标 + exe 本体图标）
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
        // 先转成 String 再 {:?}：String 的 Debug 一定会输出带转义的字符串字面量
        // （{Windows 路径的 `\` → `\\`），正好是 include_bytes! 要的形式。
        // 直接在 PathBuf 上 {:?} 依赖 OsStr 的 Debug 实现，不想赌它。
        let abs_s: String = abs.to_string_lossy().into_owned();
        // 生成一行：    ("index.html", include_bytes!("D:\\...\\www\\index.html")),
        // 括号数：外层元组一对 + include_bytes 一对，后面跟逗号。
        src.push_str(&format!(r#"    ({rel:?}, include_bytes!({abs_s:?})),"#));
        src.push('\n');
    }
    src.push_str("];\n");

    let dest = out_dir.join("embedded_www.rs");
    std::fs::write(&dest, src).expect("写入 embedded_www.rs");

    // 前端没构建过时不该让编译失败（后端开发时可能还没 pnpm build），
    // 但要让用户知道为什么界面 404。
    if files.is_empty() {
        println!("cargo:warning=没有找到前端产物 {}，exe 将不含界面", www.display());
    }

    embed_icon(&manifest, &out_dir);
}

/// 图标：把 `rust/assets/app.ico` 带进构建。
///
/// 两件事：
///   1. 生成 `icon_embed.rs`，让 tray.rs 能拿到原始 .ico 字节去建托盘图标
///   2. Windows 目标下编译一份资源脚本，把图标嵌进 exe 本体
///      （否则资源管理器/任务栏显示的是通用 exe 图标）
///
/// .ico 由 `tools/make-icons.py` 从 `assets/icon.png` 生成，CI 会在 cargo 之前
/// 跑它；本地想改图标也只需要跑那一次。
/// **缺失时不阻断构建**：降级成默认图标 + 醒目警告，
/// 总比因为少一张图就打不出包好。
fn embed_icon(manifest: &Path, out_dir: &Path) {
    let ico = manifest.join("assets").join("app.ico");
    println!("cargo:rerun-if-changed={}", ico.display());

    let mut src = String::from("// 由 build.rs 生成，请勿手改。\n");
    if ico.is_file() {
        let path: String = ico.to_string_lossy().into_owned();
        src.push_str(&format!(
            "pub static APP_ICO: Option<&[u8]> = Some(include_bytes!({path:?}));\n"
        ));
    } else {
        src.push_str("pub static APP_ICO: Option<&[u8]> = None;\n");
        println!(
            "cargo:warning=缺少 {}，托盘与 exe 将用默认图标（运行 python tools/make-icons.py 生成）",
            ico.display()
        );
    }
    std::fs::write(out_dir.join("icon_embed.rs"), src).expect("写入 icon_embed.rs");

    // 嵌进 exe 本体：只有 Windows 目标才做。
    //
    // 这一步需要 RC 编译器（rc.exe）。失败只告警、不阻断——
    // 图标是装饰，不该成为“打不出包”的理由。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") || !ico.is_file() {
        return;
    }
    #[cfg(windows)]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon(&ico.to_string_lossy());
        // 顺带写入文件描述：右键属性里能看到，排查“装的到底是哪个构建”很有用。
        res.set("FileDescription", "dsh-chat");
        res.set("ProductName", "dsh-chat");
        if let Err(e) = res.compile() {
            println!("cargo:warning=exe 图标/版本资源编译失败（不影响构建）: {e}");
        }
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
