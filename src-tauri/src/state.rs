use std::path::PathBuf;
use std::sync::{Mutex, RwLock};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::cc_switch_watcher::WatcherHandle;
use crate::db::Db;
use crate::repo::commits::CommitBrief;

/// 跨命令共享的全局状态。在 `lib.rs` 的 Tauri Builder 上用 `.manage(...)` 注入。
pub struct AppState {
    /// 用户当前选中的仓库;所有后续 git/git-ai 命令以此为 CWD。
    pub current_repo: RwLock<Option<RepoEntry>>,
    /// `diagnostic_overview` 的进程内缓存(60s TTL,UI 主动 force 可绕过)。
    pub diag_cache: RwLock<Option<CachedDiag>>,
    /// install / uninstall 长任务的当前 job。
    /// 用 std `RwLock` 让 `is_install_running` 走非阻塞 read 路径,
    /// 抢占由 write 路径的 `try_write` + 内容判定来做。
    pub install_lock: RwLock<Option<String>>,
    /// hooks 切换的短任务锁(秒级)。与 install_lock 不互抢,但跨锁 precondition 严格:
    /// install 进行时不允许 hooks 切换,反之亦然。
    pub hooks_lock: RwLock<Option<String>>,
    /// `mock_checkpoint` 短任务锁。**危险动作**:会向 `.git/ai/working_logs/<sha>/checkpoints.jsonl`
    /// 真实追加 checkpoint,且 git-ai 无 CLI 撤销路径。
    /// 与 install_lock / hooks_lock 三者两两互斥(任一进行中,其它都拒绝)。
    pub mock_lock: RwLock<Option<String>>,
    /// `list_recent_commits` 的进程内缓存(30s TTL,按 repo_path+max_count 做 key)。
    /// 切仓库时由 `commands::repo::select_repo` 主动清空;install / hooks 操作不动该缓存
    /// (git log 不受 git-ai 安装影响,无需对称失效)。
    pub commits_cache: RwLock<Option<CachedCommits>>,
    /// `~/.git-ai-studio/studio.sqlite` 的单连接。P5 起承载 `commit_stats_cache`,后续表
    /// 按 `user_version` migration 增量加。
    /// 跨锁约束:**绝不**在持有 `db.lock()` 期间再去 acquire `current_repo` / `commits_cache`,
    /// 否则有死锁可能。约定调用方先 clone path/sha 出 RwLock,再走 db。
    pub db: Db,
    /// cc-switch 文件 watcher 的运行句柄。`None` = 未启动(默认),`Some` = 正在监听。
    /// 由 [`NotificationsConfig::cc_switch_auto_repair`] 开关驱动 spawn / drop。
    pub cc_switch_watcher: Mutex<Option<WatcherHandle>>,
    /// 当前仓库 `refs/notes/ai` 变化的 watcher 句柄。
    /// 由 `low_ai_share.enabled` && `low_ai_share.realtime_enabled` 联合驱动 spawn / drop;
    /// 切仓时 unwatch 旧路径并 watch 新路径。详见 [`crate::repo_notes_watcher`]。
    pub repo_notes_watcher: Mutex<Option<crate::repo_notes_watcher::NotesWatcherHandle>>,
}

impl AppState {
    /// 启动时构造。db 打开 / migration 失败直接 panic(no fallback);
    /// 内嵌 SQLite 失败意味着磁盘 / 权限严重异常,UI 也跑不起来。
    pub fn new(db: Db) -> Self {
        Self {
            current_repo: RwLock::new(None),
            diag_cache: RwLock::new(None),
            install_lock: RwLock::new(None),
            hooks_lock: RwLock::new(None),
            mock_lock: RwLock::new(None),
            commits_cache: RwLock::new(None),
            db,
            cc_switch_watcher: Mutex::new(None),
            repo_notes_watcher: Mutex::new(None),
        }
    }
}

