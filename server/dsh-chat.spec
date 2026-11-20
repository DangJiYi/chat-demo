# -*- mode: python ; coding: utf-8 -*-
"""PyInstaller 打包配置（Windows）。

用法（在 server/ 目录下）：
    pyinstaller --noconfirm dsh-chat.spec

产物：dist/dsh-chat/（onedir，双击 dsh-chat.exe 启动，常驻系统托盘）。
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
    #   www    前端静态站，必须随包走
    #   app.ico 托盘和 exe 的图标；运行时从 _MEIPASS 读
    datas=[
        (WWW, "www"),
        (ICON, "."),
    ],
    hiddenimports=[],
    hookspath=[],
    hooksconfig={},
    runtime_hooks=[],
    excludes=[],
    noarchive=False,
    optimize=0,
)
pyz = PYZ(a.pure)

exe = EXE(
    pyz,
    a.scripts,
    [],
    exclude_binaries=True,
    name="dsh-chat",
    debug=False,
    bootloader_ignore_signals=False,
    strip=False,
    upx=True,
    # 窗口程序：不弹黑色控制台，程序常驻托盘
    console=False,
    disable_windowed_traceback=False,
    argv_emulation=False,
    target_arch=None,
    codesign_identity=None,
    entitlements_file=None,
    icon=ICON,
)
coll = COLLECT(
    exe,
    a.binaries,
    a.datas,
    strip=False,
    upx=True,
    upx_exclude=[],
    name="dsh-chat",
)
