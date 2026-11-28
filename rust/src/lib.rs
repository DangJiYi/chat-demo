//! dsh-chat 本地服务 —— Rust 实现。
//!
//! 契约必须与 Python 版（`server/server.py`）完全一致：前端
//! `src/local/api.ts` 与 `src/services/chat/db.ts` 不改一行。
//!
//! 这个 crate 有两个产物：
//!   - `dsh-chat`（bin）：Windows 上双击就用，带托盘、自动开浏览器
//!   - `dsh_chat_server`（cdylib）：安卓的 `.so`，由 JNI 调进来（见 `jni`）
//!
//! 端口分工：
//!   - UI 服务（/local/*、/db/*、静态文件、反向代理）只听回环
//!   - 同步服务独听另一个端口（默认 +1），对局域网开放
//!   - UDP 8792 应答自动发现

pub mod aggregate;
pub mod applog;
pub mod archive;
pub mod assets;
pub mod billing;
pub mod http;
pub mod store;
pub mod sync;
#[cfg(windows)]
pub mod tray;

#[cfg(target_os = "android")]
mod jni;

use std::thread;

use tiny_http::Server;

use store::Store;

/// 版本号。**改功能就递增**：排查时靠它确认对方装的到底是哪个构建，
/// 不然「改了但没生效」和「根本没改」看起来一模一样。
pub const VERSION: &str = "0.2.5";

/// 启动参数。字段都是「跑起来要什么」，与命令行/安卓无关。
pub struct Options {
    /// 数据目录（绝对路径）。安卓传应用私有目录。
    pub data_dir: String,
    /// 前端目录。**空串表示只用内嵌资源**（安卓就是这种）。
    pub www: String,
    /// UI 端口
    pub port: u16,
    /// 同步端口。None 表示读 config.json，缺省时用 port + 1
    pub sync_port: Option<u16>,
    /// 不隐藏控制台（仅 Windows 有意义；安卓/桌面无控制台概念）
    #[allow(dead_code)]
    pub console: bool,
    /// 启动后打开浏览器（仅 Windows 有意义）
    pub open_browser: bool,
}

/// 启动全部服务，然后**阻塞**在 UI 请求循环里，直到进程结束。
///
/// 为什么是阻塞的：调用方（`main`）本来就没有别的事要干；
/// 安卓侧会在自己的线程里调它，返回值用不上。
pub fn serve(opts: Options) -> Result<(), String> {
    let store = Store::open(&opts.data_dir, opts.port)
        .map_err(|e| format!("无法初始化数据目录 {}: {e}", opts.data_dir))?;
    // 存活期与进程相同：后台线程和每个请求处理都要用
    let store: &'static Store = Box::leak(Box::new(store));

    // 让 applog 能在没有 Store 的地方写日志（HTTP 4xx、前端上报）。
    applog::init(store.dir());

    // ---- UI 端口：IPv4 必须有，IPv6 尽力而为 ----
    //
    // **两个都要绑。** 安卓的 network-security-config 只接受主机名
    // `localhost`（IP 字面量会被静默忽略 → 明文被拦 → WebView 白屏），
    // 而 localhost 可能解析到 ::1。只绑 127.0.0.1 的话，
    // 系统一旦选了 ::1 就是一片空白——这是排查了很久的那个坑。
    let v4_addr = format!("127.0.0.1:{}", opts.port);
    let server = Server::http(&v4_addr).map_err(|e| format!("无法监听 {v4_addr}: {e}"))?;
    let v6 = Server::http(format!("[::1]:{}", opts.port)).ok();

    // ---- 同步端口 ----
    let want_sync = opts.sync_port.unwrap_or_else(|| {
        store
            .config()
            .get("syncPort")
            .and_then(|v| v.as_i64())
            .unwrap_or(opts.port as i64 + 1)
            .clamp(1024, 65535) as u16
    });
    let sync_port = start_sync(store, want_sync);

    // 前端目录：空串表示只用内嵌资源（安卓）。
    // 用 Arc 共享给 v6 线程和主循环两边。
    let www = std::sync::Arc::new(std::path::PathBuf::from(&opts.www));

    // ---- 后台线程 ----
    if let Some(v6) = v6 {
        let www6 = std::sync::Arc::clone(&www);
        thread::spawn(move || {
            for request in v6.incoming_requests() {
                let www = std::sync::Arc::clone(&www6);
                thread::spawn(move || http::handle(request, store, www.as_path(), VERSION));
            }
        });
    }

    thread::spawn(move || sync::sync_loop(store));
    thread::spawn(move || sync::discovery_server(store));
    thread::spawn(move || sync::archive_tick(store));

    announce(store, &opts, sync_port);

    // ---- 平台外壳：托盘 / 开浏览器 ----
    platform_shell(store, &opts);

    // ---- 主循环 ----
    // 每个请求单独一个线程：聊天走 SSE，一次回答可能持续几分钟，
    // 串行处理会让自动保存、状态查询全卡住——「一提问界面就死」。
    for request in server.incoming_requests() {
        let www = std::sync::Arc::clone(&www);
        thread::spawn(move || http::handle(request, store, www.as_path(), VERSION));
    }
    Ok(())
}

