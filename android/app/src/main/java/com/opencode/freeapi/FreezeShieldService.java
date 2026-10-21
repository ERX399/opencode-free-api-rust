package com.opencode.freeapi;

import android.accessibilityservice.AccessibilityService;
import android.view.accessibility.AccessibilityEvent;

/**
 * 冻结防护锚点（可选）。
 * <p>
 * vivo / OPPO / 小米等国产 ROM 会把后台 app 冻入 cgroup freezer，导致：
 * TCP accept 停摆（请求全部挂死）、TileService 不响应（磁贴灰、点击无效）、
 * AlarmManager / Watchdog 全部停摆——即使已在前台服务 + doze 白名单也照冻不误。
 * <p>
 * 实测这些 ROM 的冻结策略会跳过「已启用无障碍服务的 app」（GKD、Key Mapper
 * 等保活框架即依赖此机制常驻）。本服务只收窗口状态变化事件（维持活跃的最小
 * 事件集）、不读取任何窗口内容，仅作为无障碍锚点存在；用户需在 系统设置 →
 * 无障碍 → 已下载的服务 中手动启用。
 * <p>
 * 服务启用/关闭时顺带触发一次 ENSURE，确保 native 代理随之恢复/确认健康。
 */
public class FreezeShieldService extends AccessibilityService {

    @Override
    protected void onServiceConnected() {
        super.onServiceConnected();
        // 无障碍锚点生效：立即确认一次服务健康（用户交互上下文，允许 FGS 启动）
        try {
            android.content.Intent intent = new android.content.Intent(this, ProxyService.class)
                    .setAction(ProxyService.ACTION_ENSURE);
            startService(intent);
        } catch (Exception ignored) {
        }
    }

    @Override
    public void onAccessibilityEvent(AccessibilityEvent event) {
        // 只收窗口状态变化事件（保持服务活跃的最小事件集），不读取任何内容
    }

    @Override
    public void onInterrupt() {
        // 无操作
    }
}