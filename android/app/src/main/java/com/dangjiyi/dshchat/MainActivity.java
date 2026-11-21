package com.dangjiyi.dshchat;

import android.app.Activity;
import android.content.pm.PackageInfo;
import android.content.res.AssetManager;
import android.graphics.Color;
import android.net.http.SslError;
import android.os.Bundle;
import android.util.Log;
import android.view.KeyEvent;
import android.view.View;
import android.view.ViewGroup;
import android.webkit.ConsoleMessage;
import android.webkit.SslErrorHandler;
import android.webkit.WebChromeClient;
import android.webkit.WebResourceError;
import android.webkit.WebResourceRequest;
import android.webkit.WebSettings;
import android.webkit.WebView;
import android.webkit.WebViewClient;
import android.widget.FrameLayout;
import android.widget.ScrollView;
import android.widget.TextView;

import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.HttpURLConnection;
import java.net.URL;
import java.nio.charset.StandardCharsets;

/**
 * 宿主壳：拉起 Rust 写的本地服务（`libdsh_chat.so`），再用 WebView 加载
 * `http://localhost:8790/`。
 *
 * 分工：
 *   - 服务端（HTTP/同步/归档/反向代理）全在 .so 里，见 `rust/src/lib.rs`
 *   - 前端已内嵌进 .so，这里不再解压 assets
 *   - 本类只负责：加载 .so、传数据目录与端口、轮询就绪、装 WebView
 */
public class MainActivity extends Activity {

    private static final String TAG = "dsh-chat";
    // 必须用主机名 localhost，不能用 IP：
    // Android 的 network-security-config 里 <domain> 只接受主机名，
    // 写 127.0.0.1 会被静默忽略 → 明文被拦 → WebView 白屏。
    // 代价是 localhost 可能解析到 ::1，所以服务端同时监听 IPv4/IPv6
    // （见 rust/src/lib.rs 的 serve：127.0.0.1 必须成功，[::1] 尽力而为）。
    private static final String HOST = "localhost";
    private static final int PORT = 8790;
    private static final long BOOT_TIMEOUT_MS = 30_000L;

    private WebView webView;
    private TextView errorView;
    // 盖在 WebView 上面的诊断遮罩；成功加载后必须整个隐藏，否则会吃掉所有触摸
    private ScrollView overlay;
    private final StringBuilder diag = new StringBuilder();
    // Rust 服务启动失败的原因（从 native 侧读回来），失败时展示到界面上
    private volatile String bootError = null;
    // 服务和线程是进程级的；Activity 重建时不能重复启动
    private static volatile boolean sBooted = false;

    /**
     * Rust 服务。`.so` 由 cargo-ndk 交叉编译产出，见 `rust/src/jni.rs`。
     *
     * nativeStart 只负责把服务线程拉起来（立即返回），错误通过
     * nativeLastError 回传——轮询循环一旦拿到错误就立刻停，
     * 不用把真正的原因埋在一句「30 秒超时」里。
     */
    private native int nativeStart(String dataDir, int port);