/// 已选中或扫描出来的仓库摘要。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoEntry {
    /// 绝对路径(规整化后)。
    pub path: String,
    /// 仓库名(目录名)。
    pub name: String,
    /// 当前 HEAD 分支名;detached 时为 None。
    pub head_branch: Option<String>,
    /// HEAD commit 完整 SHA。
    pub head_sha: Option<String>,
    /// 工作区是否有未提交改动;`None` 表示尚未探测(扫描列表态下)。
    /// 真实值通过子进程调 `git status --porcelain` 在选中后异步填充,见 commands::repo::detect_dirty。
    pub dirty: Option<bool>,
    /// `.git/ai/` 目录是否存在(粗略反映曾经使用过 git-ai)。
    pub has_git_ai_dir: bool,
    /// `.git/ai/working_logs/<head_sha>/` 下的 .jsonl 数量;0 表示当前 HEAD 没有 checkpoint。
    /// 这个数字是后续 stats 准确性的关键前置:为 0 时 ai_accepted 必为 0。
    pub working_logs_count: u32,
}

/// 缓存 `diagnostic_overview` 的快照 + 采集时间。
pub struct CachedDiag {
    pub value: serde_json::Value,
    pub at: Instant,
}

/// 缓存 `list_recent_commits` 的快照。命中条件:repo_path + max_count 都匹配,且 30s 内。
pub struct CachedCommits {
    pub repo_path: String,
    pub max_count: u32,
    pub at: Instant,
    pub items: Vec<CommitBrief>,
}

/// 应用自身的偏好(写入 ~/.git-ai-studio/config.json)。
///
/// # 嵌套通知配置
/// `notifications` 子结构集中所有"打扰用户"类开关:cc-switch 守护、低 AI 占比提醒等,
/// 后续 #9 hook-server 异常提醒等也会进这里。避免顶层字段无限膨胀。
///
/// # 迁移
/// 历史顶层字段 `cc_switch_auto_repair` 仍可被旧 config.json 反序列化(保留 Option 字段,
/// 默认 None),`AppSettings::load()` 会把它迁移到新位置 + 记一行 `log::info!`。
/// 旧 `retention_days` 字段已彻底删除,serde 默认忽略未知字段,旧 config 不会因此报错。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppSettings {
    #[serde(default)]
    pub scan_roots: Vec<String>,
    #[serde(default)]
    pub recent_repos: Vec<String>,
    #[serde(default)]
    pub last_repo: Option<String>,
    /// "light" | "dark" | "system";None 视为 "system"。
    #[serde(default)]
    pub theme: Option<String>,
    /// 主窗口被关闭时的行为:"exit"(默认,直接退出进程)| "tray"(隐藏到系统托盘,
    /// 左键点托盘或菜单"显示主窗口"恢复)。None 视为 "exit",与历史行为一致。
    #[serde(default)]
    pub close_behavior: Option<String>,
    /// 通知与守护类开关。所有"打扰用户"类配置进这里,避免顶层字段膨胀。
    #[serde(default)]
    pub notifications: NotificationsConfig,
    #[serde(default)]
    pub repo_setup_seen: bool,
    /// 桌面宠物(Ink pet)配置。默认关(opt-in)。详见 ADR-011。
    #[serde(default)]
    pub pet: PetConfig,
    /// 显式勾选"纳入跨仓聚合"的仓库绝对路径集合(已 canonicalize)。
    /// Dashboard / 历史趋势 / 作者归因 的唯一聚合数据源(M1)。
    /// 与 `recent_repos`(下钻 MRU,cap 5)、`current_repo`(单仓下钻焦点)**正交**:
    /// `set_aggregate_repos` 绝不触碰 current_repo / recent_repos,反之亦然。
    /// 空集合 = 合法的"尚未勾选"态(聚合命令返回 NoReposSelected 空态,而非错误)。
    #[serde(default)]
    pub aggregate_repos: Vec<String>,
    // ====== 已废弃 / 迁移用字段 ======
    /// **已废弃**:历史顶层位置;`load()` 会把它迁到 `notifications.cc_switch_auto_repair`。
    /// 用 `skip_serializing_if` 让迁移完成后下一次 save 不再写出。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cc_switch_auto_repair: Option<bool>,
}

