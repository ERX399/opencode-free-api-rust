package com.opencode.freeapi;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Intent;
import android.content.pm.ApplicationInfo;
import android.content.pm.ServiceInfo;
import android.os.Build;
import android.os.IBinder;
import android.util.Log;

import java.io.File;
import java.io.IOException;

public class ProxyService extends Service {

    private static final String TAG = "OCProxy";
    private static final String CHANNEL_ID = "proxy";
    private static final int NOTIF_ID = 1;
    private static final String ACTION_STOP = "com.opencode.freeapi.STOP";
    private Process process;
    private volatile boolean stopped;

    @Override
    public void onCreate() {
        super.onCreate();
        createChannel();
        startForegroundCompat();
        startProxy();
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        if (intent != null && ACTION_STOP.equals(intent.getAction())) {
            stopProxy();
            stopForeground(STOP_FOREGROUND_REMOVE);
            stopSelf();
            return START_NOT_STICKY;
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
        } catch (IOException e) {
            Log.e(TAG, "failed to start native process", e);
        }
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
                    CHANNEL_ID, "代理服务", NotificationManager.IMPORTANCE_HIGH);
            channel.setDescription("OC Free API 代理常驻通知");
            channel.enableVibration(false);
            channel.setSound(null, null);
            NotificationManager manager = getSystemService(NotificationManager.class);
            manager.createNotificationChannel(channel);
        }
    }

    private void startForegroundCompat() {
        Intent contentIntent = new Intent(this, MainActivity.class);
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
                .setContentText("端口 127.0.0.1:8788，点击返回控制台")
                .setContentIntent(pending)
                .setOngoing(true)
                .setPriority(Notification.PRIORITY_HIGH)
                .setCategory(Notification.CATEGORY_SERVICE)
                .addAction(android.R.drawable.ic_menu_close_clear_cancel, "强制关闭", stopPending)
                .build();
        if (Build.VERSION.SDK_INT >= 29) {
            startForeground(NOTIF_ID, notification,
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC);
        } else {
            startForeground(NOTIF_ID, notification);
        }
    }

    private void stopProxy() {
        stopped = true;
        if (process != null) {
            process.destroy();
            process = null;
        }
    }
}