/// 绑同步端口：先试想要的，失败就往后试几个。
///
/// 8791 是热门端口：本机跑两个实例、别的软件占用、或者 Windows 把它划进了
/// Hyper-V 保留区间（`netsh int ipv4 show excludedportrange`）都会绑不上。
/// 绑不上还照常跑的话，界面会显示一个没人监听的端口，对端连过来得到
/// 「拒绝连接」——完全无从下手。
fn start_sync(store: &'static Store, want: u16) -> u16 {
    for offset in 0..5u16 {
        let Some(port) = want.checked_add(offset) else {
            break;
        };
        if let Ok(s) = Server::http(format!("0.0.0.0:{port}")) {
            store.set_sync_bound(port);
            if port != want {
                // 把实际端口写回配置：否则界面和 UDP 广播都会报错的那个值
                let patch = serde_json::json!({ "syncPort": port })
                    .as_object()
                    .cloned()
                    .unwrap_or_default();
                let _ = store.save_config(&patch);
                let msg = format!("同步端口 {want} 不可用，已改用 {port}");
                applog::write(store, &msg);
                println!("{msg}");
            }
            thread::spawn(move || {
                for request in s.incoming_requests() {
                    thread::spawn(move || http::handle_sync(request, store, VERSION));
                }
            });
            return port;
        }
    }
    let msg = format!("同步端口 {want} 起 5 个都绑不上，同步不可用");
    store.set_sync_bind_err(&msg);
    applog::write(store, &msg);
    eprintln!("{msg}");
    0
}

/// 打印启动摘要，并写一行到 app.log。
fn announce(store: &Store, opts: &Options, sync_port: u16) {
    let data_dir = store.dir().display();
    let www = std::path::Path::new(&opts.www);
    let www_ok = !opts.www.is_empty() && www.join("index.html").is_file();
    let embedded = !assets::is_empty();

    println!("dsh-chat {VERSION} 就绪 http://127.0.0.1:{}/", opts.port);
    println!("数据目录 {data_dir}");
    if sync_port == 0 {
        println!("同步端口 未监听（绑定失败，详见 data/app.log）");
    } else {
        println!("同步端口 0.0.0.0:{sync_port}");
    }
    if www_ok {
        println!("静态目录 {} (index.html 存在)", www.display());
    } else if embedded {
        println!("界面 内嵌（{} 个文件），无需外部 www 目录", assets::count());
    } else {
        println!("警告：既没有 www/index.html，也没内嵌前端，界面将 404");
    }
    applog::write(
        store,
        &format!(
            "启动 v{VERSION} ui={} sync={sync_port} data={data_dir}",
            opts.port
        ),
    );
}

/// Windows 的托盘图标 + 自动开浏览器。
#[cfg(windows)]
fn platform_shell(store: &Store, opts: &Options) {
    if !opts.console {
        // 顺序不能反：托盘失败又藏了黑框，用户会以为程序没启动
        if tray::spawn(opts.port) {
            tray::hide_console_if_owned();
        } else {
            eprintln!("托盘图标创建失败，保留控制台窗口");
            applog::write(store, "托盘图标创建失败，保留控制台窗口");
        }
    }
    // 放在托盘之后：先隐藏控制台再开页面，不会看到黑框一闪
    if opts.open_browser {
        tray::open_ui_url(&format!("http://127.0.0.1:{}/", opts.port));
    }
}

/// 其他桌面系统：没有托盘，只用系统命令开浏览器。
#[cfg(all(not(windows), not(target_os = "android")))]
fn platform_shell(_store: &Store, opts: &Options) {
    if !opts.open_browser {
        return;
    }
    let url = format!("http://127.0.0.1:{}/", opts.port);
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(target_os = "macos"))]
    let program = "xdg-open";
    if let Err(e) = std::process::Command::new(program).arg(&url).spawn() {
        eprintln!("打开浏览器失败（{program}）: {e}，请手动访问 {url}");
    }
}

/// 安卓：没有任何桌面交互，WebView 由 Java 侧负责。
#[cfg(target_os = "android")]
fn platform_shell(_store: &Store, _opts: &Options) {}
