//! 桌面宠物(Ink pet)悬浮窗的生命周期 helper。
//!
//! 窗口在 `tauri.conf.json` 静态声明(label "pet",`visible:false`),本模块只负责按
//! [`AppSettings::pet`] 开关显隐、恢复上次拖拽位置。宠物的状态(PetState:健康 / 打标中 /
//! 打标失败 …)由前端主窗口聚合后 `emit` `git-ai-studio://pet-state`,pet 窗口纯渲染,
//! 后端不参与状态计算。详见 ADR-011。

use tauri::{AppHandle, Manager, PhysicalPosition};

use crate::state::AppSettings;

/// pet 窗口在 `tauri.conf.json` 里的 label。
const PET_WINDOW_LABEL: &str = "pet";

/// 按 `enabled` 显隐 pet 窗口;show 前先恢复上次记忆的位置。
///
/// 窗口理论上已静态声明,不存在;若取不到只记一行 warn 而非 panic —— 宠物是可选层,
/// 它的缺失绝不能拖垮主程序。
pub fn apply_visibility(app: &AppHandle, enabled: bool, position: Option<(i32, i32)>) {
    let Some(win) = app.get_webview_window(PET_WINDOW_LABEL) else {
        log::warn!("pet 窗口未注册(label={PET_WINDOW_LABEL}),跳过显隐");
        return;
    };
    if enabled {
        // 有记忆位置就用记忆;否则首次落在主屏右下角,避免 tauri 默认居中挡住工作区。
        match position {
            Some((x, y)) => {
                let _ = win.set_position(PhysicalPosition::new(x, y));
            }
            None => {
                if let Some(corner) = default_corner_position(&win) {
                    let _ = win.set_position(corner);
                }
            }
        }
        let _ = win.show();
    } else {
        let _ = win.hide();
    }
}

/// 主屏右下角位置(留 24px 边距),供宠物首次显示落位。取不到屏幕 / 窗口尺寸时返回
/// None,调用方退回 tauri 默认位置(居中),绝不中断显示。
fn default_corner_position(win: &tauri::WebviewWindow) -> Option<PhysicalPosition<i32>> {
    let monitor = win.primary_monitor().ok().flatten()?;
    let screen = monitor.size();
    let win_size = win.outer_size().ok()?;
    const MARGIN: i32 = 24;
    let x = (screen.width as i32 - win_size.width as i32 - MARGIN).max(0);
    let y = (screen.height as i32 - win_size.height as i32 - MARGIN).max(0);
    Some(PhysicalPosition::new(x, y))
}

/// 启动时按 [`AppSettings::pet`] 的 `enabled` 恢复显示(被动,不弹通知,避免开机噪声)。
pub fn restore_on_startup(app: &AppHandle) -> crate::error::Result<()> {
    // 1. 读取有效配置，读取失败交由启动入口处理
    let s = AppSettings::load()?;
    // 2. 按保存的宠物设置恢复显示
    if s.pet.enabled {
        apply_visibility(app, true, s.pet.position);
    }
    Ok(())
}
