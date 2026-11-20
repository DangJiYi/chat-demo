"""Windows 系统托盘图标。纯标准库 ctypes，不引入任何第三方依赖。

只在 win32 上可用；其它平台导入本模块会抛 ImportError，调用方需自行判断。

用法：
    tray = TrayIcon("dsh-chat", icon_path, on_open=..., on_data=..., on_exit=...)
    tray.start()          # 非阻塞，内部起一个带消息循环的线程
    ...
    tray.stop()           # 移除图标并结束消息循环

设计要点：
  - 托盘图标必须由「有消息循环的线程」创建和销毁，所以整个生命周期都在同一后台线程里。
  - 右键菜单前需要 SetForegroundWindow + 事后 PostMessage(WM_NULL)，
    否则菜单点空白处不会消失（Win32 的经典坑）。
  - WNDPROC 回调对象必须保持引用，否则被 GC 回收后进程直接崩。
"""

import ctypes
import sys

if sys.platform != "win32":
    raise ImportError("tray 只在 Windows 上可用")

from ctypes import wintypes as wt

user32 = ctypes.WinDLL("user32", use_last_error=True)
shell32 = ctypes.WinDLL("shell32", use_last_error=True)
kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)

LRESULT = ctypes.c_ssize_t
WPARAM = ctypes.c_size_t
LPARAM = ctypes.c_ssize_t

# ---------------------------------------------------------------- 常量

WM_DESTROY = 0x0002
WM_CLOSE = 0x0010
WM_COMMAND = 0x0111
WM_NULL = 0x0000
WM_LBUTTONUP = 0x0202
WM_LBUTTONDBLCLK = 0x0203
WM_RBUTTONUP = 0x0205
WM_TRAY = 0x0400 + 1          # WM_USER + 1，自定的托盘回调消息

NIM_ADD = 0
NIM_MODIFY = 1
NIM_DELETE = 2

NIF_MESSAGE = 0x01
NIF_ICON = 0x02
NIF_TIP = 0x04

IMAGE_ICON = 1
LR_LOADFROMFILE = 0x0010
LR_DEFAULTSIZE = 0x0040
IDI_APPLICATION = 32512

MF_STRING = 0x0000
MF_SEPARATOR = 0x0800
TPM_RIGHTBUTTON = 0x0002
TPM_RETURNCMD = 0x0100

SW_HIDE = 0

ID_OPEN = 1
ID_DATA = 2
ID_EXIT = 3

HWND_MESSAGE = wt.HWND(-3)

# ---------------------------------------------------------------- 结构体


class NOTIFYICONDATAW(ctypes.Structure):
    _fields_ = [
        ("cbSize", wt.DWORD),
        ("hWnd", wt.HWND),
        ("uID", wt.UINT),
        ("uFlags", wt.UINT),
        ("uCallbackMessage", wt.UINT),
        ("hIcon", wt.HICON),
        ("szTip", wt.WCHAR * 128),
        ("dwState", wt.DWORD),
        ("dwStateMask", wt.DWORD),
        ("szInfo", wt.WCHAR * 256),
        ("uTimeoutOrVersion", wt.UINT),
        ("szInfoTitle", wt.WCHAR * 64),
        ("dwInfoFlags", wt.DWORD),
        ("guidItem", ctypes.c_byte * 16),
        ("hBalloonIcon", wt.HICON),
    ]


WNDPROC = ctypes.WINFUNCTYPE(LRESULT, wt.HWND, wt.UINT, WPARAM, LPARAM)


class WNDCLASSEXW(ctypes.Structure):
    _fields_ = [
        ("cbSize", wt.UINT),
        ("style", wt.UINT),
        ("lpfnWndProc", WNDPROC),
        ("cbClsExtra", ctypes.c_int),
        ("cbWndExtra", ctypes.c_int),
        ("hInstance", wt.HINSTANCE),
        ("hIcon", wt.HICON),
        ("hCursor", wt.HANDLE),
        ("hbrBackground", wt.HBRUSH),
        ("lpszMenuName", wt.LPCWSTR),
        ("lpszClassName", wt.LPCWSTR),
        ("hIconSm", wt.HICON),
    ]


# ---------------------------------------------------------------- 函数签名
# 不声明 argtypes/restype 的话，64 位下指针会被截断成 32 位，句柄直接失效。

user32.DefWindowProcW.argtypes = [wt.HWND, wt.UINT, WPARAM, LPARAM]
user32.DefWindowProcW.restype = LRESULT
user32.RegisterClassExW.argtypes = [ctypes.POINTER(WNDCLASSEXW)]
user32.RegisterClassExW.restype = wt.WORD
user32.CreateWindowExW.argtypes = [wt.DWORD, wt.LPCWSTR, wt.LPCWSTR, wt.DWORD,
                                   ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_int,
                                   wt.HWND, wt.HMENU, wt.HINSTANCE, ctypes.c_void_p]
