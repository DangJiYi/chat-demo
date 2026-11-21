//! dsh-chat 本地服务 —— Rust 骨架。
//!
//! 目前只实现两个端点，用于验证整条链路（HTTP 服务、读文件、解析 JSON、返回响应）：
//!   GET /local/status     服务状态
//!   GET /db/summaries     会话摘要列表
//!
//! 其余端点按 server.py 逐个移植。契约必须与 Python 版完全一致——
//! 前端 `src/local/api.ts` 与 `src/services/chat/db.ts` 不改一行。

mod http;
mod store;

use std::env;
use std::process;

use tiny_http::Server;

const VERSION: &str = "0.2.0";

/// 命令行参数：与 server.py 的默认值保持一致。
struct Args {
    port: u16,
    data: String,
}

fn parse_args() -> Args {
    let mut port: u16 = 8790;
    let mut data = String::from("data");

    // 收成 Vec 再用下标遍历：避免 `while let Some(x) = it.next()` 在循环里
    // 又调用 it.next() 时被 clippy 误判（while_let_on_iterator）。
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
            // 安卓壳会传这些；Rust 骨架暂不处理，静默忽略以免启动失败
            "--www" | "--sync-port" => {
                takes_value = true;
            }
            "--no-window" => {}
            "-h" | "--help" => {
                println!("用法: dsh-chat [--port 8790] [--data ./data]");
                process::exit(0);
            }
            _ => {}
        }
        i += if takes_value { 2 } else { 1 };
    }

    Args { port, data }
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
    println!("数据目录 {}", st.dir().display());

    for request in server.incoming_requests() {
        http::handle(request, &st, VERSION);
    }
}
