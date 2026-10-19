package com.opencode.freeapi;

import android.content.Intent;
import android.os.Build;
import android.service.quicksettings.Tile;
import android.service.quicksettings.TileService;

// 下拉快捷设置磁贴：点击开关代理，磁贴状态跟随 native 进程存活
public class ProxyTileService extends TileService {

    @Override
    public void onStartListening() {
        // 下拉通知栏时刷新状态
        updateTile();
    }

    @Override
    public void onTileAdded() {
        updateTile();
    }

    @Override
    public void onClick() {
        boolean running = isProxyRunning();
        Intent intent = new Intent(this, ProxyService.class);
        if (running) {
            intent.setAction(ProxyService.ACTION_STOP);
            startServiceCompat(intent);
            setTile(false);
        } else {
            // 用户点击磁贴 = 交互触发的 FGS 启动，不受后台限制
            startServiceCompat(intent);
            // 启动是异步的，先置「启动中」再延迟复核真实状态，
            // 避免 startForegroundService 失败时磁贴错误显示为已运行
            // （STATE_UNAVAILABLE 会让磁贴暂时不可点，改用 INACTIVE 更安全）
            setTile(false);
        }
        // 无论哪个方向，1.2s 后按真实状态校正一次
        new android.os.Handler(getMainLooper()).postDelayed(this::updateTile, 1200);
    }

    private void startServiceCompat(Intent intent) {
        if (Build.VERSION.SDK_INT >= 26) {
            startForegroundService(intent);
        } else {
            startService(intent);
        }
    }

    private boolean isProxyRunning() {
        // Android 没有 pgrep（非 toybox 标准命令），原先的 Runtime.exec("pgrep"...)
        // 必然失败并永远返回 false，导致磁贴状态恒为「未运行」。
        // 改用服务自己维护的存活标记 + 一次真实健康探测双重确认。
        if (!ProxyService.isAlive()) return false;
        java.net.HttpURLConnection conn = null;
        try {
            conn = (java.net.HttpURLConnection)
                    new java.net.URL("http://127.0.0.1:8788/health").openConnection();
            conn.setConnectTimeout(800);
            conn.setReadTimeout(800);
            conn.setUseCaches(false);
            return conn.getResponseCode() == 200;
        } catch (Exception e) {
            return false;
        } finally {
            if (conn != null) conn.disconnect();
        }
    }

    private void setTile(boolean active) {
        Tile tile = getQsTile();
        if (tile != null) {
            tile.setState(active ? Tile.STATE_ACTIVE : Tile.STATE_INACTIVE);
            tile.updateTile();
        }
    }

    private void updateTile() {
        setTile(isProxyRunning());
    }
}