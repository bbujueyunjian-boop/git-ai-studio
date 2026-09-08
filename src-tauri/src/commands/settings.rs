//! 应用偏好(主题、扫描根目录、通知开关等)。

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};

use crate::state::{AppSettings, AppState};

const LOW_AI_SHARE_MIN_REMIND_INTERVAL_MINUTES: u32 = 5;
const LOW_AI_SHARE_MAX_REMIND_INTERVAL_MINUTES: u32 = 24 * 60;
const LOW_AI_SHARE_MIN_DISMISS_MINUTES: u32 = 5;
const LOW_AI_SHARE_MAX_DISMISS_MINUTES: u32 = 7 * 24 * 60;

/// 增量 patch:所有字段都是可选,只覆盖前端显式给出的字段。
///
/// # 扁平化设计
/// 后端持久化结构 [`AppSettings`] 用嵌套(`notifications.cc_switch_auto_repair` 等),
/// 但 patch 这里保持扁平,前端调用更直接,且增量语义更清楚("只想改 enabled 就只发 enabled")。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettingsPatch {
    pub scan_roots: Option<Vec<String>>,
    pub theme: Option<String>,
    /// "exit" | "tray";其它字面被后端校验拒绝。
    pub close_behavior: Option<String>,
    /// 通知:cc-switch 守护开关。
    pub cc_switch_auto_repair: Option<bool>,
    /// 通知:低 AI 占比提醒总开关。
    pub low_ai_share_enabled: Option<bool>,
    /// 通知:低 AI 占比阈值百分比;后端会 `clamp(1, 100)`。
    pub low_ai_share_threshold_percent: Option<u32>,
    /// 通知:低 AI 占比手动关注邮箱;空列表表示前端自动取当前仓库 git user.email。
    pub low_ai_share_target_emails: Option<Vec<String>>,
    /// 通知:重复提醒间隔(分钟);后端会 clamp 到 [5, 1440]。
    pub low_ai_share_remind_interval_minutes: Option<u32>,
    /// 通知:用户点 X 后静默时长(分钟);后端会 clamp 到 [5, 10080]。
    pub low_ai_share_dismiss_minutes: Option<u32>,
    /// 通知:低 AI 占比"实时"开关。开启后 fsnotify 监听 refs/notes/ai 文件变化,
    /// commit 完成 1-3s 内推送;关闭走 15 分钟轮询。默认 true。
    pub low_ai_share_realtime_enabled: Option<bool>,
    /// 通知:git-ai daemon 异常推送 OS 通知的独立总开关。
    pub daemon_unhealthy_alert: Option<bool>,
    pub repo_setup_seen: Option<bool>,
    /// 桌面宠物:总开关。翻转即时显隐 pet 窗口。
    pub pet_enabled: Option<bool>,
    /// 桌面宠物:形象主题 id(robot3d / robotflat / inkbeast)。
    pub pet_theme_id: Option<String>,
    /// 桌面宠物:拖拽后记忆的窗口位置(physical x, y)。
    pub pet_position: Option<(i32, i32)>,
    /// 桌面宠物:尺寸档位(small / medium / large)。
    pub pet_size: Option<String>,
    /// 桌面宠物:整体不透明度 [0.2, 1.0];后端 clamp。
    pub pet_opacity: Option<f32>,
    /// 桌面宠物:醒目提醒重复间隔(秒);0 = 不重复。后端 clamp 到 [0, 600]。
    pub pet_alert_interval_sec: Option<u32>,
}

/// 读取应用设置；读取或解析失败通过 IPC 返回明确错误。
#[tauri::command]
pub async fn get_app_settings() -> Result<AppSettings, String> {
    // 1. 仅从已验证的配置构造返回值
    AppSettings::load().map_err(Into::into)
}

