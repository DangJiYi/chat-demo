//! Windows / 桌面命令行入口。
//!
//! 真正的服务在 `lib.rs`（`dsh_chat_server::serve`）：同一个 crate 既要产出
//! 这个 exe，又要产出安卓的 `.so`，所以两边共享同一份实现。
//!
//! 这里只做「命令行才有的那点事」：
//!   - 解析 `--port/--data/--www/...`
//!   - 默认路径解析（相对 exe 所在目录）
//!   - 双击第二次时别再报错崩掉，直接把浏览器指向前一个实例

use std::env;
use std::path::Path;
use std::process;
use std::time::Duration;

use dsh_chat_server::{Options, serve};

struct Args {
    port: u16,
    data: String,
    www: String,
    /// 不隐藏控制台（调试/自查用）
    console: bool,
    /// 不自动打开浏览器（开机自启时用）
    no_open: bool,
    /// 显式指定同步端口（优先于 config.json）
    sync_port: Option<u16>,
}

/// 找一个默认目录：exe 同级优先，其次向上逐级找（最多 5 层）。
///
/// `allow_upward` 很关键：
///   - www 允许向上找：开发时 exe 在 `rust/target/release/`，而 `www` 在 `chat/`
///   - data **禁止**向上找：向上扫会撞到无关目录。实测过一次——exe 在
///     `D:\Downloads\dsh-chat-rust-windows\`，往上找到 `D:\Downloads\data`，
///     把数据目录指向了一个跟本应用无关的文件夹，用户看到空会话列表。
fn resolve_default(exe_dir: &Path, name: &str, allow_upward: bool) -> String {
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
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    let mut data = resolve_default(&exe_dir, "data", false);
    let mut www = resolve_default(&exe_dir, "www", true);
    let mut console = false;
    let mut no_open = false;
    let mut sync_port: Option<u16> = None;

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
            "--sync-port" => {
                takes_value = true;
                if let Some(v) = argv.get(i + 1) {
                    sync_port = v.parse().ok();
                }
            }
            "--console" => console = true,
            "--no-open" => no_open = true,
            "--no-window" => {}
            "-h" | "--help" => {
                println!(
                    "用法: dsh-chat [--port 8790] [--data ./data] [--www ./www] \
                     [--sync-port 8791] [--console] [--no-open]"
                );
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
        no_open,
        sync_port,
    }
}

/// 探测「本机是不是已经有一个实例在跑这个端口」。
///
/// 用裸 TCP 发一个最小 GET，不走 ureq：
///   - 不读系统代理（localhost 绝不能交给代理）
///   - 超时短（250ms），端口没人听就立即返回
/// 只认返回里同时有 `HTTP/1.` 和 `"version"` 的响应，
/// 避免把别的服务误判成本应用。
fn already_running(port: u16) -> bool {
    use std::io::{Read, Write};
    use std::net::{TcpStream, ToSocketAddrs};

    // `to_socket_addrs` 的迭代项已经是 SocketAddr，不需要再包一层 From
    let Some(addr) = ("127.0.0.1", port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut it| it.next())
    else {
        return false;
    };
    let Ok(mut sock) = TcpStream::connect_timeout(&addr, Duration::from_millis(250)) else {
        return false;
    };
    let _ = sock.set_read_timeout(Some(Duration::from_millis(500)));
    let req = format!("GET /local/status HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\n\r\n");
    if sock.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 512];
    let Ok(n) = sock.read(&mut buf) else {
        return false;
    };
    let text = String::from_utf8_lossy(&buf[..n]);
    text.contains("HTTP/1.") && text.contains("\"version\"")
}

fn main() {
    let args = parse_args();

    // 已经有一个实例在跑：把浏览器指向它，然后正常退出。
    //
    // 场景：托盘常驻 + 用户又双击了一次 exe（或开机自启后手动打开）。
    // 以前会在端口绑定处报「无法监听」并退出码 1，看起来像崩了；
    // 而且用户什么也没得到——他要的只是界面。
    //
    // 放在服务启动之前：第二个实例不该去碰数据目录。
    if already_running(args.port) {
        let url = format!("http://127.0.0.1:{}/", args.port);
        println!("已有实例在运行：{url}（本次启动退出）");
        if !args.no_open {
            open_existing(&url);
        }
        return;
    }

    let opts = Options {
        data_dir: args.data,
        www: args.www,
        port: args.port,
        sync_port: args.sync_port,
        console: args.console,
        open_browser: !args.no_open,
    };

    if let Err(e) = serve(opts) {
        eprintln!("{e}");
        process::exit(1);
    }
}

/// 打开已运行实例的界面。
///
/// 这里不能复用 `lib` 里的 `platform_shell`：那条路径会连托盘一起建，
/// 而第二个实例马上要退出，建了托盘只会留下一个悬空图标。
fn open_existing(url: &str) {
    #[cfg(windows)]
    {
        dsh_chat_server::tray::open_ui_url(url);
    }
    #[cfg(not(windows))]
    {
        #[cfg(target_os = "macos")]
        let program = "open";
        #[cfg(not(target_os = "macos"))]
        let program = "xdg-open";
        let _ = process::Command::new(program).arg(url).spawn();
    }
}
