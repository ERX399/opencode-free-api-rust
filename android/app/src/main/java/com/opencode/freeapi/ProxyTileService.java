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
            setTile(true);
        }
    }

    private void startServiceCompat(Intent intent) {
        if (Build.VERSION.SDK_INT >= 26) {
            startForegroundService(intent);
        } else {
            startService(intent);
        }
    }

    private boolean isProxyRunning() {
        try {
            java.lang.Process p = Runtime.getRuntime().exec(
                    new String[]{"pgrep", "-f", "libopencode_free_api.so"});
            return p.waitFor() == 0;
        } catch (Exception e) {
            return false;
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