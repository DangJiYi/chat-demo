//! dsh-chat 本地服务 —— Rust 重写（替代 server.py）。
//!
//! 契约必须与 Python 版完全一致：前端 `src/local/api.ts` 与
//! `src/services/chat/db.ts` 不改一行。
//!
//! 目录约定：
//!   - UI 服务（/local/*、/db/*、静态文件）只听 127.0.0.1
//!   - 同步服务独听另一个端口（默认 +1），对局域网开放
//!   - UDP 8792 应答自动发现
//!
//! 默认路径相对 **exe 所在目录**：www 允许向上找（开发方便），
//! data 只认 exe 同级（向上猜会指向无关目录，静默读错数据）。

mod aggregate;
mod applog;
mod archive;
mod billing;
mod http;
mod store;
mod sync;
#[cfg(windows)]
mod tray;

use std::env;
use std::path::PathBuf;
use std::process;
use std::thread;

use tiny_http::Server;

use store::Store;

const VERSION: &str = "0.2.0";

struct Args {
    port: u16,
    data: String,
    www: String,
    /// 不隐藏控制台（调试/自查用）
    console: bool,
}

/// 找一个默认目录：exe 同级优先，其次向上逐级找（最多 5 层）。
///
/// `allow_upward` 很关键：
///   - www 允许向上找：开发时 exe 在 `rust/target/release/`，而 `www` 在 `chat/`
///   - data **禁止**向上找：向上扫会撞到无关目录。实测过一次——exe 在
///     `D:\Downloads\dsh-chat-rust-windows\`，往上找到 `D:\Downloads\data`，
///     把数据目录指向了一个跟本应用无关的文件夹，用户看到空会话列表。
fn resolve_default(exe_dir: &std::path::Path, name: &str, allow_upward: bool) -> String {
    if allow_upward {
        let mut probe = Some(exe_dir);
        for _ in 0..5 {
            let Some(dir) = probe else { break };
            let candidate = dir.join(name);
            if candidate.is_dir() {
                return candidate.display().to_string();
            }
            probe = dir.parent();
        }
    }
    exe_dir.join(name).display().to_string()
}

fn parse_args() -> Args {
    let mut port: u16 = 8790;

    let exe_dir = env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    let mut data = resolve_default(&exe_dir, "data", false);
    let mut www = resolve_default(&exe_dir, "www", true);
    let mut console = false;

    // 收成 Vec 再用下标遍历：避免 `while let Some(x) = it.next()` 在循环里
    // 又调用 it.next() 时被 clippy 判为 while_let_on_iterator。
    let argv: Vec<String> = env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].as_str();
        let mut takes_value = false;
        match arg {
            "--port" => {
                takes_value = true;
                if let Some(v) = argv.get(i + 1) {
                    port = v.parse().unwrap_or(8790);
                }
            }
            "--data" => {
                takes_value = true;
                if let Some(v) = argv.get(i + 1) {
                    data = v.clone();
                }
            }
            "--www" => {
                takes_value = true;
                if let Some(v) = argv.get(i + 1) {
                    www = v.clone();
                }
            }
            // 同步端口由 config.json 里的 syncPort 决定，这里只兼容性吞掉参数
            "--sync-port" => takes_value = true,
            "--console" => console = true,
            "--no-window" => {}
            "-h" | "--help" => {
                println!("用法: dsh-chat [--port 8790] [--data ./data] [--www ./www] [--console]");
                process::exit(0);
            }
            _ => {}
        }
        i += if takes_value { 2 } else { 1 };
    }

    Args {
        port,
        data,
        www,
        console,
    }
}

fn main() {
    let args = parse_args();

    let store = match Store::open(&args.data, args.port) {
        Ok(s) => s,
        Err(e) => {
            let data_dir = &args.data;
            eprintln!("无法初始化数据目录 {data_dir}: {e}");
            process::exit(1);
        }
    };
    // 存活期与进程相同的共享状态：后台线程与请求处理都要用
    let store: &'static Store = Box::leak(Box::new(store));

    let www = PathBuf::from(&args.www);
    let www_ok = www.join("index.html").is_file();

    let addr = format!("127.0.0.1:{}", args.port);
    let server = match Server::http(&addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("无法监听 {addr}: {e}");
            process::exit(1);
        }
    };

    // 同步端口：默认 UI 端口 + 1，可在界面上改（存在 config.json）
    let cfg = store.config();
    let sync_port = cfg
        .get("syncPort")
        .and_then(|v| v.as_i64())
        .unwrap_or(args.port as i64 + 1)
        .clamp(1024, 65535) as u16;

    match Server::http(format!("0.0.0.0:{sync_port}")) {
        Ok(s) => {
            thread::spawn(move || {
                for request in s.incoming_requests() {
                    http::handle_sync(request, store, VERSION);
                }
            });
        }
        Err(e) => {
            applog::write(store, &format!("同步端口 {sync_port} 不可用: {e}"));
            eprintln!("同步端口 {sync_port} 不可用: {e}");
        }
    }

    thread::spawn(move || sync::sync_loop(store));
    thread::spawn(move || sync::discovery_server(store));
    thread::spawn(move || sync::archive_tick(store));

    println!("dsh-chat {VERSION} 就绪 http://{addr}/");
    let data_dir = store.dir().display();
    println!("数据目录 {data_dir}");
    println!("同步端口 0.0.0.0:{sync_port}");
    applog::write(
        store,
        &format!(
            "启动 v{VERSION} ui={} sync={sync_port} data={data_dir}",
            args.port
        ),
    );

    let www_dir = www.display();
    if www_ok {
        println!("静态目录 {www_dir} (index.html 存在)");
    } else {
        println!("静态目录 {www_dir} (index.html 缺失，界面会 404)");
        println!("  用 --www 指向含 index.html 的目录，例如：");
        println!("    dsh-chat --www ./www");
        applog::write(store, &format!("警告：静态目录 {www_dir} 缺 index.html"));
    }

    // 托盘图标：加上了才隐藏控制台。
    // 顺序不能反——托盘失败又藏了黑框，用户会以为程序没启动。
    #[cfg(windows)]
    if !args.console {
        if tray::spawn(args.port) {
            tray::hide_console_if_owned();
        } else {
            eprintln!("托盘图标创建失败，保留控制台窗口");
            applog::write(store, "托盘图标创建失败，保留控制台窗口");
        }
    }

    for request in server.incoming_requests() {
        http::handle(request, store, &www, VERSION);
    }
}
