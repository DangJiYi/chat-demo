# 构建说明

仓库里现在有三块东西：

```text
src/            前端源码（React + rspack）
www/            前端预构建产物（src 的 dist，随包发布；与 python 版一致）
server/         Python 本地服务（server.py + PyInstaller 配置）
android/        Chaquopy 安卓壳（把 server.py + www 打进 APK）
```

## 改前端之后要重建 www

`www/` 是构建产物（源码在 `src/`），两个打包产物都用它。改了 `src/` 必须重建，
否则改了等于没改。

双击工作区根目录的 `build-web.bat`（等价于下面这些）：

```bash
cd chat
pnpm install --frozen-lockfile        # 首次
rm -rf dist && DSH_LOCAL=1 NODE_ENV=production ./node_modules/.bin/rspack build -c rspack.config.ts
rm -rf www && cp -r dist www
```

**`DSH_LOCAL=1` 不能漏。** 它把 `apiOrigin` 置为空字符串，前端才会用同源相对路径
（`/models`、`/anthropic/v1/...`），由 `server.py` 反向代理并注入服务端保存的 Key。
漏了就会构建成直连 `https://api.deepseek.com`：前端把哨兵值 `__server__` 当 Key 发到上游，
聊天永远 401「API Key 无效或已被删除」。

检查方法（应为 0）：

```bash
grep -c "https://api.deepseek.com" www/static/js/main.*.js
```

**不要直接跑 `pnpm build`**：它的脚本里是 `rm -rf dist && NODE_ENV=production ...`，
这是 POSIX 写法，在 cmd.exe 里两个都会失败，而且没有 `DSH_LOCAL`。

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

`server.py` 在仓库里有两份，内容必须完全一致：

```text
server/server.py                          Windows 打包用（PyInstaller）
android/app/src/main/python/server.py     Chaquopy 只认这个默认目录
```

**为什么不在构建时自动复制**：试过用 Gradle 的 `syncServerPy` 任务在 `preBuild` 前复制，
但 `src/main/python` 会同时成为该任务的输出和 Chaquopy 的输入，Gradle 判定为隐式依赖
并直接拒绝构建（`implicit_dependency`）。所以保留两份提交，用 CI 的 `diff` 步骤防漂移。

**改了 `server/server.py` 后，记得同步 Android 那份：**

```bash
cp server/server.py android/app/src/main/python/server.py
```

漏了会怎样：CI 的 `Check server.py copies are in sync` 步骤直接失败，不会静默放过。

`android/app/build.gradle` 里没有 `python { }` 块：`server.py` 只用标准库，
没有 pip 依赖，`buildPython` 用 Chaquopy 的默认值即可。

`server/tray.py` 不需要同步——它只在 Windows 上用。Android 上 `start_tray()`
在 `sys.platform != "win32"` 时直接返回 `None`，不会去 import 它。

### APK 体积

实测数据（`apk-contents` artifact，arm64-only 构建）：

```text
解压总计      17.13 MB   148 个文件
  lib/arm64-v8a      7.16 MB   libpython 3.10 + libcrypto 2.25 + libsqlite3 1.12
  assets/chaquopy    6.47 MB   stdlib-common 2.82 + stdlib-arm64 1.38 + bootstrap 1.14
  assets/static      3.44 MB   前端（已只留 woff2）
  classes.dex 等     0.04 MB
```

所以 APK 本体约 17 MB（不压缩）/ 约 10 MB（下载）。
**如果手机设置里显示 50 MB+，那是“APK + 解压出来的 .so + 数据”的合计，不是 APK 大小。**

已经做过的：

- **只出 `arm64-v8a`**（`android/app/build.gradle` 的 `abiFilters`）。
  Chaquopy 的 `libpython` 每个 ABI 一份，带 `armeabi-v7a` 等于 native 部分翻倍。
  2020 年后的安卓机都是 arm64；要兼容 32 位老机器就改回两个。
- **字体只留 woff2**。KaTeX 每个字族都同时发 woff2/woff/ttf，CSS 把 woff2 排第一，
  另两种永远不会被加载（~900 KB）。构建脚本会自动删。
- **`useLegacyPackaging false`**：`.so` 不再解压到应用目录，直接从 APK 加载。
  下载量 +4 MB，但手机占用 -7 MB（不保留副本）。

剩下的都是 Chaquopy 自带、改不动的东西：`libpython` 3.10 MB、stdlib 4.2 MB、
`libsqlite3` 1.12 MB（`server.py` 不用 sqlite，但 Chaquopy 标准库是整包发的）、
`libcrypto`+`libssl` 2.77 MB（HTTPS 代理必需）。

**还要更小只能换路子**：去掉 Python 运行时（换成 Kotlin/Go 原生实现），
那不是一个配置能解决的。

**debug 与 release 的差异。** debug 构建不剥离 native 符号。当前 CI 出的是
`assembleDebug`（免签名、直接能装）。要 strip 需要 release 构建 + 自己的签名密钥：

```bash
keytool -genkeypair -keystore release.keystore -alias dsh -keyalg RSA -keysize 2048 \
        -validity 10000 -storepass <你的密码> -keypass <你的密码>
```

把 keystore 和密码存进仓库 Secrets，CI 里还原并加 `signingConfig`。
一次性配置，之后每次出包都是精简版。**注意：从 debug 签名换到自己的签名，
手机上必须先卸载旧版再装**（签名不同不能覆盖安装）。

### 几个已知点

- APK 用的是 **debug 签名**（Android SDK 的公共调试密钥）。自己用没问题；要和 release 包共存得先卸载。
- 同步端口 8791 在手机上仍然监听 `0.0.0.0`，且**没有鉴权**。只在信任的局域网里开。
- 应用切到后台后系统可能冻结 Python 线程，同步会暂停；回到前台自动恢复。
- `--www` 参数是为安卓加的（默认仍指向包内 `www/`），桌面行为不变。

### 局域网自动发现（免手填 IP）

同步端口旁多了一个 UDP 发现端口 **8792**：

- 一台机器广播 `DSH-CHAT-DISCOVER/1`，另一台应答自己的设备名与同步端口
- 界面上点「搜索附近设备」→ 列表里点一下即发起配对
- **配对是请求制（像蓝牙）**：发起方立即指向对端；对端只会收到一条待确认请求，
  要在它自己的界面上点「接受」才生效

为什么对端不直接改配置：同步端口本来就无鉴权，任何局域网内的人都能 POST。
若请求到达就直接改配置，等于别人可以把你指向任意服务器。
所以 `/peer` 只把请求放进待确认队列，不会自动生效。

**配对请求在同步端口（8791）受理**，不是 UI 端口——对方只知道 8791。

排查：搜不到通常是 **Windows 防火墙拦了 UDP 8792**，或两台不在同一子网（访客 Wi-Fi 常见），
或热点开启了 AP 隔离。不想用发现也可以照旧手填 IP。

### 本机名称与同步端口

「本地与同步」面板的本机一栏可以改：

- **名称**：显示在对方的发现列表里，立即生效
- **同步端口**：保存在 `config.json`，**重启客户端后生效**

改了端口后，对方的配对信息会失效，需要重新配对。

### 同步后界面自动刷新

前端每 5 秒（仅在页面可见时）拉一次会话摘要，并重读当前会话，
所以对端新消息不用关掉再打开。自己正在流式生成时不会被打断。

## 密钥与数据

`server.py` 运行时在 `<data>/meta/secret.json` 写 API Key。**这个目录已在 `.gitignore` 里**（`server/data/`），不要提交。
