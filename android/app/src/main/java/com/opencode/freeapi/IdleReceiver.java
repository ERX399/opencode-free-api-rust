package com.opencode.freeapi;

import android.app.AlarmManager;
import android.app.PendingIntent;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.os.Build;
import android.os.SystemClock;
import android.app.ActivityManager;
import java.util.List;

/**
 * AlarmManager 心跳兜底：即使 ProxyService 被系统整个杀掉（FGS+Watchdog 全失效），
 * 该 receiver 仍会按 60s 周期被拉起，检测到服务未运行就重新 startForegroundService。
 * 注册在 MainActivity onCreate 里（scheduleResurrection），自续链。
 */
public class IdleReceiver extends BroadcastReceiver {
    private static final String TAG = "OCProxy";
    public static final String ACTION_RESURRECT = "com.opencode.freeapi.RESURRECT";

    @Override
    public void onReceive(Context context, Intent intent) {
        if (!ACTION_RESURRECT.equals(intent == null ? null : intent.getAction())) return;
        // 用户主动「强制关闭」过：不再自动复活，直到他重新打开 APP
        if (ProxyService.isUserStopped(context)) return;
        if (!isServiceRunning(context)) {
            Intent service = new Intent(context, ProxyService.class);
            if (Build.VERSION.SDK_INT >= 26) {
                context.startForegroundService(service);
            } else {
                context.startService(service);
            }
        }
        schedule(context);
    }

    private static boolean isServiceRunning(Context context) {
        ActivityManager am = (ActivityManager) context.getSystemService(Context.ACTIVITY_SERVICE);
        if (am == null) return false;
        List<ActivityManager.RunningServiceInfo> list = am.getRunningServices(200);
        if (list != null) {
            for (ActivityManager.RunningServiceInfo info : list) {
                if (ProxyService.class.getName().equals(info.service.getClassName())) {
                    return true;
                }
            }
        }
        return false;
    }

    /** 由 MainActivity / IdleReceiver 自身调用，续订 60s 后的下一次心跳 */
    public static void schedule(Context context) {
        try {
            AlarmManager am = (AlarmManager) context.getSystemService(Context.ALARM_SERVICE);
            Intent intent = new Intent(context, IdleReceiver.class).setAction(ACTION_RESURRECT);
            PendingIntent pi = PendingIntent.getBroadcast(context, 888, intent,
                    PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
            am.setAndAllowWhileIdle(AlarmManager.ELAPSED_REALTIME_WAKEUP,
                    SystemClock.elapsedRealtime() + 60 * 1000L, pi);
        } catch (Exception ignored) {
        }
    }
}