/// 通知与守护类配置子结构。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NotificationsConfig {
    /// cc-switch 切换 profile 后,studio 自动恢复缺失的 git-ai hook。
    /// 详见 [`crate::cc_switch_watcher`]。默认 false。
    #[serde(default)]
    pub cc_switch_auto_repair: bool,
    /// 低 AI 占比提醒。默认关。
    #[serde(default)]
    pub low_ai_share: LowAiShareConfig,
    /// git-ai daemon 异常时推送 OS 通知的独立总开关。默认 false。
    ///
    /// # 触发链
    /// 由 `src/components/DaemonWatcher.tsx` 轮询。开关打开后才会推送 OS 通知;
    /// 关闭后立刻停止 30s 轮询(useQuery `enabled` 与 `refetchInterval` 同步禁用)。
    #[serde(default)]
    pub daemon_unhealthy_alert: bool,
}

/// 低 AI 占比提醒配置。
///
/// # 触发条件(前端 LowAiShareWatcher 实现)
/// 近 7 天 `ai_additions / total_additions` 严格小于 `threshold_percent`,且窗口总加行数 ≥ 50,
/// 距上次提醒超过 `remind_interval_minutes`,距上次切仓库 ≥ 5 分钟。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LowAiShareConfig {
    /// 总开关,默认 false。
    #[serde(default)]
    pub enabled: bool,
    /// 阈值百分比 [1, 100];None 视为前端默认 80。后端 `set_app_settings` 对入参 `clamp(1, 100)`。
    #[serde(default)]
    pub threshold_percent: Option<u32>,
    /// 手动关注的作者邮箱列表。空列表表示前端自动使用当前仓库 `git config user.email`;
    /// 若本地 Git 邮箱也为空,再按仓库整体统计。
    #[serde(default)]
    pub target_emails: Vec<String>,
    /// 重复提醒间隔(分钟)。None 视为前端默认 360。
    #[serde(default)]
    pub remind_interval_minutes: Option<u32>,
    /// 用户点 X / 静默按钮后的静默时长(分钟)。None 视为前端默认 1440。
    #[serde(default)]
    pub dismiss_minutes: Option<u32>,
    /// 实时触发开关:开启后,后端 fsnotify 监听 `<repo>/.git/refs/notes/`,
    /// commit 完成后 1-3s 内推送提醒(替代 15 分钟轮询)。None 视为前端默认 true。
    ///
    /// # 行为
    /// - 关 → 走原 15 分钟轮询(`LOW_AI_SHARE_CHECK_INTERVAL_MS`)
    /// - 开 → watcher 监听 refs/notes/ 目录 + packed-refs 兜底,emit
    ///   `git-ai-studio://notes-updated` 事件;前端 listen 后 invalidateQueries 触发立刻重拉
    /// - 既有冷却(切仓 5 分钟 / 提醒间隔 6 小时)在两种模式下都生效
    #[serde(default)]
    pub realtime_enabled: Option<bool>,
}

/// 桌面宠物(Ink pet)配置。默认关。审美层(主题 / 颜色)可由用户切换,信息层(色 → 数据
/// 映射)由前端 renderer 锁死,本结构只存用户偏好。详见 ADR-011。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PetConfig {
    /// 总开关,默认 false(opt-in)。翻转时 `set_app_settings` 即时显隐 pet 窗口。
    #[serde(default)]
    pub enabled: bool,
    /// 选中的形象主题 id;None 视为前端默认 "robot3d"。三套内置:robot3d / robotflat / inkbeast。
    #[serde(default)]
    pub theme_id: Option<String>,
    /// 上次拖拽后的窗口位置(physical x, y);None = 用 tauri.conf 默认位置。
    #[serde(default)]
    pub position: Option<(i32, i32)>,
    /// 尺寸档位:"small" | "medium" | "large";None 视为前端默认 "medium"。
    #[serde(default)]
    pub size: Option<String>,
    /// 整体不透明度 [0.2, 1.0];None 视为前端默认 1.0。后端 `set_app_settings` clamp。
    #[serde(default)]
    pub opacity: Option<f32>,
    /// 醒目提醒重复间隔(秒);0 = 只提醒一次不重复。None 视为前端默认 30。
    #[serde(default)]
    pub alert_interval_sec: Option<u32>,
}

