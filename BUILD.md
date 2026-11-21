# 构建说明

仓库里现在有三块东西：

```text
src/            前端源码（React + rspack）
www/            前端预构建产物（src 的 dist，随包发布；与 python 版一致）
server/         Python 本地服务（server.py + PyInstaller 配置）
android/        Chaquopy 安卓壳（把 server.py + www 打进 APK）
```

## 改前端之后要重建 www

**打包时不需要你重建。** CI 的 `web` job 会从 `src/` 构建前端，
再把它覆盖到 `www/`，然后 windows / android 两个 job 才开打。
所以仓库里的 `www/` 只是“参考产物”，忘了重建也不会打出错界面。

本地想自己看效果的话，双击工作区根目录的 `build-web.bat`（等价于下面这些）：

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

## Rust 版（进行中，逐步替代 server.py）

`rust/` 是后端重写工程，目标是用一份代码同时覆盖 Windows exe 和安卓 `.so`，
摆脱 Python 运行时（约省 13.6 MB）。

**进度：Windows 侧功能已与 Python 版对齐（含同步、配对、归档）。**

UI 端口（默认 8790，只听 `127.0.0.1`）：

```text
GET    /                        静态文件（含 SPA fallback 与目录穿越防护）
GET    /static/*                长缓存资源
GET    /local/status            服务状态（含 localIps / deviceName）
GET    /local/storage           存储占用
GET    /local/config            读配置                PUT 改配置
GET    /local/apikey            Key 状态            PUT 存 Key  DELETE 删除
GET    /local/usage             用量与费用（会话/今日/本月）
GET    /local/balance           余额（60 秒缓存）
GET    /local/search            全文搜索（活跃 + 归档）
GET    /local/archived          归档列表
POST   /local/restore/<id>      从归档恢复
POST   /local/delete            批量删除（区分归档与会话）
GET    /local/sync/status       同步状态            POST /local/sync/now 立即拉一次
GET    /local/discover          UDP 广播找同网段设备
POST   /local/pair              指向对端并反向请求配对
GET    /local/pair/requests     待确认的配对请求
POST   /local/pair/accept       接受配对（此时才写进配置）
POST   /local/pair/reject       拒绝配对
POST   /local/archive/now       立即归档 + 去重
POST   /local/images/now        重压缩旧 PNG
GET    /local/orphans           无人引用的附件（只统计）
GET    /health                  存活检查
GET    /db/summaries            会话摘要（扫目录，按 updatedAt 倒序）
GET    /db/session/<id>         读会话原文
PUT    /db/session/<id>         存会话（校验 id 一致）
POST   /db/session/<id>/rename  改标题
DELETE /db/session/<id>         删会话（写墓碑，不删文件）
GET    /db/file/<id>            读附件（内容寻址，长缓存）
PUT    /db/file/<id>            存附件
POST   /db/staging/clear        清空导入暂存
PUT    /db/staging/session/<id> 暂存会话
PUT    /db/staging/file/<id>    暂存附件
POST   /db/staging/filetypes    回填附件 MIME
POST   /db/staging/commit       暂存 → 正式
GET    /models                  → 转发 api.deepseek.com/models（校验 Key）
*      /anthropic/*             → 转发上游（聊天 SSE、附件上传、Files API）
*      /v1/*                    → 转发上游（语音合成等）
```

同步端口（默认 UI 端口 + 1，听 `0.0.0.0`，对局域网开放）：

```text
GET  /health            存活检查
GET  /sync/since?since= 增量拉取：会话 + 附件元数据 + 游标
POST /sync/merge        推送合并
POST /peer              配对请求（只入待确认队列，不改配置）
GET  /file/<sha256>     按内容取附件原件
```

**没有鉴权**，所以同步端口只给「读同步数据 + 入队配对请求」；改配置必须由
本机用户在界面上点接受。这一点和蓝牙配对是同一个道理。

### 反向代理（本地构建的关键一环）

本地构建时 `__API_ORIGIN__` 是空串（见 `rspack.config.ts` 的 `DSH_LOCAL=1`），
前端**不直连上游**，而是把聊天、附件上传、语音、Key 校验都发到同源路径：
`/models`、`/anthropic/*`、`/v1/*`。服务端把它们转发到 `api.deepseek.com`，
并把 Key 注入进去。

少了这一段的表现很容易误判：这些路径在服务端不存在，GET 会落到 SPA 回退
（返回 index.html，200），POST 则 404 `{"error":"not found"}`。
于是界面报「无法对话」或「API Key 无效或已被删除」——看起像 Key 的问题，
实际是根本没有代理。

