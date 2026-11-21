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
mod assets;
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
use std::time::Duration;

use tiny_http::Server;

use store::Store;

/// 版本号。**改功能就递增**：排查时靠它确认对方装的到底是哪个构建，
/// 不然「改了但没生效」和「根本没改」看起来一模一样。
const VERSION: &str = "0.2.2";

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
            // 同步端口：默认取 config.json 里的 syncPort（UI 端口 + 1）
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
                    "用法: dsh-chat [--port 8790] [--data ./data] [--www ./www] [--sync-port 8791] [--console] [--no-open]"
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

/// 绑同步端口：先试配置值，失败就往下试几个。
///
/// 为什么不能只试一次：8791 是热门端口，本机同时跑两个实例、或者 Windows
/// 把这段划进 Hyper-V 的保留区间（`netsh int ipv4 show excludedportrange`），
/// 都会直接绑不上。以前只在日志里记一行就接着跑，结果界面照样显示 8791、
/// 对端连过来却“拒绝连接”——用户看到的是一句完全无从下手的报错。
fn bind_sync(want: u16) -> Result<(u16, Server), String> {
    let mut last = String::from("没有可用端口");
    for offset in 0..5u16 {
        let Some(port) = want.checked_add(offset) else {
            break;
        };
        match Server::http(format!("0.0.0.0:{port}")) {
            Ok(s) => return Ok((port, s)),
            Err(e) => last = format!("{port}: {e}"),
        }
    }
    Err(last)
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

/// 用默认浏览器打开界面。
///
/// 分平台：Windows 走 `cmd /c start`（不自建窗口），macOS 走 `open`，
/// 其余按 XDG 走 `xdg-open`。失败不致命：用户可以手动打开那一行 URL，
/// 所以只记日志、不中断启动。
fn open_browser(url: &str) {
    #[cfg(windows)]
    {
        // 复用托盘的那份实现：它带 CREATE_NO_WINDOW，不会闪一个 cmd 黑框
        tray::open_ui_url(url);
        return;
    }
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(any(windows, target_os = "macos")))]
    let program = "xdg-open";

    #[cfg(not(windows))]
    {
        match process::Command::new(program).arg(url).spawn() {
            Ok(_) => {}
            Err(e) => eprintln!("打开浏览器失败（{program}）: {e}，请手动访问 {url}"),
        }
    }
}

fn main() {
    let args = parse_args();

    // 已经有一个实例在跑：把浏览器指向它，然后正常退出。
    //
    // 场景：托盘常驻 + 用户又双击了一次 exe（或开机自启后手动打开）。
    // 以前会在端口绑定处报「无法监听」并退出码 1，看起来像崩了；
    // 而且用户什么也没得到——他要的只是界面。
    //
    // 放在 Store::open 之前：第二个实例不该去碰数据目录。
    if already_running(args.port) {
        let url = format!("http://127.0.0.1:{}/", args.port);
        println!("已有实例在运行：{url}（本次启动退出）");
        if !args.no_open {
            open_browser(&url);
        }
        return;
    }

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
    // 界面来源：磁盘 www 优先，否则用内嵌的（单 exe 发布）
    let www_ok = www.join("index.html").is_file();
    let ui_from_embedded = !www_ok && !assets::is_empty();

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
    let want_sync = args.sync_port.unwrap_or_else(|| {
        cfg.get("syncPort")
            .and_then(|v| v.as_i64())
            .unwrap_or(args.port as i64 + 1)
            .clamp(1024, 65535) as u16
    });

    let sync_port = match bind_sync(want_sync) {
        Ok((port, s)) => {
            store.set_sync_bound(port);
            if port != want_sync {
                // 配置里的端口用不了，把实际用的写回去。
                // 不写的话，界面、UDP 发现广播出去的都是一个没人监听的端口，
                // 对端永远连不上，而本机看不出任何异常。
                let patch = serde_json::json!({ "syncPort": port })
                    .as_object()
                    .cloned()
                    .unwrap_or_default();
                let _ = store.save_config(&patch);
                let msg = format!("同步端口 {want_sync} 不可用，已改用 {port}");
                applog::write(store, &msg);
                println!("{msg}");
            }
            thread::spawn(move || {
                for request in s.incoming_requests() {
                    // 每个请求一个线程：附件下载、对端拉取都可能慢，
                    // 串行处理会让一个慢请求堵住其他所有请求。
                    thread::spawn(move || http::handle_sync(request, store, VERSION));
                }
            });
            port
        }
        Err(e) => {
            store.set_sync_bind_err(&e);
            applog::write(store, &format!("同步端口绑定失败: {e}"));
            eprintln!("同步端口绑定失败: {e}");
            0
        }
    };

    thread::spawn(move || sync::sync_loop(store));
    thread::spawn(move || sync::discovery_server(store));
    thread::spawn(move || sync::archive_tick(store));

    println!("dsh-chat {VERSION} 就绪 http://{addr}/");
    let data_dir = store.dir().display();
    println!("数据目录 {data_dir}");
    if sync_port == 0 {
        println!("同步端口 未监听（绑定失败，详见 data/app.log）");
    } else {
        println!("同步端口 0.0.0.0:{sync_port}");
    }
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
    } else if ui_from_embedded {
        println!("界面 内嵌（{} 个文件），无需外部 www 目录", assets::count());
    } else {
        println!("静态目录 {www_dir} (index.html 缺失)");
        println!("  用 --www 指向含 index.html 的目录，或先构建前端：");
        println!("    pnpm build（DSH_LOCAL=1）");
        applog::write(store, "警告：既没有 www/index.html，也没内嵌前端");
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

    // 自动打开浏览器。
    //
    // 放在托盘之后：先隐藏控制台再打开页面，用户不会看到黑框一闪。
    // `--no-open` 用于开机自启 / 调试（不想每次弹一个标签页）。
    if !args.no_open {
        open_browser(&format!("http://127.0.0.1:{}/", args.port));
    }

    // 每个请求单独一个线程。
    //
    // 这不是“为了并发而并发”：聊天走 SSE，一次回答可能持续几分钟，
    // 而代理是流式的，`handle()` 会一直阻塞到流结束。串行处理的话，
    // 正在出字的时候，自动保存会话（PUT /db/session/...）、
    // 侧边栏状态查询（/local/status）全部卡住——
    // 表现就是「一提问整个界面就死了」。
    let www = std::sync::Arc::new(www);
    for request in server.incoming_requests() {
        let www = std::sync::Arc::clone(&www);
        thread::spawn(move || http::handle(request, store, www.as_path(), VERSION));
    }
}
