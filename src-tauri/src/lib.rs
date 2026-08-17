// git-ai-studio 后端入口。
//
// 模块分层:
//   error      统一 AppError + Result alias
//   paths      跨平台路径 helper(~/.git-ai、~/.claude 等)
//   state      AppState(当前仓库、诊断缓存、应用偏好)
//   proc       跨平台子进程封装(超时 + Windows CREATE_NO_WINDOW)
//   git_ai     git-ai CLI 包装(binary 路径 / debug report 解析 / notes 读取)
//   agents     7 个 AI Agent 的 hook 探测(Claude/Cursor 完整,其余 5 占位)
//   repo       仓库扫描与 HEAD 读取
//   commands   #[tauri::command] 暴露层

pub mod agent_cli;
pub mod agents;
pub mod auto_launch;
pub mod cc_switch_watcher;
pub mod commands;
pub mod db;
pub mod env_path;
pub mod error;
pub mod git_ai;
pub mod hooks;
pub mod installer;
pub mod paths;
pub mod pet;
pub mod proc;
pub mod repo;
pub mod repo_notes_watcher;
pub mod state;

use state::{AppSettings, AppState, CloseBehavior};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager, WindowEvent,
};

/// 把当前 [`AppSettings::close_behavior`] 解析为 [`CloseBehavior`]。
/// 每次窗口 close 实时读盘,而不是缓存到内存 —— 配置改了立刻生效,无需重启。
fn current_close_behavior() -> CloseBehavior {
    CloseBehavior::from_settings(AppSettings::load().close_behavior.as_deref())
}