user32.CreateWindowExW.restype = wt.HWND
user32.DestroyWindow.argtypes = [wt.HWND]
user32.PostQuitMessage.argtypes = [ctypes.c_int]
user32.GetMessageW.argtypes = [ctypes.POINTER(wt.MSG), wt.HWND, wt.UINT, wt.UINT]
user32.GetMessageW.restype = ctypes.c_int
user32.TranslateMessage.argtypes = [ctypes.POINTER(wt.MSG)]
user32.DispatchMessageW.argtypes = [ctypes.POINTER(wt.MSG)]
user32.DispatchMessageW.restype = LRESULT
user32.PostMessageW.argtypes = [wt.HWND, wt.UINT, WPARAM, LPARAM]
user32.CreatePopupMenu.restype = wt.HMENU
user32.AppendMenuW.argtypes = [wt.HMENU, wt.UINT, ctypes.c_size_t, wt.LPCWSTR]
user32.DestroyMenu.argtypes = [wt.HMENU]
user32.TrackPopupMenu.argtypes = [wt.HMENU, wt.UINT, ctypes.c_int, ctypes.c_int,
                                  ctypes.c_int, wt.HWND, ctypes.c_void_p]
user32.TrackPopupMenu.restype = ctypes.c_uint
user32.SetForegroundWindow.argtypes = [wt.HWND]
user32.GetCursorPos.argtypes = [ctypes.POINTER(wt.POINT)]
user32.LoadImageW.argtypes = [wt.HINSTANCE, wt.LPCWSTR, wt.UINT,
                              ctypes.c_int, ctypes.c_int, wt.UINT]
user32.LoadImageW.restype = wt.HANDLE
user32.LoadIconW.argtypes = [wt.HINSTANCE, wt.LPCWSTR]
user32.LoadIconW.restype = wt.HICON
user32.ShowWindow.argtypes = [wt.HWND, ctypes.c_int]
shell32.Shell_NotifyIconW.argtypes = [wt.DWORD, ctypes.POINTER(NOTIFYICONDATAW)]
shell32.Shell_NotifyIconW.restype = wt.BOOL
kernel32.GetModuleHandleW.argtypes = [wt.LPCWSTR]
kernel32.GetModuleHandleW.restype = wt.HMODULE
kernel32.GetConsoleWindow.restype = wt.HWND


def hide_console() -> bool:
    """隐藏当前进程的控制台窗口；没有控制台（打包成窗口程序）时返回 False。"""
    hwnd = kernel32.GetConsoleWindow()
    if not hwnd:
        return False
    user32.ShowWindow(hwnd, SW_HIDE)
    return True