/// 将显式设置项合并到有效配置，保存成功后再同步 watcher 和窗口状态。
#[tauri::command]
pub async fn set_app_settings(
    app: AppHandle,
    state: State<'_, AppState>,
    patch: AppSettingsPatch,
) -> Result<AppSettings, String> {
    // 1. 严格读取已有配置，失败时停止更新
    let mut s = AppSettings::load()?;
    let prev_cc_switch = s.notifications.cc_switch_auto_repair;
    let prev_pet_enabled = s.pet.enabled;
    let prev_low_ai_enabled = s.notifications.low_ai_share.enabled;
    let prev_low_ai_realtime = s
        .notifications
        .low_ai_share
        .realtime_enabled
        .unwrap_or(true);
    // 2. 校验并合并本次显式提交的设置项
    if let Some(roots) = patch.scan_roots {
        s.scan_roots = roots;
    }
    if let Some(theme) = patch.theme {
        s.theme = Some(theme);
    }
    if let Some(cb) = patch.close_behavior {
        match cb.as_str() {
            "exit" | "tray" => s.close_behavior = Some(cb),
            other => {
                return Err(format!(
                    "close_behavior 仅接受 'exit' / 'tray',收到 '{other}'"
                ))
            }
        }
    }
    if let Some(b) = patch.cc_switch_auto_repair {
        s.notifications.cc_switch_auto_repair = b;
    }
    if let Some(b) = patch.low_ai_share_enabled {
        s.notifications.low_ai_share.enabled = b;
    }
    if let Some(n) = patch.low_ai_share_threshold_percent {
        // 阈值物理意义在 [1, 100];0 会让"占比 ≥ 0"恒成立 → 永远触发,语义损坏。
        s.notifications.low_ai_share.threshold_percent = Some(n.clamp(1, 100));
    }
    if let Some(emails) = patch.low_ai_share_target_emails {
        s.notifications.low_ai_share.target_emails = normalize_email_list(emails);
    }
    if let Some(n) = patch.low_ai_share_remind_interval_minutes {
        s.notifications.low_ai_share.remind_interval_minutes = Some(n.clamp(
            LOW_AI_SHARE_MIN_REMIND_INTERVAL_MINUTES,
            LOW_AI_SHARE_MAX_REMIND_INTERVAL_MINUTES,
        ));
    }
    if let Some(n) = patch.low_ai_share_dismiss_minutes {
        s.notifications.low_ai_share.dismiss_minutes = Some(n.clamp(
            LOW_AI_SHARE_MIN_DISMISS_MINUTES,
            LOW_AI_SHARE_MAX_DISMISS_MINUTES,
        ));
    }
    if let Some(b) = patch.low_ai_share_realtime_enabled {
        s.notifications.low_ai_share.realtime_enabled = Some(b);
    }
    if let Some(b) = patch.daemon_unhealthy_alert {
        s.notifications.daemon_unhealthy_alert = b;
    }
    if let Some(b) = patch.repo_setup_seen {
        s.repo_setup_seen = b;
    }
    if let Some(b) = patch.pet_enabled {
        s.pet.enabled = b;
    }
    if let Some(t) = patch.pet_theme_id {
        s.pet.theme_id = Some(t);
    }
    if let Some(p) = patch.pet_position {
        s.pet.position = Some(p);
    }
    if let Some(sz) = patch.pet_size {
        match sz.as_str() {
            "small" | "medium" | "large" => s.pet.size = Some(sz),
            other => {
                return Err(format!(
                    "pet_size 仅接受 'small' / 'medium' / 'large',收到 '{other}'"
                ))
            }
        }
    }
    if let Some(o) = patch.pet_opacity {
        s.pet.opacity = Some(o.clamp(0.2, 1.0));
    }
    if let Some(n) = patch.pet_alert_interval_sec {
        s.pet.alert_interval_sec = Some(n.min(600));
    }
    // 3. 配置持久化成功后才更新外部运行状态
    s.save().map_err(|e| format!("写入设置失败: {e}"))?;

    // 4. 同步运行中的监控与窗口
    // 4.1 cc-switch watcher 即时联动:开关翻转就启/停,无需重启应用。
    let now_cc_switch = s.notifications.cc_switch_auto_repair;
    if prev_cc_switch != now_cc_switch {
        crate::cc_switch_watcher::apply_enabled(&app, &state, now_cc_switch);
    }

    // 4.2 refs/notes/ai 实时 watcher 联动:任意一个开关变化都重新应用一次(幂等)。
    // 默认 true:realtime_enabled = None 视为开启。
    let now_low_ai_enabled = s.notifications.low_ai_share.enabled;
    let now_low_ai_realtime = s
        .notifications
        .low_ai_share
        .realtime_enabled
        .unwrap_or(true);
    if prev_low_ai_enabled != now_low_ai_enabled || prev_low_ai_realtime != now_low_ai_realtime {
        let realtime_active = now_low_ai_enabled && now_low_ai_realtime;
        let repo_path = state
            .current_repo
            .read()
            .ok()
            .and_then(|g| g.as_ref().map(|r| r.path.clone()));
        crate::repo_notes_watcher::apply_state(&app, &state, repo_path.as_deref(), realtime_active);
    }

    // 桌面宠物窗口显隐联动:仅 enabled 翻转才显 / 隐(主题、位置改变不影响显隐)。
    if prev_pet_enabled != s.pet.enabled {
        crate::pet::apply_visibility(&app, s.pet.enabled, s.pet.position);
    }

    Ok(s)
}

/// 查询应用「开机自启」当前状态(真源 = 操作系统登录项,非 app config)。
#[tauri::command]
pub async fn get_auto_launch_status() -> Result<bool, String> {
    crate::auto_launch::is_auto_launch_enabled()
        .await
        .map_err(|e| e.to_string())
}

/// 切换应用「开机自启」。返回切换后的实际状态,供前端回显。
#[tauri::command]
pub async fn set_auto_launch(enabled: bool) -> Result<bool, String> {
    if enabled {
        crate::auto_launch::enable_auto_launch()
            .await
            .map_err(|e| e.to_string())?;
    } else {
        crate::auto_launch::disable_auto_launch()
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(enabled)
}

fn normalize_email_list(emails: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = emails
        .into_iter()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::normalize_email_list;

    #[test]
    fn normalizes_low_ai_target_emails() {
        let emails = normalize_email_list(vec![
            " Bob@Example.com ".to_string(),
            "alice@example.com".to_string(),
            "bob@example.com".to_string(),
            "".to_string(),
        ]);
        assert_eq!(emails, vec!["alice@example.com", "bob@example.com"]);
    }
}
