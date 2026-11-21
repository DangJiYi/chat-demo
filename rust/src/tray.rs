//! Windows 托盘图标：右键菜单「打开界面 / 退出」，双击直接打开界面。
//!
//! 为什么需要：exe 是控制台程序，用户双击后看到黑框；关掉黑框等于杀进程，
//! 而聊天服务本该像 QQ 那样缩在托盘里。所以：
//!   - 托盘创建成功 **之后** 才隐藏控制台（顺序很重要：托盘失败就留着黑框，
//!     否则程序会变得完全看不见）
//!   - 控制台是「独占」的才隐藏：从终端启动时，那个窗口是终端自己的，
//!     藏了会连带把用户的终端藏掉
//!
//! 只用少量 Win32 调用，全部在一个线程里跑消息循环，不引 GUI 框架。

use std::sync::atomic::{AtomicU16, Ordering};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::Console::{GetConsoleProcessList, GetConsoleWindow};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD};
use windows_sys::Win32::UI::Shell::{NIM_DELETE, NOTIFYICONDATAW};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconFromResourceEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyMenu, DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics, LoadIconW,
    PostMessageW, RegisterClassW, SetForegroundWindow, ShowWindow, TrackPopupMenu,
    TranslateMessage, HICON, IDI_APPLICATION, LR_DEFAULTCOLOR, MF_SEPARATOR, MF_STRING, MSG,
    SM_CXSMICON, SW_HIDE, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_LBUTTONDBLCLK,
    WM_NULL, WM_RBUTTONUP, WNDCLASSW,
};

// APP_ICO 由 build.rs 从 rust/assets/app.ico 生成；缺失时是 None。
include!(concat!(env!("OUT_DIR"), "/icon_embed.rs"));

/// UI 端口：菜单里「打开界面」要拼 URL。
static UI_PORT: AtomicU16 = AtomicU16::new(8790);

/// 托盘回调消息：图标被点击时系统会发这个。
const WM_TRAY: u32 = WM_APP + 1;
const CMD_OPEN: usize = 1001;
const CMD_QUIT: usize = 1002;
/// 托盘图标 id（同一个窗口可以有多个图标，这里只用 1 个）。
const TRAY_ID: u32 = 1;
/// 创建控制台时用的标记常量：CREATE_NO_WINDOW。
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 系统小图标边长（通常是 16，高 DPI 下会更大）。
fn small_icon_size() -> i32 {
    let n = unsafe { GetSystemMetrics(SM_CXSMICON) };
    n.clamp(16, 64)
}

/// 从内嵌的 .ico 里挑一张，做成 HICON。
///
/// 不用 `LoadImageW(LR_LOADFROMFILE)`：那是从磁盘读文件，而我们是单 exe，
/// 图标就在二进制里。`CreateIconFromResourceEx` 直接吃 .ico 中某一张的字节，
/// 并按 cx/cy 缩放。
fn app_icon() -> HICON {
    unsafe {
        let Some(payload) = APP_ICO.and_then(|ico| pick_ico_entry(ico, small_icon_size()))
        else {
            // 没图标（没跑 tools/make-icons.py）：退回系统默认，不影响启动
            return LoadIconW(std::ptr::null_mut(), IDI_APPLICATION);
        };
        let edge = small_icon_size();
        let h = CreateIconFromResourceEx(
            payload.as_ptr(),
            payload.len() as u32,
            1,            // fIcon = TRUE
            0x0003_0000,  // dwVer = 3.0
            edge,
            edge,
            LR_DEFAULTCOLOR,
        );
        if h.is_null() {
            LoadIconW(std::ptr::null_mut(), IDI_APPLICATION)
        } else {
            h
        }
    }
}

