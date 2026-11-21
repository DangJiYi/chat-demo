//! 安卓 JNI 入口。
//!
//! Java 侧（`MainActivity`）做三件事：
//!   1. `System.loadLibrary("dsh_chat_server")` 加载 `libdsh_chat_server.so`
//!   2. 调 `nativeStart(dataDir, port)` 起服务，然后轮询 `/local/status`
//!   3. WebView 加载 `http://localhost:8790/`
//!
//! 为什么用 JNI 而不是「解压一个可执行文件再 fork」：
//! 安卓 10+ 对应用私有目录禁用 exec（W^X），加载 .so 再调进来是唯一稳定的做法。
//!
//! 为什么服务在独立线程：`serve()` 会一直阻塞在请求循环里，占着 Java 主线程
//! 会直接 ANR。所以这里是「起线程 → 立刻返回 → Java 侧轮询就绪」。
//!
//! **诊断信息不走 logcat**：这里刻意不调 `android.util.Log`——那需要
//! `call_static_method` 的泛型推断，一旦写错只会在设备上炸，而本机无法编译验证。
//! 启动与错误已经写在 `data/app.log`（`applog`），Java 侧出错时会 tail 这个文件
//! 显示到界面上；真正的失败原因则通过 `nativeLastError` 直接回传。

use std::sync::Mutex;

use jni::JNIEnv;
use jni::objects::{JClass, JString};
use jni::sys::{jint, jstring};

/// 启动失败的原因。Java 侧在等待超时时读它，好把真正的原因显示出来，
/// 而不是只看到一句「服务未就绪」。
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// 启动本地服务。返回 0 表示已开始（不代表已就绪），负数表示启动阶段的错误。
///
/// 参数：
///   - `data_dir`：应用私有目录下的 data 路径（可写）
///   - `port`：UI 端口。前端固定在 8790（见 MainActivity.PORT）
///
/// 前端不在这里传：它已经由 `build.rs` 内嵌进 .so，`www` 传空串即可。
#[no_mangle]
pub extern "system" fn Java_com_dangjiyi_dshchat_MainActivity_nativeStart(
    mut env: JNIEnv,
    _class: JClass,
    data_dir: JString,
    port: jint,
) -> jint {
    let data_dir: String = match env.get_string(&data_dir) {
        Ok(s) => s.into(),
        Err(e) => {
            set_error(format!("读取 dataDir 失败: {e}"));
            return -1;
        }
    };
    if data_dir.is_empty() {
        set_error("dataDir 为空".to_string());
        return -2;
    }
    if !(1024..=65535).contains(&port) {
        set_error(format!("端口不合法: {port}"));
        return -3;
    }

    std::thread::Builder::new()
        .name("dsh-server".to_string())
        .spawn(move || {
            let opts = crate::Options {
                data_dir,
                // 空串 = 只用内嵌前端（.so 里已经有 www）
                www: String::new(),
                port: port as u16,
                sync_port: None,
                console: false,
                open_browser: false,
            };
            if let Err(e) = crate::serve(opts) {
                // serve 只在启动阶段失败时返回（监听失败、目录不可写）
                set_error(e);
            }
        })
        .map(|_| 0)
        .unwrap_or_else(|e| {
            set_error(format!("无法创建服务线程: {e}"));
            -4
        })
}

/// 上一次启动失败的原因；没有失败返回 null。
///
/// Java 侧在 `waitForServer` 的循环里调它：一旦有值就立刻停下并显示，
/// 不用白等到 30 秒超时——超时提示「服务未就绪」会把真正的原因埋掉。
#[no_mangle]
pub extern "system" fn Java_com_dangjiyi_dshchat_MainActivity_nativeLastError(
    // 用 mut：jni 0.21 的 new_string 需要 &mut self；
    // 若它取 &self，这里只是多一个 unused_mut 警告，不会报错。
    mut env: JNIEnv,
    _class: JClass,
) -> jstring {
    let msg = match LAST_ERROR.lock() {
        Ok(g) => g.clone(),
        Err(e) => e.into_inner().clone(),
    };
    match msg {
        Some(m) => match env.new_string(m) {
            Ok(s) => s.into_raw(),
            Err(_) => std::ptr::null_mut(),
        },
        None => std::ptr::null_mut(),
    }
}

fn set_error(msg: String) {
    if let Ok(mut g) = LAST_ERROR.lock() {
        *g = Some(msg);
    }
}
