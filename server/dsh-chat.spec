# -*- mode: python ; coding: utf-8 -*-
"""PyInstaller 打包配置（Windows）。

用法（在 server/ 目录下）：
    pyinstaller --noconfirm dsh-chat.spec

产物：dist/dsh-chat/（onedir，双击 dsh-chat.exe 即启动）。
www/ 和 server.py 是兄弟目录，所以这里用 SPECPATH 定位，避免依赖工作目录。
"""

import os

HERE = os.path.abspath(SPECPATH)

a = Analysis(
    [os.path.join(HERE, "server.py")],
    pathex=[HERE],
    binaries=[],
    # (源路径, 打包后的相对路径)；www 是前端静态站，必须随包走
    datas=[(os.path.join(HERE, os.pardir, "www"), "www")],
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
    console=True,
    disable_windowed_traceback=False,
    argv_emulation=False,
    target_arch=None,
    codesign_identity=None,
    entitlements_file=None,
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