几个必须保住的细节：

- **流式回传**。聊天走 SSE，先收完整个响应再返回的话，要等回答结束才出字。
  实现上给 `Response::new` 传 `data_length = None`，tiny_http 用 chunked
  边收边发。
- **凭据优先级：浏览器带来的真凭据 > 服务端保存的 Key**。
  `verifyApiKey()` 只带 `Authorization`、不带 `x-api-key`；哨兵值 `__server__`
  不算凭据。不这么判的话，验证新 Key 时会被旧 Key 顶替，用户永远换不掉 Key。
- **上游 4xx/5xx 原样透传**（状态码 + 响应体），不包装成 502。
  否则界面只能看到“网络错误”，看不到上游说的“Key 无效”。
- **每个请求一个线程**。代理是流式的，一次回答可能持续几分钟；
  串行处理会让自动保存、侧边栏状态查询全卡住——表现为「一提问界面就死」。
- 附件上传按 128 MB 上限读请求体（与会话的 32 MB 分开）。

#### TLS：一个很容易踩穿的点

ureq 的 `native-tls` 特性**不会自动成为默认 TLS 后端**。它自己的源码里写着：

> native-tls is a feature that must be configured via the AgentBuilder.
> it is never picked up as a default (and never used by `ureq::get` etc).

只开 feature 不传连接器，任何 HTTPS 请求都会在运行时失败：

```
Unknown Scheme: cannot make HTTPS request because no TLS backend is configured
```

所以有两个 agent 构造器，不要直接用 `ureq::get` 之类的快捷函数：

- `http::upstream_agent(...)` —— 发往 `api.deepseek.com`（余额、代理），
  带系统 TLS + 读系统代理（`HTTP_PROXY` 等，与 Python 的 urllib 一致）。
- `sync::lan_agent(...)` —— 发往局域网对端，**刻意关掉系统代理**：
  对端在 10.x/192.168.x，走代理会送到公司代理/科学上网去。

`http.rs` 里有个单测 `tls_backend_is_configured` 守着这一条：
它不联网，只断言 `native_tls::TlsConnector::new()` 能成功。
丢了 TLS 配置时测试直接红，不用等到用户发现聊天全挂。

### 两条前缀，别弄混

前端用两套前缀，服务端必须都认：

- **`/local/*`** —— 本机设置类接口（`src/local/api.ts`）
- **`/local/db/*`** —— 数据层接口（`src/services/chat/db.ts` 里 `BASE = '/local/db'`）

Python 版的做法是分发前统一剥掉 `/local`，于是 `/local/db/summaries` 落到
`/db/summaries`。Rust 版跟着做同样的事（源码里只写一次 `/db/*` 路由）。

这东西踩过一次，值得写下来：漏剥前缀时不是「安静 404」，而是 ——
`db.ts` 对 404 是刻意不抛错的（用来表达「这条会话不存在」），于是 404 的
错误体被当成数组去 `.map()`，界面上报的是「对话列表加载失败」。
排查时很难想到根因在路由前缀上。

之前的 CI 全打 `/db/*`，所以这件事一路绿着进了产物。
`tests/smoke.rs` 的 `frontend_local_db_prefix_is_served` 现在直接按前端真实路径断言。

还差：安卓壳（`cargo-ndk` + 薄 Kotlin 层）。

### 计费口径与测试

`src/billing.rs` 里集中了时段、节假日、价目。日期换算用 Howard Hinnant 的
`civil_from_days` 算法（约 20 行），不引 chrono——只为了算北京时间的年月日和星期，
多一个依赖不划算。

这四个单元测试是计费的「自证」：

- `civil_dates_match_known_values`：1970-01-01 / 2000-03-01（闰年边界）/ 2026-11-21
- `weekday_matches_python_convention`：周一=0，与 Python `weekday()` 一致
- `peak_rules`：周五 10 点峰、13 点谷、周六全谷
- `cost_is_halved_off_peak`：1M 未命中峰时 2 元、谷时 1 元

期望值都是先用 Python 的 `datetime` 算出来再写进去的，不是拍脑袋。

### 测试方式

`tests/smoke.rs` 起真实二进制、发真实 HTTP 请求，覆盖静态文件、会话 CRUD、
附件内容寻址、导入暂存、配置、Key、用量、搜索、归档恢复。

`tests/sync_e2e.rs` 起**两个**实例（A、B 各占一个端口），让 A 去拉 B：
会话（含回答）、附件（要说按 id 读得回来，不只是落盘）、配对队列（请求不得
擅自改配置，接受后才生效）、归档（旧会话搬走并从活跃列表消失）。

