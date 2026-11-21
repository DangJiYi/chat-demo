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

import com.chaquo.python.PyObject;
import com.chaquo.python.Python;
import com.chaquo.python.android.AndroidPlatform;

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
 * 宿主壳：把 Python 的本地服务拉起来，再用 WebView 加载 127.0.0.1:8790。
 *
 * Python 侧只做两件事需要宿主配合：
 *   1. --data / --www 指向应用私有目录（APK 内部是只读的）；
 *   2. 带 --no-window，别去调 explorer.exe 开浏览器。
 */
public class MainActivity extends Activity {

    private static final String TAG = "dsh-chat";
    // 必须用主机名 localhost，不能用 IP：
    // Android 的 network-security-config 里 <domain> 只接受主机名，
    // 写 127.0.0.1 会被静默忽略 → 明文被拦 → WebView 白屏。
    // 代价是 localhost 可能解析到 ::1，所以 Python 侧同时监听 IPv4/IPv6。
    private static final String HOST = "localhost";
    private static final int PORT = 8790;
    private static final long BOOT_TIMEOUT_MS = 30_000L;

    private WebView webView;
    private TextView errorView;
    // 盖在 WebView 上面的诊断遮罩；成功加载后必须整个隐藏，否则会吃掉所有触摸
    private ScrollView overlay;
    private final StringBuilder diag = new StringBuilder();
    // Python 服务启动时的异常/日志，失败时展示到界面上
    private volatile String pyError = null;
    // Python 解释器和 server 线程是进程级的；Activity 重建时不能重复启动
    private static volatile boolean sBooted = false;

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
            setStatus("正在解压前端资源…");
            extractAssets();
            if (sBooted) {
                // 服务已经在跑（Activity 重建），直接等它就绪即可
                setStatus("正在等待本地服务…");
                waitForServer(BOOT_TIMEOUT_MS);
            } else {
                setStatus("正在启动 Python 服务…");
                startPython();
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
        sb.append("静态：").append(wwwDir().getAbsolutePath()).append("\n");
        if (pyError != null) {
            sb.append("\n--- Python 错误 ---\n").append(pyError);
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

    // ---------------------------------------------------------------- 静态资源

    /**
     * 当前版本的解压目录：www-&lt;stamp&gt;。
     *
     * 不要用固定的 "www"：deleteTree 删不掉某些文件时会静默残留，
     * 每装一次新版就多积一份 3.4 MB，攒几次就变成 17 MB——这正是之前
     * “界面文件 17.8 MB” 的来源。用版本化目录后，新目录总是干净的，
     * 旧目录删不掉也影响不到运行。
     */
    private File wwwDir() {
        return new File(getFilesDir(), "www-" + appStamp());
    }

    private File dataDir() {
        return new File(getFilesDir(), "data");
    }

    /**
     * 把 www/ 从 assets 解压到私有目录。目录名带版本戳，
     * 所以升级后一定是全新目录，并顺手清掉旧版本残留。
     */
    private void extractAssets() throws IOException {
        File target = wwwDir();
        // 目录已完整（上次解压成功）就直接用
        if (new File(target, "index.html").isFile()) {
            pruneOldWww(target);
            ensureDataDir();
            return;
        }
        deleteTree(target);
        if (!target.isDirectory() && !target.mkdirs()) {
            throw new IOException("无法创建目录 " + target);
        }
        copyAssetDir("", target);
        // 解压完整性自检：缺 index.html 说明中途失败，不能当成功
        if (!new File(target, "index.html").isFile()) {
            throw new IOException("解压未完成：" + target + "/index.html 不存在");
        }
        pruneOldWww(target);
        ensureDataDir();
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
    /** 缓存一份：wwwDir() 每次调用都会走这里，不必反复问 PackageManager。 */
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
     * 递归复制 assets。AssetManager 对「空目录」和「文件」都返回空数组，本项目没有空目录，
     * 所以用「list 非空即目录」判断足够。
     *
     * 注意：Chaquopy 会把自己的运行时放在 assets/chaquopy/（app.zip + stdlib），
     * 这些不能当成前端资源抄进 www/，否则白拷几十 MB。
     */
    private void copyAssetDir(String path, File outDir) throws IOException {
        AssetManager am = getAssets();
        String[] children = am.list(path);
        if (children == null) {
            return;
        }
        for (String name : children) {
            if (path.isEmpty() && "chaquopy".equals(name)) {
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

    // ---------------------------------------------------------------- Python

    private void startPython() {
        if (!Python.isStarted()) {
            Python.start(new AndroidPlatform(getApplicationContext()));
        }
        Python py = Python.getInstance();
        PyObject server = py.getModule("server");

        // 不要往 sys.argv 里塞 String[]：Chaquopy 把它转成 jarray，
        // 不是真正的 list，argparse 内部调 list.remove() 会抛 AttributeError。
        // server.run_android() 收普通字符串参数，在 Python 侧拼 argv。
        final String dataPath = dataDir().getAbsolutePath();
        final String wwwPath = wwwDir().getAbsolutePath();

        Thread t = new Thread(() -> {
            try {
                // serve_forever 会一直阻塞，所以必须在后台线程里跑
                server.callAttr("run_android", dataPath, wwwPath, PORT);
            } catch (Throwable e) {
                pyError = Log.getStackTraceString(e);
                Log.e(TAG, "server 退出", e);
            }
        }, "dsh-server");
        t.setDaemon(true);
        t.start();
    }

    private void waitForServer(long timeoutMs) throws IOException {
        long deadline = System.currentTimeMillis() + timeoutMs;
        IOException last = null;
        String[] probes = {"127.0.0.1", "localhost"};
        while (System.currentTimeMillis() < deadline) {
            // Python 侧可能崩了，不用白等到超时
            if (pyError != null) {
                throw new IOException("Python 服务已退出");
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
