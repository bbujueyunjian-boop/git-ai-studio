//! 监听当前仓库的 `refs/notes/ai` 变化,commit 完成后 1-3s 内通知前端立即重拉指标。
//!
//! # 用途
//! 替代 LowAiShareWatcher 15 分钟轮询的实时路径。git-ai post-commit hook 写入
//! `refs/notes/ai/<sha>` 后,fsnotify 捕获文件变化 → debounce 1.5s 合并多事件 →
//! Tauri emit 给前端 → react-query invalidate → 自动重拉 history / people。
//!
//! # 监听对象
//! - `<repo>/.git/refs/notes/`(目录,NonRecursive)— loose ref 写入路径
//! - `<repo>/.git/packed-refs`(单文件)— git gc 紧凑化后 loose ref 被 pack 进此文件
//!
//! 二者择一即可监听到所有 refs/notes/ai 变化;同时监听是冗余兜底,代价是两个 watch
//! handle(notify-debouncer-mini 内部 ReadDirectoryChangesW,内核级零开销)。
//!
//! # 启动条件
//! [`AppSettings.notifications.low_ai_share.enabled`] = `true`
//! **且** `low_ai_share.realtime_enabled.unwrap_or(true)` = `true`
//! **且** 存在选中仓库(`current_repo / last_repo`)。
//!
//! 任一条件不满足或仓库无 `.git/` 目录时,watcher 不启动 / 自动停止。
//!
//! # 与 cc_switch_watcher 的差异
//! cc_switch_watcher 监听 home 目录下的固定文件(不随仓库变化);本 watcher 跟随当前
//! 仓库切换,生命周期与 `AppState.current_repo` 绑定。

use std::path::Path;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use notify_debouncer_mini::{
    new_debouncer,
    notify::{RecommendedWatcher, RecursiveMode},
    DebounceEventResult, Debouncer,
};
use serde_json::json;
use tauri::{AppHandle, Emitter};

use crate::state::{AppSettings, AppState};

/// 防抖窗口。git post-commit hook 写 ref 可能多步(`.lock` → rename),1500ms 足够合并;
/// 同时让连续多个 commit(用户狂 batch 提交)合并成一次前端 refetch,避免抖动。
const WATCHER_DEBOUNCE_MS: u64 = 1500;

/// Tauri event topic。前端 LowAiShareWatcher 通过 `listen` 订阅。
pub const NOTES_UPDATED_EVENT: &str = "git-ai-studio://notes-updated";

/// fsnotify watcher 句柄。Drop 时停止监听 + 显式 join worker。
pub struct NotesWatcherHandle {
    /// 当前监听的仓库绝对路径。用于"重复调 apply_state 同路径时跳过重启"。
    repo_path: String,
    /// 字段顺序就是 Drop 顺序(逆序):_stop_sentinel 先 drop 通知 worker 退出,然后
    /// worker join,最后 _debouncer 停止文件监听。
    stop_tx: Option<mpsc::Sender<()>>,
    worker: Option<JoinHandle<()>>,
    debouncer: Option<Debouncer<RecommendedWatcher>>,
}

impl Drop for NotesWatcherHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        let _ = self.debouncer.take();
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

/// 统一启停入口。多次同入参调用幂等(已在监听该 repo 时不重启)。
///
/// # 决策表
/// - `enabled = false` 或 `repo_path = None` → 停止当前 watcher(若有)
/// - `enabled = true` 且 `repo_path = Some(path)`:
///   - 已在监听同一 path → 不动(幂等)
///   - 监听不同 path / 未监听 → 替换为新 watcher
pub fn apply_state(
    app: &AppHandle,
    state: &tauri::State<'_, AppState>,
    repo_path: Option<&str>,
    enabled: bool,
) {
    let mut guard = match state.repo_notes_watcher.lock() {
        Ok(g) => g,
        Err(_) => return,
    };

    if !enabled {
        if guard.is_some() {
            let old = guard.take();
            log::info!("repo_notes_watcher: 已停止(开关关闭)");
            drop(guard);
            drop(old);
        }
        return;
    }

    let Some(path) = repo_path else {
        // 开关开但没仓库 — 停止当前 watcher 等下次切仓再起
        if guard.is_some() {
            let old = guard.take();
            log::debug!("repo_notes_watcher: 暂停(无选中仓库)");
            drop(guard);
            drop(old);
        }
        return;
    };

    // 幂等:同路径已在监听就不重启
    if let Some(h) = guard.as_ref() {
        if h.repo_path == path {
            return;
        }
    }

    match spawn_watcher(app.clone(), path) {
        Ok(h) => {
            let old = guard.replace(h);
            log::info!("repo_notes_watcher: 已挂载到 {}", path);
            drop(guard);
            drop(old);
        }
        Err(e) => {
            // 启动失败不致命:可能是仓库刚 init 还没 refs/notes/ 目录,留待用户首次
            // commit 后下一轮 apply_state 重试。不弹 toast 避免 onboarding 噪音。
            log::warn!("repo_notes_watcher: 启动失败 ({path}): {e}");
            let old = guard.take();
            drop(guard);
            drop(old);
        }
    }
}