断言放在 Rust 里而不是 CI 的 YAML 里：YAML 里的 PowerShell 断言很难写对类型
（`byte[]` vs `string`、`@()` 包数组、`Uri` 预先规范化 `..`），而且只有推上去才知道错。

`commit_staging` 与 Python 版有一处刻意的差别：Python 先删光 `sessions/` 再写，
中途失败数据就没了；Rust 先把暂存会话全部写入，最后才删「不属于新集合」的旧文件。
要么新数据完整落盘，要么旧数据还在。

### 一把锁守 `index.json`

三个地方会并发跑 `pull_once`：后台 `sync_loop`、`POST /local/sync/now`
（每点一次新起一个线程）、配对接受后。而归档、同步都在改同一份
`meta/index.json`（一个写 `archive` 键，一个写 `files` 键），且都是
「读整个文件 → 改一个键 → 写回」。不串行就会丢键，表现为「附件全部 404」
或「归档列表空了」，**只在并发时偶发**——最难查的那种。

所以用一个 `pull_lock` 把「拉取 + 合并 + 写索引」与归档串起来：

- `pull_once`：阻塞拿锁（手动触发就该真的跑一趟）
- `try_pull_once`：错了就跳过（后台循环用。对端离线时一轮要卡到网络超时，
  阻塞拿锁会让循环每 3 秒叠一个线程上去）
- `/sync/merge`、`/local/archive/now`、启动后的 `archive_tick` 也拿同一把锁

锁中毒时用 `into_inner()` 放行：中毒说明上个持锁线程 panic 了，但数据本身是
文件，不会因此不一致；宁可能拉一次，也不要让同步线程全部停掉。

其余端点按 `server.py` 逐个移植。**协议必须一致**——前端的
`src/local/api.ts` 与 `src/services/chat/db.ts` 不改一行。

### 编译与验证：走 CI

本地调试有两个阻碍，所以骨架阶段直接交给 CI：

1. 本机缺 MSVC 链接器（rustup 装了 `x86_64-pc-windows-msvc`，但没用 VS Build Tools），
   `cargo build` 会在链接阶段失败
2. 执行 `cargo build` 会在工作区里跑构建脚本，权限策略会拦

`.github/workflows/rust.yml` 做四件事：`clippy`（只把 correctness/suspicious
两组当错误，其余风格问题不拦）→ `cargo test`（单测 + 两个端到端测试）
→ `cargo build --release` → **真实启动服务并用 HTTP 请求断言** → 上传 exe artifact。

自检不只验证“能编译”，而是真的造两个会话文件、起服务、请求两个端点，
断言：返回 2 条、按 `updatedAt` 倒序（s2 在前）、中文标题正确、未知路径返回 404。

跑法：Actions → `rust` → Run workflow。

本地想自己跑的话，先装 VS Build Tools（勾 MSVC v143 + Windows SDK），再：

```bash
cd rust && cargo run -- --port 8790
```

**默认路径不依赖当前目录。** 两者都以 **exe 所在目录**为基准：

- **`www`：先找 exe 同级，再向上最多 5 层**。开发时 exe 在 `rust/target/release/`，
  而 `www` 在 `chat/`，向上找免去每次手写 `--www ../www`。
- **`data`：只用 exe 同级，绝不向上找**。向上扫会撞到无关目录。

数据目录那条是踩出来的：exe 放在 `D:\Downloads\dsh-chat-rust-windows\`，
往上扫到了 `D:\Downloads\data`，把数据目录指向了一个跟本应用无关的文件夹——
用户看到空会话列表，以为聊天记录丢了。**数据目录宁可在 exe 旁新建，也不能猜。**

CI 里有条回归断言专门盯这个：在上级放一个诱饵 `data/`，断言 `dataDir`
仍是 exe 同级而不是诱饵。

需要指定其他目录时：

```bash
dsh-chat --port 8790 --data <数据目录> --www <前端目录>
```

### 余额查询（`/local/balance`）

上游固定是 `https://api.deepseek.com/user/balance`——没有区域或版本分支。
所以：

- **只缓存成功结果**。失败不入缓存（Python 版也是），否则你换好 Key、
  修好网络后，界面还得干等一分钟才恢复。
- **保存新 Key 时清空缓存**（`save_api_key` 里 `cache_balance(None)`）。
  不清的话，上一次用旧 Key 查出的错误会继续显示 60 秒，容易误判成“新 Key 也不行”。
- **失败会写 `data/app.log`**，且只在错误内容变化时记一行（卡片每 60 秒重查，
  逐次记会把日志刷满）。