class TrayIcon:
    """一个托盘图标 + 右键菜单。start() 之后在后台线程跑消息循环。"""

    def __init__(self, tip, icon_path=None, on_open=None, on_data=None, on_exit=None):
        self.tip = tip[:127]
        self.icon_path = icon_path
        self.on_open = on_open
        self.on_data = on_data
        self.on_exit = on_exit

        self._hwnd = None
        self._hicon = None
        self._thread = None
        self._wndproc_ref = None      # 必须持有，否则回调被回收会崩
        self._nid = None
        self.ok = False
        self.error = None

    # ---------------------------------------------------------------- 线程

    def start(self):
        import threading
        self._thread = threading.Thread(target=self._run, name="dsh-tray", daemon=True)
        self._thread.start()
        return self

    def stop(self):
        if self._hwnd:
            try:
                user32.PostMessageW(self._hwnd, WM_CLOSE, 0, 0)
            except OSError:
                pass

    # ---------------------------------------------------------------- 主循环

    def _run(self):
        import time
        try:
            hinst = kernel32.GetModuleHandleW(None)

            def wndproc(hwnd, msg, wparam, lparam):
                return self._wndproc(hwnd, msg, wparam, lparam)

            self._wndproc_ref = WNDPROC(wndproc)

            wc = WNDCLASSEXW()
            wc.cbSize = ctypes.sizeof(WNDCLASSEXW)
            wc.style = 0
            wc.lpfnWndProc = self._wndproc_ref
            wc.hInstance = hinst
            wc.lpszClassName = "DshChatTrayCls"
            if not user32.RegisterClassExW(ctypes.byref(wc)):
                raise OSError(ctypes.get_last_error(), "RegisterClassExW 失败")

            # 用普通隐藏窗口而不是 message-only 窗口：
            # TrackPopupMenu 需要真实窗口才能正确接管前台、让菜单在点空白处时消失。
            self._hwnd = user32.CreateWindowExW(
                0, "DshChatTrayCls", "dsh-chat", 0,
                0, 0, 0, 0, None, None, hinst, None)
            if not self._hwnd:
                raise OSError(ctypes.get_last_error(), "CreateWindowExW 失败")

            self._hicon = self._load_icon(hinst)

            nid = NOTIFYICONDATAW()
            nid.cbSize = ctypes.sizeof(NOTIFYICONDATAW)
            nid.hWnd = self._hwnd
            nid.uID = 1
            nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP
            nid.uCallbackMessage = WM_TRAY
            nid.hIcon = self._hicon
            nid.szTip = self.tip
            self._nid = nid

            if not shell32.Shell_NotifyIconW(NIM_ADD, ctypes.byref(nid)):
                raise OSError(ctypes.get_last_error(), "Shell_NotifyIcon 添加图标失败")

            self.ok = True

            msg = wt.MSG()
            while user32.GetMessageW(ctypes.byref(msg), None, 0, 0) > 0:
                user32.TranslateMessage(ctypes.byref(msg))
                user32.DispatchMessageW(ctypes.byref(msg))
        except Exception as exc:  # noqa: BLE001
            self.error = exc
            self.ok = False

    # ---------------------------------------------------------------- 回调

    def _wndproc(self, hwnd, msg, wparam, lparam):
        try:
            if msg == WM_TRAY:
                event = lparam & 0xFFFF
                if event in (WM_LBUTTONUP, WM_LBUTTONDBLCLK):
                    self._invoke(self.on_open)
                elif event == WM_RBUTTONUP:
                    self._menu(hwnd)
                return 0
            if msg == WM_COMMAND:
                cmd = wparam & 0xFFFF
                if cmd == ID_OPEN:
                    self._invoke(self.on_open)
                elif cmd == ID_DATA:
                    self._invoke(self.on_data)
                elif cmd == ID_EXIT:
                    self._remove_icon()
                    self._invoke(self.on_exit)
                    user32.DestroyWindow(hwnd)
                return 0
            if msg == WM_CLOSE:
                self._remove_icon()
                user32.DestroyWindow(hwnd)
                return 0
            if msg == WM_DESTROY:
                user32.PostQuitMessage(0)
                return 0
        except Exception:  # noqa: BLE001
            pass
        return user32.DefWindowProcW(hwnd, msg, wparam, lparam)

    def _menu(self, hwnd):
        hmenu = user32.CreatePopupMenu()
        if not hmenu:
            return
        user32.AppendMenuW(hmenu, MF_STRING, ID_OPEN, "打开界面")
        user32.AppendMenuW(hmenu, MF_STRING, ID_DATA, "打开数据目录")
        user32.AppendMenuW(hmenu, MF_SEPARATOR, 0, None)
        user32.AppendMenuW(hmenu, MF_STRING, ID_EXIT, "退出")

        pt = wt.POINT()
        user32.GetCursorPos(ctypes.byref(pt))
        # 这两行是 Win32 的经典配套：不设前台窗口，菜单不会因点空白而关闭
        user32.SetForegroundWindow(hwnd)
        cmd = user32.TrackPopupMenu(
            hmenu, TPM_RIGHTBUTTON | TPM_RETURNCMD, pt.x, pt.y, 0, hwnd, None)
        user32.PostMessageW(hwnd, WM_NULL, 0, 0)
        user32.DestroyMenu(hmenu)

        if cmd == ID_OPEN:
            self._invoke(self.on_open)
        elif cmd == ID_DATA:
            self._invoke(self.on_data)
        elif cmd == ID_EXIT:
            self._remove_icon()
            self._invoke(self.on_exit)
            user32.PostMessageW(hwnd, WM_CLOSE, 0, 0)

    def _invoke(self, fn):
        if fn:
            try:
                fn()
            except Exception:  # noqa: BLE001
                pass

    def _remove_icon(self):
        if self._nid is not None:
            try:
                shell32.Shell_NotifyIconW(NIM_DELETE, ctypes.byref(self._nid))
            except OSError:
                pass
            self._nid = None

    def _load_icon(self, hinst):
        """优先加载 app.ico，失败时退回系统默认图标。"""
        if self.icon_path:
            h = user32.LoadImageW(None, self.icon_path, IMAGE_ICON,
                                  32, 32, LR_LOADFROMFILE)
            if h:
                return h
        # IDI_APPLICATION 是整数资源 ID，要按 MAKEINTRESOURCE 交给 API
        return user32.LoadIconW(None, ctypes.cast(ctypes.c_void_p(IDI_APPLICATION),
                                                  wt.LPCWSTR))
