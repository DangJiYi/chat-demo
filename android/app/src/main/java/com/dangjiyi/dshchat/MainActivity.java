package com.dangjiyi.dshchat;

import android.app.Activity;
import android.content.pm.PackageInfo;
import android.content.res.AssetManager;
import android.graphics.Color;
import android.os.Bundle;
import android.util.Log;
import android.view.KeyEvent;
import android.view.View;
import android.view.ViewGroup;
import android.webkit.WebSettings;
import android.webkit.WebView;
import android.webkit.WebViewClient;
import android.widget.FrameLayout;
import android.widget.TextView;

import com.chaquopy.python.PyObject;
import com.chaquopy.python.Python;
import com.chaquopy.python.android.AndroidPlatform;

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
    private static final String HOST = "127.0.0.1";
    private static final int PORT = 8790;
    private static final long BOOT_TIMEOUT_MS = 60_000L;

    private WebView webView;
    private TextView errorView;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);

        FrameLayout root = new FrameLayout(this);
        webView = new WebView(this);
        errorView = new TextView(this);
        root.addView(webView, new FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT));
        root.addView(errorView, new FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT));
        errorView.setVisibility(View.GONE);
        errorView.setPadding(48, 96, 48, 48);
        errorView.setTextColor(Color.parseColor("#444444"));
        errorView.setTextSize(14f);
        setContentView(root);

        WebSettings settings = webView.getSettings();
        settings.setJavaScriptEnabled(true);
        settings.setDomStorageEnabled(true);
        settings.setDatabaseEnabled(true);
        settings.setAllowFileAccess(false);
        settings.setAllowContentAccess(false);
        settings.setMediaPlaybackRequiresUserGesture(false);
        webView.setWebViewClient(new WebViewClient());
        webView.setBackgroundColor(Color.WHITE);

        new Thread(this::boot, "dsh-boot").start();
    }

    private void boot() {
        try {
            extractAssets();
            startPython();
            waitForServer(BOOT_TIMEOUT_MS);
            runOnUiThread(() -> webView.loadUrl("http://" + HOST + ":" + PORT + "/"));
        } catch (Throwable t) {
            Log.e(TAG, "启动失败", t);
            runOnUiThread(() -> showError(t));
        }
    }

    private void showError(Throwable t) {
        webView.setVisibility(View.GONE);
        errorView.setVisibility(View.VISIBLE);
        errorView.setText("启动失败：" + t + "\n\n日志：adb logcat -s dsh-chat");
    }

    // ---------------------------------------------------------------- 静态资源

    private File wwwDir() {
        return new File(getFilesDir(), "www");
    }

    private File dataDir() {
        return new File(getFilesDir(), "data");
    }

    /**
     * 把 www/ 从 assets 解压到私有目录。按 versionCode 打戳，升级后自动重解压。
     */
    private void extractAssets() throws IOException {
        File target = wwwDir();
        File stamp = new File(getFilesDir(), "www.stamp");
        String want = String.valueOf(appVersionCode());
        String have = stamp.isFile() ? readText(stamp) : "";
        if (want.equals(have) && new File(target, "index.html").isFile()) {
            return;
        }
        deleteTree(target);
        if (!target.isDirectory() && !target.mkdirs()) {
            throw new IOException("无法创建目录 " + target);
        }
        copyAssetDir("", target);
        writeText(stamp, want);
        if (!dataDir().isDirectory()) {
            // server.py 自己也会建，这里提前建好省得首屏报错
            //noinspection ResultOfMethodCallIgnored
            dataDir().mkdirs();
        }
    }

    private int appVersionCode() {
        try {
            PackageInfo info = getPackageManager().getPackageInfo(getPackageName(), 0);
            return info.versionCode;
        } catch (Exception e) {
            return 1;
        }
    }

    /**
     * 递归复制 assets。AssetManager 对「空目录」和「文件」都返回空数组，本项目没有空目录，
     * 所以用「list 非空即目录」判断足够。
     */
    private void copyAssetDir(String path, File outDir) throws IOException {
        AssetManager am = getAssets();
        String[] children = am.list(path);
        if (children == null) {
            return;
        }
        for (String name : children) {
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

    private static void deleteTree(File f) {
        if (f == null || !f.exists()) {
            return;
        }
        File[] kids = f.listFiles();
        if (kids != null) {
            for (File kid : kids) {
                deleteTree(kid);
            }
        }
        //noinspection ResultOfMethodCallIgnored
        f.delete();
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

        // server.py 的 main() 读 sys.argv，这里按命令行方式拼好
        PyObject sys = py.getModule("sys");
        PyObject argv = py.getModule("builtins").callAttr("list");
        argv.callAttr("append", "server.py");
        argv.callAttr("append", "--no-window");
        argv.callAttr("append", "--data");
        argv.callAttr("append", dataDir().getAbsolutePath());
        argv.callAttr("append", "--www");
        argv.callAttr("append", wwwDir().getAbsolutePath());
        argv.callAttr("append", "--port");
        argv.callAttr("append", String.valueOf(PORT));
        sys.put("argv", argv);

        PyObject server = py.getModule("server");
        Thread t = new Thread(() -> {
            try {
                // serve_forever 会一直阻塞，所以必须在后台线程里跑
                server.callAttr("main");
            } catch (Throwable e) {
                Log.e(TAG, "server.main 退出", e);
            }
        }, "dsh-server");
        t.setDaemon(true);
        t.start();
    }

    private void waitForServer(long timeoutMs) throws IOException {
        long deadline = System.currentTimeMillis() + timeoutMs;
        IOException last = null;
        while (System.currentTimeMillis() < deadline) {
            HttpURLConnection conn = null;
            try {
                conn = (HttpURLConnection) new URL(
                        "http://" + HOST + ":" + PORT + "/local/status").openConnection();
                conn.setConnectTimeout(1000);
                conn.setReadTimeout(2000);
                if (conn.getResponseCode() == 200) {
                    return;
                }
            } catch (IOException e) {
                last = e;
            } finally {
                if (conn != null) {
                    conn.disconnect();
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