/// 启动时按 settings 决定是否恢复 watcher。
pub fn restore_on_startup(
    app: &AppHandle,
    state: &tauri::State<'_, AppState>,
) -> crate::error::Result<()> {
    // 1. 读取有效配置，读取失败交由启动入口处理
    let s = AppSettings::load()?;
    // 2. 根据保存的开关恢复上次仓库监听
    let low_ai_enabled = s.notifications.low_ai_share.enabled;
    // 默认 true:realtime_enabled = None / true 都视为开启
    let realtime_enabled = s
        .notifications
        .low_ai_share
        .realtime_enabled
        .unwrap_or(true);
    if !(low_ai_enabled && realtime_enabled) {
        return Ok(());
    }
    if let Some(repo_path) = s.last_repo.as_deref() {
        apply_state(app, state, Some(repo_path), true);
    }
    Ok(())
}

fn spawn_watcher(app: AppHandle, repo_path: &str) -> Result<NotesWatcherHandle, String> {
    let git_dir = Path::new(repo_path).join(".git");
    if !git_dir.is_dir() {
        return Err(format!("{} 不是 git 仓库(.git 不存在)", repo_path));
    }
    let notes_dir = git_dir.join("refs").join("notes");
    let packed_refs = git_dir.join("packed-refs");

    let (event_tx, event_rx) = mpsc::channel::<DebounceEventResult>();
    let (stop_tx, stop_rx) = mpsc::channel::<()>();

    let mut debouncer = new_debouncer(Duration::from_millis(WATCHER_DEBOUNCE_MS), move |res| {
        let _ = event_tx.send(res);
    })
    .map_err(|e| format!("创建 debouncer 失败: {e}"))?;

    // 监听 refs/notes/ 目录(NonRecursive,避免无关 ref 也触发)
    let notes_watched = watch_path_or_parent(&mut debouncer, &notes_dir);
    // 监听 packed-refs 文件(git gc 后 loose ref 被 pack 进此文件)
    let packed_watched = watch_path_or_parent(&mut debouncer, &packed_refs);

    if !notes_watched && !packed_watched {
        // refs/notes/ 不存在且 packed-refs 也不存在 — 全新 git 仓库还没有任何 notes。
        // 不是错误,但本次 spawn 无意义;留待用户首次 commit + git-ai checkpoint 写 ref
        // 后下一轮 apply_state 重试。
        return Err(
            "仓库还没有 refs/notes/ 目录或 packed-refs,等首次 git-ai 写入 ref 后再试".to_string(),
        );
    }

    let app_for_worker = app.clone();
    let repo_path_for_emit = repo_path.to_string();
    let worker = thread::spawn(move || loop {
        match stop_rx.try_recv() {
            Ok(_) | Err(mpsc::TryRecvError::Disconnected) => break,
            Err(mpsc::TryRecvError::Empty) => {}
        }
        match event_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(_) => {
                // 触发事件:emit 给前端,载荷带 repo_path 让前端可以做幂等(收到
                // 切仓后的旧事件可丢弃)
                let _ = app_for_worker.emit(
                    NOTES_UPDATED_EVENT,
                    json!({ "repo_path": repo_path_for_emit }),
                );
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    });

    Ok(NotesWatcherHandle {
        repo_path: repo_path.to_string(),
        stop_tx: Some(stop_tx),
        worker: Some(worker),
        debouncer: Some(debouncer),
    })
}

/// 复用 cc_switch_watcher 的"路径不存在就监听父目录"策略:对于 `refs/notes/` 目录
/// 不存在的新仓库,监听 `refs/` 等待 notes/ 被创建。
fn watch_path_or_parent(debouncer: &mut Debouncer<RecommendedWatcher>, path: &Path) -> bool {
    if path.exists() {
        return debouncer
            .watcher()
            .watch(path, RecursiveMode::NonRecursive)
            .is_ok();
    }
    if let Some(parent) = path.parent() {
        if parent.exists() {
            return debouncer
                .watcher()
                .watch(parent, RecursiveMode::NonRecursive)
                .is_ok();
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Instant;

    /// 仅用于测试:验证路径拼接行为(Windows 反斜杠 / POSIX 斜杠不影响逻辑判定)
    fn git_dir_of(repo_path: &str) -> PathBuf {
        Path::new(repo_path).join(".git")
    }

    #[test]
    fn git_dir_helper_join() {
        let p = git_dir_of("D:\\repo");
        assert!(p.ends_with(".git"));
    }

    #[test]
    fn watcher_handle_drop_stops_worker() {
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let (_event_tx, event_rx) = mpsc::channel::<DebounceEventResult>();
        let worker = thread::spawn(move || loop {
            match stop_rx.try_recv() {
                Ok(_) | Err(mpsc::TryRecvError::Disconnected) => break,
                Err(mpsc::TryRecvError::Empty) => {}
            }
            match event_rx.recv_timeout(Duration::from_millis(10)) {
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        });

        let mut handle = NotesWatcherHandle {
            repo_path: "D:\\repo".to_string(),
            stop_tx: Some(stop_tx),
            worker: Some(worker),
            debouncer: None,
        };

        let started_at = Instant::now();
        handle.stop_tx.take().unwrap().send(()).unwrap();
        handle.worker.take().unwrap().join().unwrap();
        assert!(started_at.elapsed() < Duration::from_secs(1));
    }
}