错误文案会带原因，不是只给一个状态码：

- `401/403` → 提示 Key 无效、已删或类型不对
- `404` → 提示该接口不存在，多半是请求被中转/代理接走了
  （中转地址不支持查余额，这是上游限制，不是本项目的 bug）
- 连不上 → 带上完整的 URL

如果界面显示「查询失败」，先看 `data/app.log` 最后一行，那里有上游的原话。

### 同步端口绑不上怎么办

同步端口默认是 UI 端口 + 1（8791）。这个端口很热门：本机跑两个实例、别的软件占用、
或者 Windows 把它划进了 Hyper-V 的保留区间（`netsh int ipv4 show excludedportrange`），
都会直接绑不上。

以前的做法是只在日志里记一行就接着跑。后果很阴：界面照样显示 `192.168.x.x:8791`，
用户把这个地址告诉对端，对端连过来得到“拒绝连接”，而本机看起来一切正常。

现在的行为：

1. 先试配置里的端口，不行就往下试 4 个（`bind_sync`）；
2. 换到了别的端口就把实际值写回 `config.json`，否则广播/界面都会报一个没人监听的端口；
3. `/local/status` 报**实际在监听**的 `syncPort`，并额外给 `syncListening`、`syncErr`；
4. 全都绑不上就写日志 + `syncListening: false`，界面不会拿一个假地址去骗对端。

`--sync-port` 可以显式指定（优先于 `config.json`），排查时有用。

这也解释了为什么 `sync_e2e.rs` 不再硬算 `port + 1`：它从 `/local/status` 读实际端口。
硬算的时候，端口一旦回退，测试报的是“连接被拒”，完全看不出服务端没监听。

### Windows 上的托盘与日志

双击 exe 不再是一个关不掉的黑框：托盘图标右键有「打开界面 / 退出」，
双击图标直接开界面。几个刻意的设计：

- **先加图标，再加隐藏控制台**。顺序反了，一旦托盘创建失败（explorer 没起来），
  用户会看到一个既没有窗口也没有图标、只在后台吃端口的进程。
- **控制台只有「独占」时才隐藏**（`GetConsoleProcessList` 返回 1）。
  从 PowerShell / Git Bash 启动时，那个窗口是终端自己的，藏了会连带把终端藏掉。
- **`--console` 保留黑框**。排查启动问题时用。
- **日志在 `data/app.log`**（超 1 MB 轮转成 `app.log.1`）。控制台藏了就只能靠它：
  启动参数、同步失败原因、归档结果都写在这里，界面的「存储占用」也显示它的大小。
  同步失败只在**错误内容变化**时记一行——对端离线时每 3 秒重试一次，
  逐次记录会把日志刷满。
- 托盘只用 `windows-sys` 里几个系统调用，不引 GUI 框架；图标暂时是系统默认图标。

### 设计约束

- **不引 tokio**。并发上限就是「一个 WebView + 一个同步端口」，同步 HTTP 够用。
  `axum`/`hyper` 会多带 1.5 MB，不划算。
- **不改代理层之前不碰 TLS**。原版前端本就直连 `api.deepseek.com`（CORS 是通的），
  去掉代理后 Rust 侧不需要 `rustls`，可再省 1.5 MB。
- **数据目录结构与 Python 版完全一致**，两边能读同一份 `data/`。

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

### 解压的前端资源

安卓读不了 APK 内的 `assets`，所以首次启动会把前端解压到应用私有目录，
再把它作为 `--www` 传给 `server.py`。

**只拷白名单里的条目**（`index.html`、`LICENSE`、`static`）。

这不是多余的防御。Android 会把 **WebView 提供者的 assets 合并进
`AssetManager.list("")` 的顶层命名空间**，所以直接遍历资产根会拷到：

```text
icudtl.dat+com.google.android.webview+    8.81 MB
telocation.idf                            3.26 MB
stored-locales                            1.32 MB
resources.pak+com.google.android.webview+ 0.41 MB
v8_context_snapshot_64.bin+...            0.20 MB
images                                    0.12 MB
```

这些是 WebView 自己的运行时资源，WebView 从它自己的 APK 里读，
拷到应用目录纯粹是白占空间——界面文件从 3.44 MB 虚增到 17.8 MB（453 个文件）。

目录名带版本戳（`www-<versionCode>-<lastUpdateTime>`），升级后必然换新目录，
`pruneOldWww()` 顺带删掉旧的 `www*`（包括曾经被污染的那份）。
解压完会检查 `index.html` 存在、并统计文件数（正常约 60），
日志里能看到 `前端解压完成：N 个文件`。

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
