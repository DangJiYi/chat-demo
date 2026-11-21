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

fn parse_args() -> Args {
    let mut port: u16 = 8790;
    let mut data = String::from("data");
    let mut www = String::from("www");

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
        println!("静态目录 {www_dir} (index.html 缺失，界面会 404)");
    }

    for request in server.incoming_requests() {
        http::handle(request, &st, &www, VERSION);
    }
}