    private native String nativeLastError();

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);

        FrameLayout root = new FrameLayout(this);
        webView = new WebView(this);
        errorView = new TextView(this);
        root.addView(webView, new FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT));
        // 诊断面板可滚动，白屏时能看清到底哪里出错。
        // 必须提升为字段：它是全屏盖在 WebView 上的，遮罩时必须整个 GONE 掉，
        // 只藏里面的 TextView 不够 —— ScrollView 仍会拦截触摸，导致界面点不动。
        overlay = new ScrollView(this);
        overlay.addView(errorView);
        root.addView(overlay, new FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT));
        errorView.setVisibility(View.VISIBLE);
        errorView.setPadding(40, 80, 40, 40);
        errorView.setTextColor(Color.parseColor("#333333"));
        errorView.setTextSize(13f);
        errorView.setTextIsSelectable(true);
        errorView.setText("正在启动本地服务…\n\n首次启动需要解压前端资源，可能要几秒。");
        setContentView(root);

        WebSettings settings = webView.getSettings();
        settings.setJavaScriptEnabled(true);
        settings.setDomStorageEnabled(true);
        settings.setDatabaseEnabled(true);
        settings.setAllowFileAccess(false);
        settings.setAllowContentAccess(false);
        settings.setMediaPlaybackRequiresUserGesture(false);
        webView.setBackgroundColor(Color.WHITE);

        // 关键：没有这些回调，加载失败就只是一片白，什么都看不到
        webView.setWebViewClient(new WebViewClient() {
            @Override
            public void onReceivedError(WebView view, WebResourceRequest request,
                                        WebResourceError error) {
                if (request != null && request.isForMainFrame()) {
                    showDiag("页面加载失败\n\n"
                            + "URL: " + request.getUrl() + "\n"
                            + "错误: " + error.getErrorCode() + " " + error.getDescription());
                } else if (request != null) {
                    diag.append("资源失败: ").append(request.getUrl()).append(" — ")
                        .append(error.getDescription()).append('\n');
                }
            }

            @Override
            public void onReceivedSslError(WebView view, SslErrorHandler handler, SslError error) {
                handler.cancel();
                showDiag("SSL 错误: " + error);
            }

            @Override
            public void onPageFinished(WebView view, String url) {
                // 延迟再检查：#root 是 React 挂载的，onPageFinished 可能早于首次渲染。
                // 太早检查会把“还没渲染”误判成“渲染失败”。
                view.postDelayed(() -> probeRendered(view), 800);
            }
        });

        webView.setWebChromeClient(new WebChromeClient() {
            @Override
            public boolean onConsoleMessage(ConsoleMessage m) {
                String line = "[" + m.messageLevel() + "] " + m.message()
                        + " @" + m.sourceId() + ":" + m.lineNumber();
                Log.w(TAG, line);
                diag.append(line).append('\n');
                return true;
            }
        });

        new Thread(this::boot, "dsh-boot").start();
    }

    private void boot() {
        try {
            setStatus("正在准备数据目录…");
            extractAssets();
            if (sBooted) {
                // 服务已经在跑（Activity 重建），直接等它就绪即可
                setStatus("正在等待本地服务…");
                waitForServer(BOOT_TIMEOUT_MS);
            } else {
                setStatus("正在启动本地服务…");
                startNative();
                setStatus("正在等待本地服务就绪…");
                waitForServer(BOOT_TIMEOUT_MS);
                sBooted = true;
            }
            setStatus("服务已就绪，正在加载界面…");
            runOnUiThread(() -> webView.loadUrl("http://" + HOST + ":" + PORT + "/"));
        } catch (Throwable t) {
            Log.e(TAG, "启动失败", t);
            runOnUiThread(() -> showError(t));
        }
    }

    /** 更新启动提示（遮罩上的文字）。已经在 UI 线程时要小心重复 post。 */
    private void setStatus(String text) {
        Log.i(TAG, text);
        runOnUiThread(() -> {
            if (errorView.getVisibility() == View.VISIBLE) {
                errorView.setText(text);
            }
        });
    }

    private void showError(Throwable t) {
        StringBuilder sb = new StringBuilder();
        sb.append("启动失败：").append(t).append("\n\n");
        sb.append("地址：http://").append(HOST).append(':').append(PORT).append("/\n");
        sb.append("数据：").append(dataDir().getAbsolutePath()).append("\n");
        sb.append("静态：内嵌于 libdsh_chat.so\n");
        if (bootError != null) {
            sb.append("\n--- 启动错误 ---\n").append(bootError);
        }
        String appLog = tailAppLog(40);
        if (!appLog.isEmpty()) {
            sb.append("\n--- app.log ---\n").append(appLog);
        }
        if (diag.length() > 0) {
            sb.append("\n--- 页面日志 ---\n").append(diag);
        }
        String text = sb.toString();
        Log.e(TAG, text);
        runOnUiThread(() -> {
            webView.setVisibility(View.GONE);
            if (overlay != null) {
                overlay.setVisibility(View.VISIBLE);
            }
            errorView.setVisibility(View.VISIBLE);
            errorView.setText(text);
        });
    }

    /** 读 data/app.log 的最后几行。Python 的 print 在安卓上只进 logcat，
     *  但 App.log 会同时写这个文件，所以这是拿 Python 内部状态的唯一途径。 */
    private String tailAppLog(int maxLines) {
        File f = new File(dataDir(), "app.log");
        if (!f.isFile()) {
            return "";
        }
        try (java.io.BufferedReader r = new java.io.BufferedReader(
                new java.io.InputStreamReader(new FileInputStream(f), StandardCharsets.UTF_8))) {
            java.util.ArrayDeque<String> ring = new java.util.ArrayDeque<>();
            String line;
            while ((line = r.readLine()) != null) {
                ring.addLast(line);
                if (ring.size() > maxLines) {
                    ring.removeFirst();
                }
            }
            StringBuilder sb = new StringBuilder();
            for (String l : ring) {
                sb.append(l).append('\n');
            }
            return sb.toString();
        } catch (IOException e) {
            return "(读取失败: " + e + ")";
        }
    }

    /**
     * 检查 #root 是否真的渲染出内容：
     *   有子节点 -> 隐藏整个遮罩（含 ScrollView），让 WebView 接管触摸
     *   为空     -> 显示诊断，并保留遮罩
     */
    private void probeRendered(WebView view) {
        view.evaluateJavascript(
                "(function(){var r=document.getElementById('root');"
              + "return JSON.stringify({root:r?r.childElementCount:-1,"
              + "body:document.body?document.body.innerText.length:0});})()",
                value -> {
                    boolean empty = value == null || value.contains("\"root\":0");
                    if (empty) {
                        showDiag("页面已加载，但界面没有渲染出来。\n\n"
                                + "这通常意味着 JS 执行出错。\n"
                                + "请查看下方日志，或运行：\n"
                                + "adb logcat -s dsh-chat\n\n" + diag);
                        return;
                    }
                    Log.i(TAG, "界面已渲染，移除遮罩: " + value);
                    hideOverlay();
                });
    }

    /** 隐藏整个诊断遮罩。必须连 ScrollView 一起隐藏，否则它仍会拦截触摸。 */
    private void hideOverlay() {
        runOnUiThread(() -> {
            if (overlay != null) {
                overlay.setVisibility(View.GONE);
            }
            webView.setVisibility(View.VISIBLE);
        });
    }

    /** 显示完整诊断信息（含已收集的 console 日志）。必须在 UI 线程调用。 */
    private void showDiag(String text) {
        Log.e(TAG, text);
        runOnUiThread(() -> {
            webView.setVisibility(View.GONE);
            errorView.setVisibility(View.VISIBLE);
            errorView.setText(text);
        });
    }

    // ---------------------------------------------------------------- 私有目录

    private File dataDir() {
        return new File(getFilesDir(), "data");
    }

    /**
     * 把 www/ 从 assets 解压到私有目录。目录名带版本戳，
     * 所以升级后一定是全新目录，并顺手清掉旧版本残留。
     */
    private void extractAssets() throws IOException {
        // 前端已经内嵌在 .so 里（`rust/build.rs` 把 `www/` 编进二进制），
        // 不再需要从 assets 解压到私有目录：
        //   - 省掉那 3.4 MB 的重复占用（系统会把它算进“用户数据”）
        //   - 省掉每次升级后的一次性拷贝
        //   - 顺带消掉“白名单抄错就把 14 MB WebView 资产拷进来”的整个风险面
        //
        // 下面的 copyAssetDir / pruneOldWww 等方法暂时保留（已无调用点），
        // 等这版跑通再一并清理——一次只改一件事。
        ensureDataDir();
    }

    /** 统计目录下的文件数，用于自检。 */
    private static int countFiles(File dir) {
        int n = 0;
        File[] kids = dir.listFiles();
        if (kids == null) {
            return 0;
        }
        for (File f : kids) {
            n += f.isDirectory() ? countFiles(f) : 1;
        }
        return n;
    }

    /** 删掉 filesDir 下除当前之外的所有 www* 目录（旧版本残留、旧固定名 www、临时目录）。 */
    private void pruneOldWww(File keep) {
        File[] kids = getFilesDir().listFiles();
        if (kids == null) {
            return;
        }
        for (File f : kids) {
            String n = f.getName();
            if (!n.startsWith("www") || f.equals(keep)) {
                continue;
            }
            boolean ok = deleteTree(f);
            Log.i(TAG, "清理旧资源 " + n + " -> " + (ok ? "ok" : "部分失败"));
        }
    }

    private void ensureDataDir() {
        if (!dataDir().isDirectory()) {
            // server.py 自己也会建，这里提前建好省得首屏报错
            //noinspection ResultOfMethodCallIgnored
            dataDir().mkdirs();
        }
    }

    /**
     * 解压标记：versionCode + lastUpdateTime。
     * 只用 versionCode 的话，重装同一个 versionCode 的包不会重新解压，
     * 修好的 assets 永远进不了设备——这是个很难查的坑。
     */
    /** 缓存一份：应用版本戳只查一次，不必反复问 PackageManager。
     *  目前没有调用点（前端已内嵌，不再需要版本化目录），
     *  等这版跑通后随 copyAssetDir 一起清掉。 */
    private volatile String cachedStamp;

    private String appStamp() {
        String s = cachedStamp;
        if (s != null) {
            return s;
        }
        try {
            PackageInfo info = getPackageManager().getPackageInfo(getPackageName(), 0);
            s = info.versionCode + "-" + info.lastUpdateTime;
        } catch (Exception e) {
            s = "0";
        }
        cachedStamp = s;
        return s;
    }

    /**
     * 我们自己的前端资源顶层条目。**只拷这些。**
     *
     * 为什么必须用白名单：Android 会把 WebView 提供者的 assets 合并进
     * AssetManager.list("") 的顶层命名空间。实际看到的条目：
     *   icudtl.dat+com.google.android.webview+   8.81 MB
     *   telocation.idf                             3.26 MB
     *   stored-locales                             1.32 MB
     *   resources.pak+com.google.android.webview+  0.41 MB
     *   v8_context_snapshot_64.bin+...             0.20 MB
     *   images                                     0.12 MB
     * 这些不是我们的东西，却被拷进了应用私有目录：
     * 界面文件从 3.44 MB 虚增到 17.8 MB，453 个文件。
     *
     * Chaquopy 的 assets/chaquopy 同理，也不能拷（那是 Python 运行时）。
     */
    private static final java.util.Set<String> OWN_ASSETS =
            new java.util.HashSet<>(java.util.Arrays.asList("index.html", "LICENSE", "static"));

    /**
     * 递归复制 assets/static 下的内容。
     *
     * AssetManager 对「空目录」和「文件」都返回空数组，本项目没有空目录，
     * 所以用「list 非空即目录」判断足够。
     */
    private void copyAssetDir(String path, File outDir) throws IOException {
        AssetManager am = getAssets();
        String[] children = am.list(path);
        if (children == null) {
            return;
        }
        for (String name : children) {
            // 只拷顶层白名单里的条目，避开 WebView 与 Chaquopy 的资产
            if (path.isEmpty() && !OWN_ASSETS.contains(name)) {
                continue;
            }
            String childPath = path.isEmpty() ? name : path + "/" + name;
            String[] grand = am.list(childPath);
            File out = new File(outDir, name);
            if (grand != null && grand.length > 0) {
                if (!out.isDirectory() && !out.mkdirs()) {
                    throw new IOException("无法创建目录 " + out);
                }
                copyAssetDir(childPath, out);
            } else {
                try (InputStream in = am.open(childPath)) {
                    copyStream(in, out);
                }
            }
        }
    }

    private static void copyStream(InputStream in, File out) throws IOException {
        File parent = out.getParentFile();
        if (parent != null && !parent.isDirectory() && !parent.mkdirs()) {
            throw new IOException("无法创建目录 " + parent);
        }
        try (OutputStream os = new FileOutputStream(out)) {
            byte[] buf = new byte[64 * 1024];
            int n;
            while ((n = in.read(buf)) > 0) {
                os.write(buf, 0, n);
            }
        }
    }

    private static boolean deleteTree(File f) {
        if (f == null || !f.exists()) {
            return true;
        }
        boolean ok = true;
        File[] kids = f.listFiles();
        if (kids != null) {
            for (File kid : kids) {
                ok &= deleteTree(kid);
            }
        }
        return f.delete() && ok;
    }

    private static String readText(File f) throws IOException {
        try (InputStream in = new FileInputStream(f)) {
            ByteArrayOutputStream bos = new ByteArrayOutputStream();
            byte[] buf = new byte[4096];
            int n;
            while ((n = in.read(buf)) > 0) {
                bos.write(buf, 0, n);
            }
            return new String(bos.toByteArray(), StandardCharsets.UTF_8);
        }
    }

    private static void writeText(File f, String text) throws IOException {
        try (OutputStream os = new FileOutputStream(f)) {
            os.write(text.getBytes(StandardCharsets.UTF_8));
        }
    }

    // ---------------------------------------------------------------- Rust 服务

    /** 加载 .so 的结果。只试一次：失败就记下原因，不要反复试。 */
    private static boolean sLibLoaded = false;
    private static String sLibError = null;

    private static synchronized void loadLib() {
        if (sLibLoaded || sLibError != null) {
            return;
        }
        try {
            // 名字必须与 rust/Cargo.toml 里 [lib] name 一致：
            // 那个 name 决定产出的文件名 libdsh_chat_server.so。
            // 写错的话运行时才报 UnsatisfiedLinkError，很难查。
            System.loadLibrary("dsh_chat_server");
            sLibLoaded = true;
        } catch (Throwable t) {
            // 不把 loadLibrary 放在 static 块里：那样一失败就是
            // UnsatisfiedLinkError 直接杀进程，连诊断界面都看不到。
            sLibError = Log.getStackTraceString(t);
        }
    }

    private void startNative() {
        loadLib();
        if (sLibError != null) {
            bootError = "加载 libdsh_chat.so 失败：\n" + sLibError;
            Log.e(TAG, bootError);
            return;
        }
        // 前端已内嵌在 .so 里，只传数据目录和端口
        int rc = nativeStart(dataDir().getAbsolutePath(), PORT);
        if (rc != 0) {
            String detail = nativeLastError();
            bootError = "启动本地服务失败（nativeStart=" + rc + "）"
                    + (detail == null ? "" : "：\n" + detail);
            Log.e(TAG, bootError);
            return;
        }
        Log.i(TAG, "已请求启动 Rust 服务，等待就绪…");
    }

    private void waitForServer(long timeoutMs) throws IOException {
        long deadline = System.currentTimeMillis() + timeoutMs;
        IOException last = null;
        String[] probes = {"127.0.0.1", "localhost"};
        while (System.currentTimeMillis() < deadline) {
            // native 侧可能已经报错，不用白等到超时
            if (bootError != null) {
                throw new IOException("本地服务已退出");
            }
            for (String host : probes) {
                HttpURLConnection conn = null;
                try {
                    conn = (HttpURLConnection) new URL(
                            "http://" + host + ":" + PORT + "/local/status").openConnection();
                    conn.setConnectTimeout(1000);
                    conn.setReadTimeout(2000);
                    if (conn.getResponseCode() == 200) {
                        Log.i(TAG, "本地服务已就绪，探测通过: " + host);
                        return;
                    }
                } catch (IOException e) {
                    last = e;
                } finally {
                    if (conn != null) {
                        conn.disconnect();
                    }
                }
            }
            try {
                Thread.sleep(300);
            } catch (InterruptedException e) {
                throw new IOException("等待被中断", e);
            }
        }
        throw new IOException("本地服务 " + (timeoutMs / 1000) + " 秒内未就绪", last);
    }

    // ---------------------------------------------------------------- 生命周期

    @Override
    public boolean onKeyDown(int keyCode, KeyEvent event) {
        if (keyCode == KeyEvent.KEYCODE_BACK && webView != null && webView.canGoBack()) {
            webView.goBack();
            return true;
        }
        return super.onKeyDown(keyCode, event);
    }

    @Override
    protected void onDestroy() {
        if (webView != null) {
            webView.setWebViewClient(new WebViewClient());
            webView.destroy();
            webView = null;
        }
        super.onDestroy();
    }
}
