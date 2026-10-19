package com.opencode.freeapi;
import android.app.AlarmManager;
import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Intent;
import android.content.pm.ApplicationInfo;
import android.content.pm.ServiceInfo;
import android.net.ConnectivityManager;
import android.net.Network;
import android.net.NetworkCapabilities;
import android.net.NetworkInfo;
import android.os.Build;
import android.os.IBinder;
import android.os.PowerManager;
import android.os.SystemClock;
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
    private volatile int healthFailures;
    private android.net.ConnectivityManager.NetworkCallback networkCallback;
    // 服务存活标记：IdleReceiver 无法跨进程查询服务状态（Android 8+ 限制），
    // 由服务自己在 onCreate/onDestroy 维护
    private static volatile boolean alive;
    public static boolean isAlive() {
        return alive;
    }
    @Override
    public void onCreate() {
        super.onCreate();
        // 服务被系统拉起 = 用户希望它跑（或复活链生效），清掉上次「强制关闭」的标记
        setUserStopped(false);
        alive = true;
        createChannel();
        startForegroundCompat();
        startProxy();
        startKeepAlive();
        startHealthWatchdog();
        startNetworkCallback();
        scheduleExactTick();
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
            // 用户显式关闭：置持久标志，让 IdleReceiver 的复活链不再把他拉起来
            stopProxy();
            setUserStopped(true);
            cancelExactTick();
            stopForeground(STOP_FOREGROUND_REMOVE);
            stopSelf();
            return START_NOT_STICKY;
        }
        if (ACTION_RESTART.equals(action)) {
            // 通知栏「重启」按钮：用户交互触发的 FGS 重入，系统视为前台活跃
            setUserStopped(false);
            restartNative();
            scheduleExactTick();
            return START_STICKY;
        }
        if (ACTION_PING.equals(action)) {
            // 通知栏点击/快捷磁贴触发的轻量保活：仅刷新通知时间戳，
            // 让系统把服务标记为“用户最近交互”，降低被冻结的优先级权重
            refreshNotification();
            return START_STICKY;
        }
        if (ACTION_TICK.equals(action)) {
            // AlarmManager 兜底 tick：刷新通知（活跃时间戳）+ 健康探测（真实流量），
            // 并续订下一次 alarm，形成 Doze 下也不断链的自续心跳
            refreshNotification();
            if (process != null && process.isAlive() && healthCheck()) {
                // 正常：无事
            } else if (process != null && !process.isAlive()) {
                startProxy();
            } else if (!isNetworkAvailable()) {
                // 无网阶段不算失败，避免误杀
            } else {
                healthFailures++;
                if (healthFailures >= 2) {
                    restartNative();
                    healthFailures = 0;
                }
            }
            scheduleExactTick();
            return START_STICKY;
        }
        return START_STICKY;
    }

    // 用户显式「强制关闭」的持久标记：跨进程重启保留，