/// 解析 [`AppSettings::close_behavior`] 字符串为枚举。任何非 "tray" 字面值(含 None / 拼写错误)
/// 都退回 `Exit`,保证"误改配置"不会让用户找不到窗口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseBehavior {
    Exit,
    Tray,
}

impl CloseBehavior {
    /// 不实现 `FromStr` trait:那个 trait 接受 `&str` 而非 `Option<&str>`,
    /// 且语义是"字符串 → 枚举",会丢掉"配置缺失 → 默认 Exit"的兼容路径。
    pub fn from_settings(s: Option<&str>) -> Self {
        match s {
            Some("tray") => Self::Tray,
            _ => Self::Exit,
        }
    }
}

impl AppSettings {
    /// 返回应用偏好文件的位置。
    pub fn config_path() -> PathBuf {
        // 1. 定位应用数据目录中的配置文件
        crate::paths::studio_data_dir().join("config.json")
    }

    /// 读取应用偏好；仅文件不存在时初始化默认配置，读取或解析失败原样返回错误。
    pub fn load() -> crate::error::Result<Self> {
        // 1. 从实际配置位置加载并执行现有字段迁移
        Self::load_from_path(&Self::config_path())
    }

    /// 从指定文件读取偏好，供正式加载与隔离文件测试共用。
    fn load_from_path(path: &std::path::Path) -> crate::error::Result<Self> {
        // 1. 仅缺少文件属于首次启动；其它读取失败不得转成默认值
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error.into()),
        };

        // 2. 严格解析后迁移历史字段，损坏内容保持原样
        let mut settings: Self = serde_json::from_str(&raw)?;
        Self::migrate_in_place(&mut settings);
        Ok(settings)
    }

    /// 把旧顶层字段迁移到新嵌套位置。`load()` 启动时走它。
    pub fn migrate_in_place(s: &mut Self) {
        // 1. 将历史开关移入通知配置，并从后续序列化结果移除旧字段
        if let Some(legacy) = s.cc_switch_auto_repair.take() {
            log::info!(
                "config.json: 旧顶层 cc_switch_auto_repair={} 已迁移到 notifications.cc_switch_auto_repair",
                legacy
            );
            s.notifications.cc_switch_auto_repair = legacy;
        }
    }

    /// 保存已成功读取并修改的配置；序列化失败时不得以空内容覆盖文件。
    pub fn save(&self) -> std::io::Result<()> {
        // 1. 将配置写回实际配置位置
        self.save_to_path(&Self::config_path())
    }

    /// 将配置序列化后写入指定文件，供正式保存与隔离文件测试共用。
    fn save_to_path(&self, path: &std::path::Path) -> std::io::Result<()> {
        // 1. 先完整序列化，确保失败不会截断原文件
        let serialized = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;

        // 2. 创建首次启动需要的目录并保存配置
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serialized)
    }
}

#[cfg(test)]
mod tests {
    use super::{AppSettings, CloseBehavior};

    /// 首次启动只返回默认值，直到用户保存才创建配置文件。
    #[test]
    fn missing_settings_initialize_only_on_save() {
        // 1. 验证缺失文件可以初始化，读取本身不会写盘
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("studio/config.json");
        let settings = AppSettings::load_from_path(&path).unwrap();
        assert!(settings.scan_roots.is_empty());
        assert!(!path.exists());

        // 2. 验证正常保存后可严格读取
        settings.save_to_path(&path).unwrap();
        assert!(AppSettings::load_from_path(&path).is_ok());
    }

