package com.opencode.freeapi;

import android.Manifest;
import android.annotation.SuppressLint;
import android.app.Activity;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.PowerManager;
import android.provider.Settings;
import android.webkit.WebSettings;
import android.webkit.WebView;
import android.webkit.WebViewClient;
import android.widget.Toast;

public class MainActivity extends Activity {

    private WebView webView;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        requestNotificationPermission();
        requestBatteryOptimizationExemption();
        startProxyService();
        setupWebView();
    }

    private void requestNotificationPermission() {
        if (Build.VERSION.SDK_INT >= 33
                && checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS)
                != PackageManager.PERMISSION_GRANTED) {
            requestPermissions(new String[]{Manifest.permission.POST_NOTIFICATIONS}, 100);
        }
    }

    // 请求电池优化豁免，让前台服务在后台不被冻结
    private void requestBatteryOptimizationExemption() {
        if (Build.VERSION.SDK_INT < 23) return;
        PowerManager pm = (PowerManager) getSystemService(Context.POWER_SERVICE);
        if (pm.isIgnoringBatteryOptimizations(getPackageName())) return;
        try {
            Intent intent = new Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
                    Uri.parse("package:" + getPackageName()));
            startActivity(intent);
        } catch (Exception ignored) {
        }
    }

    private void startProxyService() {
        Intent intent = new Intent(this, ProxyService.class);
        if (Build.VERSION.SDK_INT >= 26) {
            startForegroundService(intent);
        } else {
            startService(intent);
        }
    }

    @SuppressLint("SetJavaScriptEnabled")
    private void setupWebView() {
        webView = new WebView(this);
        // 深色背景消除加载前的白屏闪烁，夜间打开不刺眼
        webView.setBackgroundColor(0xFF14110D);
        WebSettings settings = webView.getSettings();
        settings.setJavaScriptEnabled(true);
        settings.setDomStorageEnabled(true);
        webView.setWebViewClient(new WebViewClient() {
            private int retries = 0;

            @Override
            public void onReceivedError(WebView view, int errorCode,
                                        String description, String failingUrl) {
                showLoadingPage(view);
                if (retries < 30) {
                    retries++;
                    view.postDelayed(() -> {
                        if (!isFinishing()) {
                            view.loadUrl("http://127.0.0.1:8788/");
                        }
                    }, 800);
                }
            }

            private void showLoadingPage(WebView view) {
                String html = "<html><head><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">"
                        + "<style>body{margin:0;height:100vh;display:flex;align-items:center;justify-content:center;"
                        + "background:#14110d;color:#a39d92;font-family:system-ui,sans-serif}"
                        + "p{text-align:center}h2{color:#cc785c;margin-bottom:8px}</style></head>"
                        + "<body><div><h2>OC Free API</h2><p>正在启动服务…</p></div></body></html>";
                view.loadDataWithBaseURL("about:blank", html, "text/html", "utf-8", null);
            }
        });
        webView.loadUrl("http://127.0.0.1:8788/");
        setContentView(webView);
    }

    @Override
    public void onBackPressed() {
        if (webView != null && webView.canGoBack()) {
            webView.goBack();
        } else {
            super.onBackPressed();
        }
    }
}
