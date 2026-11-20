# -*- mode: python ; coding: utf-8 -*-
"""PyInstaller 打包配置（Windows）—— 单文件模式。

用法（在 server/ 目录下）：
    pyinstaller --noconfirm dsh-chat.spec

产物：dist/dsh-chat.exe（一个文件，双击即用，常驻系统托盘）。

为什么用 onefile：
  - 分发只有一个 exe，不用带着 _internal 目录跑
  - 代价：每次启动要把运行时解压到临时目录，冷启动比 onedir 慢 1-3 秒
  - www/ 和 app.ico 会被打进 exe，运行时从 sys._MEIPASS 读

数据目录仍放在 exe 同级（server.py 里 BASE_DIR = dirname(sys.executable)），
不会因为解压到临时目录而丢失会话。
"""

import os

HERE = os.path.abspath(SPECPATH)
WWW = os.path.join(HERE, os.pardir, "www")
ICON = os.path.join(HERE, "app.ico")

a = Analysis(
    [os.path.join(HERE, "server.py")],
    pathex=[HERE],
    binaries=[],
    # (源路径, 打包后的相对路径)
    #   www      前端静态站，必须随包走
    #   app.ico  托盘图标；运行时从 _MEIPASS 读
    datas=[
        (WWW, "www"),
        (ICON, "."),
    ],
    # tray 是在函数里 try/except 导入的，静态分析可能漏掉；
    # 漏掉的后果是静默退回"开浏览器"，表面上像托盘没生效，很难查。
    hiddenimports=["tray"],
    hookspath=[],
    hooksconfig={},
    runtime_hooks=[],
    excludes=[],
    noarchive=False,
    optimize=0,
)
pyz = PYZ(a.pure)

# 单文件：binaries/datas 全部塞进 EXE，不要再有 COLLECT。
exe = EXE(
    pyz,
    a.scripts,
    a.binaries,
    a.datas,
    [],
    name="dsh-chat",
    debug=False,
    bootloader_ignore_signals=False,
    strip=False,
    upx=True,
    upx_exclude=[],
    runtime_tmpdir=None,
    # 窗口程序：不弹黑色控制台，程序常驻托盘
    console=False,
    disable_windowed_traceback=False,
    argv_emulation=False,
    target_arch=None,
    codesign_identity=None,
    entitlements_file=None,
    icon=ICON,
)