/// 从 .ico 目录里挑最接近 `desired` 的那一张，返回它的原始字节。
///
/// .ico 布局：6 字节头（reserved, type, count）+ count × 16 字节目录项，
/// 每项里记录该图的字节数与偏移。任何一个越界都跳过——
/// 图标文件头写坏了也只能降级，不能让托盘线程 panic。
fn pick_ico_entry(ico: &'static [u8], desired: i32) -> Option<&'static [u8]> {
    if ico.len() < 6 {
        return None;
    }
    let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
    if count == 0 || ico.len() < 6 + count * 16 {
        return None;
    }
    // (排序键, 偏移, 长度)。排序键：够大的取最小的，全都不够就取最大的。
    let mut best: Option<(i32, usize, usize)> = None;
    for i in 0..count {
        let e = 6 + i * 16;
        let w = if ico[e] == 0 { 256 } else { ico[e] as i32 };
        let len = u32::from_le_bytes([ico[e + 8], ico[e + 9], ico[e + 10], ico[e + 11]]) as usize;
        let off = u32::from_le_bytes([ico[e + 12], ico[e + 13], ico[e + 14], ico[e + 15]]) as usize;
        if off.checked_add(len).map_or(true, |end| end > ico.len()) {
            continue;
        }
        let score = if w >= desired { w } else { 1000 + w };
        if best.map_or(true, |(b, _, _)| score < b) {
            best = Some((score, off, len));
        }
    }
    best.map(|(_, off, len)| &ico[off..off + len])
}

/// 起一个托盘线程。返回 true 表示图标真的加上了。
///
/// 等最多 3 秒：调用方要据此决定是否隐藏控制台，不能猜。
pub fn spawn(port: u16) -> bool {
    UI_PORT.store(port, Ordering::Relaxed);
    let (tx, rx) = std::sync::mpsc::channel::<bool>();
    std::thread::spawn(move || {
        run(tx);
    });
    rx.recv_timeout(std::time::Duration::from_secs(3))
        .unwrap_or(false)
}

/// 托盘线程主体：建隐藏窗口 → 加图标 → 跑消息循环。
fn run(tx: std::sync::mpsc::Sender<bool>) {
    unsafe {
        let hinst = GetModuleHandleW(std::ptr::null());
        let class_name = wide("DshChatTrayWnd");
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = hinst;
        wc.lpszClassName = class_name.as_ptr();
        if RegisterClassW(&wc) == 0 {
            let _ = tx.send(false);
            return;
        }
        // 不显示窗口，只借它的消息循环（SW_HIDE 由 dwStyle=0 + 不 ShowWindow 保证）
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            class_name.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            let _ = tx.send(false);
            return;
        }
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = TRAY_ID;
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = app_icon();
        let tip = wide("dsh-chat");
        for (slot, ch) in nid.szTip.iter_mut().zip(tip.iter()) {
            *slot = *ch;
        }
        if Shell_NotifyIconW(NIM_ADD, &nid) == 0 {
            let _ = tx.send(false);
            return;
        }
        let _ = tx.send(true);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_TRAY => {
            // lParam 是鼠标事件；双击打开界面，右键弹菜单
            match lparam as u32 {
                WM_LBUTTONDBLCLK => open_ui(UI_PORT.load(Ordering::Relaxed)),
                WM_RBUTTONUP => show_menu(hwnd),
                _ => {}
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn show_menu(hwnd: HWND) {
    unsafe {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        let open = wide("打开界面");
        let quit = wide("退出");
        AppendMenuW(menu, MF_STRING, CMD_OPEN, open.as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(menu, MF_STRING, CMD_QUIT, quit.as_ptr());

        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        // 不先抢前台，点别处时菜单会赖在屏幕上（Win32 的老毛病）
        SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
            pt.x,
            pt.y,
            0,
            hwnd,
            std::ptr::null(),
        );
        DestroyMenu(menu);
        let _ = PostMessageW(hwnd, WM_NULL, 0, 0);

        match cmd as usize {
            CMD_OPEN => open_ui(UI_PORT.load(Ordering::Relaxed)),
            CMD_QUIT => {
                remove_icon(hwnd);
                // 直接退出：所有落盘都是原子的，不需要额外收尾
                std::process::exit(0);
            }
            _ => {}
        }
    }
}

fn remove_icon(hwnd: HWND) {
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = TRAY_ID;
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

/// 用默认浏览器打开界面。走 `cmd /c start`，免得再引 ShellExecuteW。
fn open_ui(port: u16) {
    open_ui_url(&format!("http://127.0.0.1:{port}/"));
}

/// 用默认浏览器打开指定 URL。
///
/// 走 `cmd /c start`：不用另引 ShellExecuteW，而且带上 CREATE_NO_WINDOW，
/// 启动时不会闪一下 cmd 黑框。
pub fn open_ui_url(url: &str) {
    use std::os::windows::process::CommandExt;
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

/// 控制台是「独占」的才隐藏。
///
/// 从终端（PowerShell / Git Bash）启动时，控制台里挂着终端进程，
/// 这时隐藏会把用户的终端一起藏掉——只在自己独占时才动手。
pub fn hide_console_if_owned() {
    unsafe {
        let mut pids = [0u32; 4];
        let n = GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32);
        if n == 1 {
            let console = GetConsoleWindow();
            if !console.is_null() {
                ShowWindow(console, SW_HIDE);
            }
        }
    }
}