// 避免 IdleReceiver 的 60s 复活链把用户主动关掉的服务又拉起来
    private void setUserStopped(boolean value) {
        try {
            getSharedPreferences("ocfa", MODE_PRIVATE).edit()
                    .putBoolean("user_stopped", value).apply();
        } catch (Exception ignored) {
        }
    }

    public static boolean isUserStopped(android.content.Context ctx) {
        try {
            return ctx.getSharedPreferences("ocfa", MODE_PRIVATE)
                    .getBoolean("user_stopped", false);
        } catch (Exception e) {
            return false;
        }
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }

    @Override
    public void onDestroy() {
        super.onDestroy();
        stopped = true;
        alive = false;
        try {
            if (networkCallback != null) {
                ((ConnectivityManager) getSystemService(CONNECTIVITY_SERVICE))
                        .unregisterNetworkCallback(networkCallback);
            }
        } catch (Exception ignored) {
        }
        releaseWakeLock();
        stopProxy();
        // 注意：不在此处 cancelExactTick —— 服务被系统回收时 alarm 是复活链的一环。
        // 只有用户显式 ACTION_STOP 才取消（见 onStartCommand）。
    }

    private void startProxy() {
        synchronized (this) {
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
                // 端口固定传给子进程，避免继承到外部环境里被改过的 PORT
                // 导致 APP 里硬编码的 8788 永远连不上
                builder.environment().put("PORT", "8788");
                process = builder.start();
                Log.i(TAG, "native process started");
                drainProcessOutput(process);
                watchProcess();
            } catch (IOException e) {
                Log.e(TAG, "failed to start native process", e);
            }
        }
    }

    // 必须持续读走子进程的 stdout/stderr。
    // redirectErrorStream(true) 只合并了两个流，管道缓冲区（约 64KB）满了以后
    // 子进程的 write() 会永久阻塞——Rust 端每个请求都 eprintln 日志，
    // 跑一段时间就会卡死在日志写入上，表现为代理无响应 / 连接被中止（os error 103）。
    private void drainProcessOutput(final Process p) {
        Thread thread = new Thread(() -> {
            byte[] buf = new byte[4096];
            try (java.io.InputStream in = p.getInputStream()) {
                int n;
                while ((n = in.read(buf)) != -1) {
                    // 只保留最后一行用于诊断，避免日志刷爆 logcat
                    String line = new String(buf, 0, n, "UTF-8").trim();
                    if (!line.isEmpty()) Log.d(TAG, "native: " + line);
                }
            } catch (Exception ignored) {
            }
        }, "proxy-stdout-drain");
        thread.setDaemon(true);
        thread.start();
    }

    // GKD 式保活核心：周期性重发前台通知 + 触发一次本机健康请求。