    /// 修改单项偏好时保留已有有效字段及嵌套配置。
    #[test]
    fn valid_settings_update_preserves_other_fields() {
        // 1. 准备包含多项偏好的合法配置
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"theme":"dark","scan_roots":["repo"],"notifications":{"low_ai_share":{"target_emails":["alice@example.com"]}}}"#,
        )
        .unwrap();

        // 2. 仅更新关闭行为，再验证其余配置不丢失
        let mut settings = AppSettings::load_from_path(&path).unwrap();
        settings.close_behavior = Some("tray".to_string());
        settings.save_to_path(&path).unwrap();
        let saved = AppSettings::load_from_path(&path).unwrap();
        assert_eq!(saved.theme.as_deref(), Some("dark"));
        assert_eq!(saved.scan_roots, vec!["repo"]);
        assert_eq!(
            saved.notifications.low_ai_share.target_emails,
            vec!["alice@example.com"]
        );
        assert_eq!(saved.close_behavior.as_deref(), Some("tray"));
    }

    /// 坏 JSON、非法 UTF-8 和错误字段类型均须中止读取，保留原字节。
    #[test]
    fn invalid_settings_fail_without_changing_original_bytes() {
        // 1. 分别模拟真实配置损坏输入
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        for raw in [
            b"{invalid".as_slice(),
            b"{\"theme\":\"\xff\"}",
            b"{\"unknown_field\":\"\xff\"}",
            b"{\"scan_roots\":false}",
        ] {
            std::fs::write(&path, raw).unwrap();

            // 2. 读取错误必须阻止后续修改和保存
            let result = AppSettings::load_from_path(&path).and_then(|mut settings| {
                settings.theme = Some("light".to_string());
                settings.save_to_path(&path)?;
                Ok(())
            });
            assert!(result.is_err());
            assert_eq!(std::fs::read(&path).unwrap(), raw);
        }
    }

    /// 配置路径是目录等读取异常不能被当作首次启动。
    #[test]
    fn settings_read_error_is_not_a_default_configuration() {
        // 1. 目录存在但无法作为配置文件读取
        let dir = tempfile::tempdir().unwrap();
        assert!(AppSettings::load_from_path(dir.path()).is_err());
        assert!(dir.path().is_dir());
    }

    #[test]
    fn close_behavior_tray_string_maps_to_tray() {
        assert_eq!(
            CloseBehavior::from_settings(Some("tray")),
            CloseBehavior::Tray
        );
    }

    #[test]
    fn close_behavior_exit_string_maps_to_exit() {
        assert_eq!(
            CloseBehavior::from_settings(Some("exit")),
            CloseBehavior::Exit
        );
    }

    #[test]
    fn close_behavior_none_maps_to_exit() {
        // 未设置时与历史行为一致:窗口关闭即退出
        assert_eq!(CloseBehavior::from_settings(None), CloseBehavior::Exit);
    }

    #[test]
    fn close_behavior_unknown_string_falls_back_to_exit() {
        // 误改配置不应让用户找不到窗口:任何拼写错误都退回 Exit
        assert_eq!(
            CloseBehavior::from_settings(Some("minimize")),
            CloseBehavior::Exit
        );
        assert_eq!(CloseBehavior::from_settings(Some("")), CloseBehavior::Exit);
        assert_eq!(
            CloseBehavior::from_settings(Some("Tray")),
            CloseBehavior::Exit
        ); // 大小写敏感
    }

    #[test]
    fn migrate_lifts_legacy_cc_switch_auto_repair() {
        let mut s: AppSettings =
            serde_json::from_str(r#"{"cc_switch_auto_repair": true}"#).unwrap();
        AppSettings::migrate_in_place(&mut s);
        assert!(s.cc_switch_auto_repair.is_none(), "legacy 字段应被清空");
        assert!(s.notifications.cc_switch_auto_repair, "新位置应承接 true");
    }

    #[test]
    fn old_retention_days_field_is_silently_ignored() {
        // 旧字段已彻底删除,serde 默认忽略未知字段 —— 旧 config 含 retention_days 不会报错。
        let s: AppSettings = serde_json::from_str(r#"{"retention_days": 30}"#).unwrap();
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("retention_days"), "废弃字段不应回写: {json}");
    }

    #[test]
    fn fresh_settings_dont_serialize_deprecated_fields() {
        let s = AppSettings::default();
        let value: serde_json::Value = serde_json::to_value(&s).unwrap();
        let top = value.as_object().unwrap();
        // 顶层不应有废弃字段(嵌套 notifications.cc_switch_auto_repair 是合法新位置,不计入)
        assert!(
            !top.contains_key("cc_switch_auto_repair"),
            "默认值不应在顶层序列化 cc_switch_auto_repair: {value}"
        );
    }

    #[test]
    fn nested_low_ai_share_round_trip() {
        let mut s = AppSettings::default();
        s.notifications.low_ai_share.enabled = true;
        s.notifications.low_ai_share.threshold_percent = Some(40);
        s.notifications.low_ai_share.target_emails = vec!["alice@example.com".to_string()];
        s.notifications.low_ai_share.remind_interval_minutes = Some(30);
        s.notifications.low_ai_share.dismiss_minutes = Some(120);
        let json = serde_json::to_string(&s).unwrap();
        let parsed: AppSettings = serde_json::from_str(&json).unwrap();
        assert!(parsed.notifications.low_ai_share.enabled);
        assert_eq!(
            parsed.notifications.low_ai_share.threshold_percent,
            Some(40)
        );
        assert_eq!(
            parsed.notifications.low_ai_share.target_emails,
            vec!["alice@example.com"]
        );
        assert_eq!(
            parsed.notifications.low_ai_share.remind_interval_minutes,
            Some(30)
        );
        assert_eq!(parsed.notifications.low_ai_share.dismiss_minutes, Some(120));
    }

    #[test]
    fn daemon_unhealthy_alert_defaults_off_and_round_trips() {
        let s = AppSettings::default();
        assert!(!s.notifications.daemon_unhealthy_alert);

        let mut s = s;
        s.notifications.daemon_unhealthy_alert = true;
        let json = serde_json::to_string(&s).unwrap();
        let parsed: AppSettings = serde_json::from_str(&json).unwrap();
        assert!(parsed.notifications.daemon_unhealthy_alert);
    }

    #[test]
    fn legacy_feishu_webhook_field_is_silently_ignored() {
        // 旧 config.json 可能仍带 notifications.feishu_webhook 字段;serde 默认忽略未知字段,
        // 旧配置不会因此报错,新结构里也不会再回写出来。
        let raw = r#"{"notifications":{"feishu_webhook":{"enabled":true,"url":"https://open.feishu.cn/x"}}}"#;
        let s: AppSettings = serde_json::from_str(raw).unwrap();
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("feishu_webhook"), "废弃字段不应回写: {json}");
    }

    #[test]
    fn pet_config_defaults_off_and_round_trips() {
        let s = AppSettings::default();
        assert!(!s.pet.enabled, "宠物默认关(opt-in)");
        assert!(s.pet.theme_id.is_none());
        assert!(s.pet.position.is_none());

        let mut s = s;
        s.pet.enabled = true;
        s.pet.theme_id = Some("robotflat".to_string());
        s.pet.position = Some((100, 200));
        s.pet.size = Some("large".to_string());
        s.pet.opacity = Some(0.8);
        s.pet.alert_interval_sec = Some(60);
        let json = serde_json::to_string(&s).unwrap();
        let parsed: AppSettings = serde_json::from_str(&json).unwrap();
        assert!(parsed.pet.enabled);
        assert_eq!(parsed.pet.theme_id.as_deref(), Some("robotflat"));
        assert_eq!(parsed.pet.position, Some((100, 200)));
        assert_eq!(parsed.pet.size.as_deref(), Some("large"));
        assert_eq!(parsed.pet.alert_interval_sec, Some(60));
    }

    #[test]
    fn aggregate_repos_defaults_empty_and_round_trips() {
        // 旧 config 缺该字段 → 空集合(向后兼容,无需迁移代码)。
        let s: AppSettings = serde_json::from_str(r#"{"recent_repos":[]}"#).unwrap();
        assert!(s.aggregate_repos.is_empty(), "缺字段应反序列化为空 Vec");

        let s = AppSettings {
            aggregate_repos: vec!["/a/repo1".to_string(), "/b/repo2".to_string()],
            ..AppSettings::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        let parsed: AppSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.aggregate_repos, vec!["/a/repo1", "/b/repo2"]);
    }
}
