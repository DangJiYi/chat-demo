# 构建说明

仓库里现在有三块东西：

```text
src/            前端源码（React + rspack）
www/            前端预构建产物（src 的 dist，随包发布；与 python 版一致）
server/         Python 本地服务（server.py + PyInstaller 配置）
android/        Chaquopy 安卓壳（把 server.py + www 打进 APK）
```

## 出 Windows 包

在本机装 Python 3.9+ 和 PyInstaller，然后在 `server/` 目录：

```bash
pip install pyinstaller
pyinstaller --noconfirm dsh-chat.spec
```

产物是**单个文件** `server/dist/dsh-chat.exe`（没有 `_internal` 目录，不需要压缩包），
拷到哪都能双击运行，数据目录建在 exe 旁边。

用的是 PyInstaller 的 onefile 模式。代价是**冷启动慢 1-3 秒**（每次启动都要把
运行时和 `www/` 解压到临时目录）；好处是分发只有一个 exe，不会出现
「只拷了 exe 忘了带 _internal」这种故障。

### 托盘程序（没有黑色控制台）

spec 里是 `console=False`，所以 exe 不再弹控制台，而是常驻**系统托盘**：

- **左键单击 / 双击图标**：打开界面
- **右键菜单**：打开界面 / 打开数据目录 / 退出

首次运行还没填 API Key 时会自动打开一次浏览器，否则你对着图标不知道从哪进。
之后就是托盘常驻、需要时再开。

托盘实现在 `server/tray.py`，**纯 ctypes 调 Win32，不引入任何第三方包**（pystray/Pillow 都不需要）。
图标是 `server/app.ico`，由纯标准库生成的（十六进制结构合法、4 个尺寸），
加载失败时会退回系统默认图标。

`--no-window` 仍然有效：加它就不起托盘、不开浏览器，适合当纯后台服务跑。

## 出安卓包

不用装 Android Studio。推一个 `v*` tag，或手动跑 Actions 的 `build` 工作流：

```bash
git tag v0.2.0
git push origin v0.2.0
```

- **tag 触发**：两个产物都构建，产物在 Actions artifact 里（90 天后清理）。
- **手动触发**：同上。

> 当前 workflow 权限是 `contents: read`，**不发 Release**。要自动发版需要你显式授权：
> 给 `release` job 加 `permissions: contents: write`，并加下面这一步。
>
> ```yaml
>   release:
>     if: startsWith(github.ref, 'refs/tags/v')
>     needs: [windows, android]
>     runs-on: ubuntu-latest
>     permissions:
>       contents: write
>     steps:
>       - uses: actions/download-artifact@v4
>         with:
>           path: artifacts
>       - run: |
>           mkdir -p out
>           cp artifacts/dsh-chat-windows/dsh-chat-win.zip out/
>           cp artifacts/dsh-chat-android/app-debug.apk out/dsh-chat-android.apk
>       - env:
>           GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
>         run: gh release create "$GITHUB_REF_NAME" out/* --generate-notes
> ```

本地构建也可以，需要 JDK 17 + Android SDK，然后：

```bash
cd android && gradle assembleDebug
```

产物：`android/app/build/outputs/apk/debug/app-debug.apk`。

### 安卓壳做了什么

1. 首次启动把 `assets` 里的 `www/` 解压到应用私有目录（APK 内部只读）。
2. 用 Chaquopy 启动 `server.py`，参数为
   `--no-window --data <filesDir>/data --www <filesDir>/www --port 8790`。
3. 轮询 `/local/status`，就绪后用 WebView 加载 `http://127.0.0.1:8790/`。

### 目录约定

`server.py` 的**唯一源头**是 `server/server.py`。

Chaquopy 只认 `app/src/main/python/`，所以 `android/app/build.gradle` 里注册了一个
`syncServerPy` 任务，在 `preBuild` 之前自动把 `server/server.py` 复制过去。
本地和 CI 走同一条路，不存在两处手改。

仓库里仍然保留着一份 `android/app/src/main/python/server.py`（方便不跑 Gradle 时也能直接看），
CI 里有一个 `diff` 步骤确认它和源头一致——不一致会直接失败。
**改了 `server/server.py` 后，重新跑一次 Gradle 或手动复制这份即可。**

`android/app/build.gradle` 里没有 `python { }` 块：`server.py` 只用标准库，
没有 pip 依赖，`buildPython` 用 Chaquopy 的默认值即可。

### 几个已知点

- APK 用的是 **debug 签名**（Android SDK 的公共调试密钥）。自己用没问题；要和 release 包共存得先卸载。
- 同步端口 8791 在手机上仍然监听 `0.0.0.0`，且**没有鉴权**。只在信任的局域网里开。
- 应用切到后台后系统可能冻结 Python 线程，同步会暂停；回到前台自动恢复。
- `--www` 参数是为安卓加的（默认仍指向包内 `www/`），桌面行为不变。

## 密钥与数据

`server.py` 运行时在 `<data>/meta/secret.json` 写 API Key。**这个目录已在 `.gitignore` 里**（`server/data/`），不要提交。