// 通知刷新更新系统“最近活跃”时间戳；健康请求产生真实流量，
// 对只看网络活动的 ROM（MIUI/OriginOS 等）同样保持活跃判定。
    private void startKeepAlive() {
        Thread thread = new Thread(() -> {
            while (!stopped) {
                try {
                    Thread.sleep(2 * 60 * 1000L); // 2 分钟
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

    // Doze 深睡下线程 sleep 会被系统合并延迟，用 setExactAndAllowWhileIdle
    // 定时唤醒做兜底 tick：刷新通知 + 健康探测。自身触发自身，形成自续 alarm 链。
    static final String ACTION_TICK = "com.opencode.freeapi.ALARM_TICK";
    private void scheduleExactTick() {
        try {
            AlarmManager am = (AlarmManager) getSystemService(ALARM_SERVICE);
            Intent intent = new Intent(this, ProxyService.class).setAction(ACTION_TICK);
            PendingIntent pi = PendingIntent.getService(this, 777,
                    intent, PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
            am.setExactAndAllowWhileIdle(AlarmManager.ELAPSED_REALTIME_WAKEUP,
                    SystemClock.elapsedRealtime() + 60 * 1000L, pi);
        } catch (Exception e) {
            Log.w(TAG, "alarm tick schedule failed", e);
        }
    }

    // 取消自续 alarm 链。否则 PendingIntent.getService 到点会重建已 stopSelf 的服务，
// 把用户主动关闭的代理在 60 秒后又拉起来。
    private void cancelExactTick() {
        try {
            AlarmManager am = (AlarmManager) getSystemService(ALARM_SERVICE);
            Intent intent = new Intent(this, ProxyService.class).setAction(ACTION_TICK);
            PendingIntent pi = PendingIntent.getService(this, 777,
                    intent, PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
            am.cancel(pi);
            pi.cancel();
        } catch (Exception e) {
            Log.w(TAG, "alarm tick cancel failed", e);
        }
    }

    // 健康探测守护：后台网络恢复后若 native 假死（worker 卡在阻塞 DNS/连接），
    // 自动 kill 并重启 native 进程。这是 GKD 式“上层守护下层”的核心。
    private void startHealthWatchdog() {
        Thread thread = new Thread(() -> {
            while (!stopped) {
                try {
                    Thread.sleep(60 * 1000L); // 1 分钟一测
                    if (stopped) return;
                    if (!isNetworkAvailable()) { healthFailures = 0; continue; }
                    if (healthCheck()) {
                        healthFailures = 0;
                    } else {
                        healthFailures++;
                        Log.w(TAG, "health check failed x" + healthFailures);
                        if (healthFailures >= 3) { // 连续 3 分钟无响应
                            Log.w(TAG, "native hung, restarting...");
                            restartNative();
                            healthFailures = 0;
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

    // 网络可用性：优先 NetworkCapabilities（API 23+），回退 NetworkInfo
    private boolean isNetworkAvailable() {
        try {
            ConnectivityManager cm = (ConnectivityManager) getSystemService(CONNECTIVITY_SERVICE);
            if (Build.VERSION.SDK_INT >= 23) {
                Network net = cm.getActiveNetwork();
                if (net == null) return false;
                NetworkCapabilities caps = cm.getNetworkCapabilities(net);
                return caps != null && caps.hasCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET);
            }
            NetworkInfo info = cm.getActiveNetworkInfo();
            return info != null && info.isConnected();
        } catch (Exception e) {
            return true; // 无法判断时假定有网，交给健康检查决定
        }
    }

    // 网络状态回调：断网恢复 / 网络切换时立即探测并按需重启 native，
    // 避免依赖轮询才能发现“切网后连接全部失效”（ECONNABORTED 高发场景）
    private void startNetworkCallback() {
        try {
            final ConnectivityManager cm = (ConnectivityManager) getSystemService(CONNECTIVITY_SERVICE);
            networkCallback = new ConnectivityManager.NetworkCallback() {
                @Override
                public void onAvailable(Network network) {
                    // 切网恢复：延迟 1.5s 让路由就绪，然后立刻做一次真实健康探测
                    new Thread(() -> {
                        try { Thread.sleep(1500); } catch (InterruptedException ignored) { return; }
                        if (stopped) return;
                        if (process == null || !process.isAlive()) {
                            startProxy();
                        } else if (!healthCheck()) {
                            restartNative();
                        }
                        refreshNotification();
                    }, "net-recover").start();
                }
            };
            cm.registerDefaultNetworkCallback(networkCallback);
        } catch (Exception e) {
            Log.w(TAG, "network callback failed", e);
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

    // 只重启 native 子进程，不动服务生命周期标志 stopped。
    // 原实现调用 stopProxy() 会把 stopped 置 true，导致健康看门狗/保活/网络回调
    // 等所有守护线程在下一次循环时永久退出——重启一次就失去全部保活能力。
    private void restartNative() {
        synchronized (this) {
            if (process != null) {
                try {
                    process.destroy();
                } catch (Exception ignored) {
                }
                process = null;
            }
        }
        healthFailures = 0;
        startProxy();
        refreshNotification();
    }

    // 看门狗：子进程意外退出时自动拉起
    // 进程看门狗：只在 onCreate 启动一次，循环检查当前 process 是否存活。
    // 原实现每次 startProxy 都新建一个 watchdog 线程（并各自 waitFor 旧进程引用），
    // 重启次数越多线程越多；这里改为单一常驻线程，按引用判活。
    private volatile boolean watchdogStarted;
    private void watchProcess() {
        if (watchdogStarted) return;
        watchdogStarted = true;
        Thread thread = new Thread(() -> {
            while (!stopped) {
                try {
                    Process p = process;
                    if (p != null) {
                        p.waitFor();
                    } else {
                        Thread.sleep(1000);
                        continue;
                    }
                    if (stopped) return;
                    Log.w(TAG, "native process exited, restarting...");
                    // 避免与健康看门狗/网络回调同时重启造成双进程
                    synchronized (this) {
                        if (process == p) {
                            startProxy();
                        }
                    }
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
