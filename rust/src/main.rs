//! dsh-chat 本地服务 —— Rust 重写（逐步替代 server.py）。
//!
//! 契约必须与 Python 版完全一致：前端 `src/local/api.ts` 与
//! `src/services/chat/db.ts` 不改一行。
//!
//! 已实现：静态文件、/local/status、/health、/db/summaries、
//!         /db/session/<id> 的 GET/PUT/DELETE、/db/session/<id>/rename

mod http;
mod store;

use std::env;
use std::path::PathBuf;
use std::process;

use tiny_http::Server;

const VERSION: &str = "0.2.0";

/// 命令行参数：与 server.py 的默认值保持一致。
struct Args {
    port: u16,
    data: String,
    www: String,
}

/// 找一个默认目录：exe 同级优先，其次向上逐级找（最多 5 层）。
///
/// `allow_upward` 很关键：
///   - www 允许向上找：开发时 exe 在 `rust/target/release/`，而 `www` 在 `chat/`，
///     没有这一步本地跑就得每次手写 `--www ../www`。
///   - data **禁止**向上找：向上扫会撞到无关目录。实测过一次：
///     exe 在 `D:\Downloads\dsh-chat-rust-windows\`，往上找到 `D:\Downloads\data`，
///     把数据目录指向了一个跟本应用无关的文件夹——用户看到空会话列表，
///     以为数据丢了。数据目录只能由用户显式指定或建在 exe 旁。
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

    // 默认路径跟 Python 版一致：相对 **exe 所在目录**，不是当前工作目录。
    //
    // 否则双击 exe（CWD 可能是 System32 或任意目录）就找不到 www，
    // 界面上表现为 404，而日志里只有一句“index.html 缺失”。
    // 开发时从仓库根跑也可以，那时 exe 在 rust/target/release/，
    // 下面会在找不到时逐级向上找（见 resolve_default）。
    let exe_dir = env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    let mut data = resolve_default(&exe_dir, "data", false);
    let mut www = resolve_default(&exe_dir, "www", true);

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
            // 安卓壳会传这个；Rust 版暂不处理，静默忽略以免启动失败
            "--sync-port" => takes_value = true,
            "--no-window" => {}
            "-h" | "--help" => {
                println!("用法: dsh-chat [--port 8790] [--data ./data] [--www ./www]");
                process::exit(0);
            }
            _ => {}
        }
        i += if takes_value { 2 } else { 1 };
    }

    Args { port, data, www }
}

fn main() {
    let args = parse_args();

    let st = match store::Store::open(&args.data, args.port) {
        Ok(s) => s,
        Err(e) => {
            let data_dir = &args.data;
            eprintln!("无法初始化数据目录 {data_dir}: {e}");
            process::exit(1);
        }
    };

    // 静态目录允许不存在（纯 API 模式下不需要），但要能明确报出来
    let www = PathBuf::from(&args.www);
    let www_ok = www.join("index.html").is_file();

    // 绑 127.0.0.1：只让本机访问。安卓上 WebView 也用这个地址。
    let addr = format!("127.0.0.1:{}", args.port);
    let server = match Server::http(&addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("无法监听 {addr}: {e}");
            process::exit(1);
        }
    };

    println!("dsh-chat {VERSION} 就绪 http://{addr}/");
    let data_dir = st.dir().display();
    println!("数据目录 {data_dir}");
    let www_dir = www.display();
    if www_ok {
        println!("静态目录 {www_dir} (index.html 存在)");
    } else {
        // 说清怎么办：光报“缺失”，用户不知道是参数没给还是目录不对
        println!("静态目录 {www_dir} (index.html 缺失，界面会 404)");
        println!("  用 --www 指向含 index.html 的目录，例如：");
        println!("    dsh-chat --www ./www");
    }

    for request in server.incoming_requests() {
        http::handle(request, &st, &www, VERSION);
    }
}
