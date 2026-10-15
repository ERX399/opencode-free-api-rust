package com.opencode.freeapi;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Intent;
import android.content.pm.ApplicationInfo;
import android.content.pm.ServiceInfo;
import android.net.ConnectivityManager;
import android.net.NetworkInfo;
import android.os.Build;
import android.os.IBinder;
import android.os.PowerManager;
import android.util.Log;
import java.io.File;
import java.io.IOException;
import java.net.HttpURLConnection;
import java.net.URL;

public class ProxyService extends Service {
    private static final String TAG = "OCProxy";
    private static final String CHANNEL_ID = "proxy";
    private static final int NOTIF_ID = 1;
    public static final String ACTION_STOP = "com.opencode.freeapi.STOP";
    private static final String ACTION_RESTART = "com.opencode.freeapi.RESTART";
    private static final String ACTION_PING = "com.opencode.freeapi.PING";
    private Process process;
    private volatile boolean stopped;
    @Override
    public void onCreate() {
        super.onCreate();
        createChannel();
        startForegroundCompat();
        startProxy();
        startKeepAlive();
        startHealthWatchdog();
        acquireWakeLock();
    }

    // PARTIAL_WAKE_LOCK：保持 CPU 运行，防后台深度休眠导致 native 线程停摆。
    // 前台服务持锁系统不回收，配合电池豁免使用
    private PowerManager.WakeLock wakeLock;
    private void acquireWakeLock() {
        try {
            PowerManager pm = (PowerManager) getSystemService(POWER_SERVICE);
            wakeLock = pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "ocfa:proxy");
            wakeLock.setReferenceCounted(false);
            wakeLock.acquire();
        } catch (Exception e) {
            Log.w(TAG, "wake lock failed", e);
        }
    }

    private void releaseWakeLock() {
        try {
            if (wakeLock != null && wakeLock.isHeld()) wakeLock.release();
        } catch (Exception ignored) {
        }
    }
    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        String action = intent == null ? null : intent.getAction();
        if (ACTION_STOP.equals(action)) {
            stopProxy();
            stopForeground(STOP_FOREGROUND_REMOVE);
            stopSelf();
            return START_NOT_STICKY;
        }
        if (ACTION_RESTART.equals(action)) {
            // 通知栏「重启」按钮：用户交互触发的 FGS 重入，系统视为前台活跃
            stopProxy();
            startProxy();
            refreshNotification();
            return START_STICKY;
        }
        if (ACTION_PING.equals(action)) {
            // 通知栏点击/快捷磁贴触发的轻量保活：仅刷新通知时间戳，
            // 让系统把服务标记为“用户最近交互”，降低被冻结的优先级权重
            refreshNotification();
            return START_STICKY;
        }
        return START_STICKY;
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }

    @Override
    public void onDestroy() {
        super.onDestroy();
        stopped = true;
        releaseWakeLock();
        stopProxy();
    }

    private void startProxy() {
        if (process != null && process.isAlive()) {
            return;
        }
        try {
            ApplicationInfo info = getApplicationInfo();
            File lib = new File(info.nativeLibraryDir, "libopencode_free_api.so");
            if (!lib.exists() || lib.length() == 0) {
                Log.e(TAG, "native library missing: " + lib.getAbsolutePath());
                return;
            }
            ProcessBuilder builder = new ProcessBuilder(lib.getAbsolutePath());
            builder.directory(getFilesDir());
            builder.redirectErrorStream(true);
            process = builder.start();
            Log.i(TAG, "native process started");
            watchProcess();
            startKeepAlive();
        } catch (IOException e) {
            Log.e(TAG, "failed to start native process", e);
        }
    }

    // GKD 式保活核心：周期性重发前台通知，更新 when 时间戳。
    // 系统冻结判定参考“服务最近活跃时间”，每次 notify 都会重置该计时。
    private void startKeepAlive() {
        Thread thread = new Thread(() -> {
            while (!stopped) {
                try {
                    Thread.sleep(10 * 60 * 1000L); // 10 分钟
                    if (stopped) return;
                    refreshNotification();
                } catch (InterruptedException e) {
                    return;
                }
            }
        }, "proxy-keepalive");
        thread.setDaemon(true);
        thread.start();
    }

    // 健康探测守护：后台网络恢复后若 native 假死（worker 卡在阻塞 DNS/连接），
    // 自动 kill 并重启 native 进程。这是 GKD 式“上层守护下层”的核心。
    private void startHealthWatchdog() {
        Thread thread = new Thread(() -> {
            int failures = 0;
            while (!stopped) {
                try {
                    Thread.sleep(60 * 1000L); // 1 分钟一测
                    if (stopped) return;
                    if (!isNetworkAvailable()) { failures = 0; continue; }
                    if (healthCheck()) {
                        failures = 0;
                    } else {
                        failures++;
                        Log.w(TAG, "health check failed x" + failures);
                        if (failures >= 3) { // 连续 3 分钟无响应
                            Log.w(TAG, "native hung, restarting...");
                            restartNative();
                            failures = 0;
                        }
                    }
                } catch (InterruptedException e) {
                    return;
                }
            }
        }, "proxy-health-watchdog");
        thread.setDaemon(true);
        thread.start();
    }

    private boolean isNetworkAvailable() {
        try {
            ConnectivityManager cm = (ConnectivityManager) getSystemService(CONNECTIVITY_SERVICE);
            NetworkInfo info = cm.getActiveNetworkInfo();
            return info != null && info.isConnected();
        } catch (Exception e) {
            return true; // 无法判断时假定有网，交给健康检查决定
        }
    }

    // 本机 /health 探测，2 秒超时
    private boolean healthCheck() {
        try {
            System.setProperty("http.keepAlive", "false");
            HttpURLConnection conn = (HttpURLConnection)
                    new URL("http://127.0.0.1:8788/health").openConnection();
            conn.setUseCaches(false);
            conn.setRequestProperty("Connection", "close");
            conn.setConnectTimeout(2000);
            conn.setReadTimeout(2000);
            int code = conn.getResponseCode();
            conn.disconnect();
            return code == 200;
        } catch (Exception e) {
            return false;
        }
    }

    private void restartNative() {
        stopProxy();
        stopped = false; // stopProxy 置位后 watchdog 线程会退出，这里复位让服务继续
        startProxy();
    }

    // 看门狗：子进程意外退出时自动拉起
    private void watchProcess() {
        Thread thread = new Thread(() -> {
            while (!stopped) {
                try {
                    process.waitFor();
                    if (stopped) return;
                    Log.w(TAG, "native process exited, restarting...");
                    startProxy();
                } catch (InterruptedException e) {
                    return;
                }
            }
        }, "proxy-watchdog");
        thread.setDaemon(true);
        thread.start();
    }

    private void createChannel() {
        if (Build.VERSION.SDK_INT >= 26) {
            NotificationChannel channel = new NotificationChannel(
                    // LOW：通知常驻但不响铃不横幅（GKD 式静默保活），每次静默刷新不打扰用户
                    CHANNEL_ID, "代理服务", NotificationManager.IMPORTANCE_LOW);
            channel.setDescription("OC Free API 代理常驻通知");
            channel.enableVibration(false);
            channel.setSound(null, null);
            NotificationManager manager = getSystemService(NotificationManager.class);
            manager.createNotificationChannel(channel);
        }
    }

    private void startForegroundCompat() {
        Intent contentIntent = new Intent(this, MainActivity.class)
                .setAction(ACTION_PING)
                .addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP);
        PendingIntent pending = PendingIntent.getActivity(
                this, 0, contentIntent, PendingIntent.FLAG_IMMUTABLE);
        Intent stopIntent = new Intent(this, ProxyService.class).setAction(ACTION_STOP);
        PendingIntent stopPending = PendingIntent.getService(
                this, 1, stopIntent, PendingIntent.FLAG_IMMUTABLE);
        Notification.Builder builder;
        if (Build.VERSION.SDK_INT >= 26) {
            builder = new Notification.Builder(this, CHANNEL_ID);
        } else {
            builder = new Notification.Builder(this);
        }
        Notification notification = builder
                .setSmallIcon(android.R.drawable.stat_notify_sync)
                .setContentTitle("OC Free API 运行中")
                .setContentText("端口 127.0.0.1:8788 · 点击刷新保活 · 下拉快捷操作")
                .setContentIntent(pending)
                .setOngoing(true)
                .setOnlyAlertOnce(true)
                .setShowWhen(true)
                .setWhen(System.currentTimeMillis())
                .setPriority(Notification.PRIORITY_LOW)
                .setCategory(Notification.CATEGORY_SERVICE)
                // 通知快捷操作：重启 = 用户交互重入 FGS（保活 + 修复异常进程）
                .addAction(android.R.drawable.ic_popup_sync, "重启",
                        buildAction(ACTION_RESTART))
                .addAction(android.R.drawable.ic_menu_close_clear_cancel, "强制关闭", stopPending)
                .build();
        if (Build.VERSION.SDK_INT >= 29) {
            startForeground(NOTIF_ID, notification,
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC);
        } else {
            startForeground(NOTIF_ID, notification);
        }
    }

    private PendingIntent buildAction(String action) {
        Intent intent = new Intent(this, ProxyService.class).setAction(action);
        return PendingIntent.getService(this, action.hashCode(),
                intent, PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
    }

    // 重新提交同一通知：更新“最近活跃”时间戳。用户点通知或触发快捷操作时调用，
    // 系统会将服务视为用户正在交互，显著降低 Doze/冻结的权重
    private void refreshNotification() {
        if (stopped) return;
        startForegroundCompat();
    }

    @Override
    public void onTaskRemoved(Intent rootIntent) {
        // 用户从最近任务划掉 App：前台服务仍应继续运行（通知保留），
        // 但某些 ROM 会连带杀进程 —— 由 START_STICKY + watchdog 兜底重启
        super.onTaskRemoved(rootIntent);
    }

    private void stopProxy() {
        stopped = true;
        if (process != null) {
            process.destroy();
            process = null;
        }
    }
}