/// 把主窗口从托盘 / 隐藏态恢复并聚焦。
fn restore_main_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 最早修正进程 PATH:GUI 从 Finder/资源管理器启动只继承最小 PATH,nvm/Homebrew/fnm
    // 装的 node/npm/claude/codex 不可见。须在任何子进程 spawn、tokio 多线程起来之前打补丁。
    env_path::ensure_patched();

    // 日志统一走 `log` crate + `tauri-plugin-log`(下方 builder 链注册)。
    // 不再装 tracing-subscriber:它默认带 tracing-log 桥接,会抢 `log::set_logger` 的 slot,
    // 导致随后 tauri-plugin-log 注册时 panic("attempted to set a logger after ...")。
    let db = db::open().expect("打开 SQLite 失败,无法启动 git-ai-studio");

    let mut builder = tauri::Builder::default();

    // 单实例守护:第二个进程启动时,插件经平台级 IPC 检测到已有实例,转交参数后直接退出;
    // 已有实例在回调里唤起主窗口。须最先注册(任何插件/窗口初始化之前),接入方式对齐 cc-switch。
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            restore_main_window(app);
        }));
    }

    builder
        .plugin(tauri_plugin_log::Builder::new().build())
        .plugin(tauri_plugin_opener::init())
        // 原生目录选择器(RepoSetupGuide wizard step 2 + Repo 页扫描根目录)
        .plugin(tauri_plugin_dialog::init())
        // OS 原生通知(macOS 通知中心 / Linux libnotify / Windows toast):
        // LowAiShareWatcher + DaemonWatcher 的告警出口,前端走
        // `@tauri-apps/plugin-notification` 调用,无需新增后端 command。
        .plugin(tauri_plugin_notification::init())
        // 更新安装完成后重启进程(updater 的 install-then-restart 流程依赖它)
        .plugin(tauri_plugin_process::init())
        .manage(AppState::new(db))
        .setup(|app| {
            // 注册 Updater 插件(桌面端)。
            // 若配置不完整(如 tauri.conf 的 pubkey 仍是占位符),跳过 Updater 而不中断应用启动。
            #[cfg(desktop)]
            {
                if let Err(e) = app
                    .handle()
                    .plugin(tauri_plugin_updater::Builder::new().build())
                {
                    log::warn!("初始化 Updater 插件失败,已跳过:{e}");
                }
            }

            // 系统托盘 — 仅当用户在 Settings 选了"最小化到托盘"时才会真的隐藏窗口到此处,
            // 但图标始终注册,菜单提供「显示主窗口 / 退出」入口。
            let show_item = MenuItem::with_id(app, "tray_show", "显示主窗口", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "tray_quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_item, &quit_item])?;

            let _tray = TrayIconBuilder::with_id("git-ai-studio-tray")
                .icon(app.default_window_icon().cloned().ok_or("缺少窗口图标")?)
                .tooltip("Git AI Studio")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "tray_show" => restore_main_window(app),
                    "tray_quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    // 左键单击托盘图标 → 恢复主窗口(右键由 menu 自动处理)。
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        restore_main_window(tray.app_handle());
                    }
                })
                .build(app)?;

            // 根据 AppSettings 恢复 cc-switch 守护(用户上次开过就继续开)
            let state = app.state::<AppState>();
            cc_switch_watcher::restore_on_startup(&app.handle().clone(), &state);
            // 同样恢复 refs/notes/ai 实时 watcher(用户开了低 AI 提醒 + realtime 时)
            repo_notes_watcher::restore_on_startup(&app.handle().clone(), &state);
            // 桌面宠物:用户上次开过就恢复显示(被动,不弹通知)。详见 ADR-011。
            pet::restore_on_startup(&app.handle().clone());

            Ok(())
        })
        .on_window_event(|window, event| {
            // 窗口关闭事件按 label 分流:
            //   - main:按 Settings.close_behavior 决定 "exit"(默认,进程退出)或 "tray"(拦截 → hide)
            //   - pet:永远拦截 close 改为 hide,重新开启宠物时即时复用,不重建窗口(见 ADR-011)
            if let WindowEvent::CloseRequested { api, .. } = event {
                match window.label() {
                    // main + exit 模式、以及其它 label 都落到 `_`,不拦截 → 走默认关闭
                    "main" if current_close_behavior() == CloseBehavior::Tray => {
                        api.prevent_close();
                        let _ = window.hide();
                    }
                    "pet" => {
                        api.prevent_close();
                        let _ = window.hide();
                    }
                    _ => {}
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::ping,
            commands::resolve_git_ai_path,
            commands::diagnostic::diagnose_environment,
            commands::diagnostic::invalidate_diagnostic_cache,
            commands::diagnostic::check_agent_hooks,
            commands::diagnostic::diagnose_git_ai_daemon,
            commands::diagnostic::repair_git_ai_daemon,
            commands::repo::discover_repos,
            commands::repo::select_repo,
            commands::repo::current_repo,
            commands::repo::current_git_user_email,
            commands::repo::detect_dirty,
            commands::repo::list_recent_repos,
            commands::repo::clear_recent_repos,
            commands::repo::list_scan_roots,
            commands::repo::set_scan_roots,
            commands::repo::get_aggregate_repos,
            commands::repo::set_aggregate_repos,
            commands::repo::restore_last_repo,
            commands::repo::open_in_explorer,
            commands::install::list_releases,
            commands::install::get_installed_version,
            commands::install::is_install_running,
            commands::install::install_git_ai,
            commands::install::uninstall_git_ai,
            commands::install::get_git_ai_config,
            commands::install::set_git_ai_config,
            commands::install::set_auto_update,
            commands::install::install_history,
            commands::agent_cli::refresh_path_env,
            commands::agent_cli::detect_npm,
            commands::agent_cli::detect_agent_cli,
            commands::agent_cli::install_agent_cli,
            commands::agent_cli::uninstall_agent_cli,
            commands::settings::get_app_settings,
            commands::settings::set_app_settings,
            commands::settings::get_auto_launch_status,
            commands::settings::set_auto_launch,
            commands::hooks::get_hooks_status,
            commands::hooks::read_claude_settings,
            commands::hooks::list_settings_backups,
            commands::hooks::restore_claude_settings,
            commands::hooks::claude_settings_merge,
            commands::hooks::install_hooks_official,
            commands::hooks::install_hooks_for_agent,
            commands::stats::get_commit_stats,
            commands::stats::get_commit_status,
            commands::stats::list_recent_commits,
            commands::history::get_history,
            commands::history::list_recent_commits_with_stats,
            commands::history::get_aggregate_history,
            commands::history::get_aggregate_working_status,
            commands::history::get_range_summary,
            commands::history::clear_stats_cache,
            commands::people::get_people_breakdown,
            commands::blame::get_blame,
            commands::blame::list_files_at_head,
            commands::blame::read_file_at_head,
            commands::blame::get_blame_at_ref,
            commands::blame::list_files_at_ref,
            commands::blame::read_file_at_ref,
            commands::diff::list_changed_files_in_commit,
            commands::diff::list_ai_lines_in_commit,
            commands::notes::list_ai_notes,
            commands::notes::show_ai_note,
            commands::checkpoints::list_checkpoints,
            commands::checkpoints::is_mock_running,
            commands::checkpoints::git_status_porcelain,
            commands::checkpoints::mock_checkpoint,
            commands::logs::read_log_file,
            commands::logs::run_git_ai_debug_report,
            commands::logs::open_log_dir,
            commands::ignore::list_effective_ignore_patterns,
            commands::auth::get_whoami,
            commands::show::get_show_raw,
            commands::branches::list_branches,
            commands::branches::checkout_branch,
        ])
        .run(tauri::generate_context!())
        .expect("git-ai-studio failed to start");
}
