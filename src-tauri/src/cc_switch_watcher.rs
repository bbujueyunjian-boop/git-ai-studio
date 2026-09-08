//! cc-switch(或其它工具)覆盖 Codex / Claude hooks 后的自动恢复 watcher。
//!
//! # 背景
//! cc-switch 切换 Codex profile 时通过 `write_text_file` 整体覆盖 `~/.codex/config.toml`
//! (`cc-switch/src-tauri/src/codex_config.rs:78-125`),git-ai 写入的
//! `[[hooks.PreToolUse / PostToolUse / Stop]]` 段必然丢失。Claude `settings.json`
//! 走深度合并,理论上不丢但作为兜底也监听。
//!
//! # 触发与恢复
//! - 启动条件:[`AppSettings.notifications.cc_switch_auto_repair`] = `true`(默认 false 不启动)
//! - 监听:`~/.codex/config.toml` 与 `~/.claude/settings.json`,文件不存在时监听父目录
//! - 防抖:[`WATCHER_DEBOUNCE_MS`] 毫秒,cc-switch 单次 atomic 写入产生的多个事件被合并
//! - 恢复:复用 [`crate::agents::codex::CodexProbe`] 重测,若 `detected && !configured`
//!   则跑 `git-ai install`(上游 toml_edit 增量编辑,只补 Codex `[hooks.*]` 段,不冲突
//!   cc-switch 写入的 `[model_providers.*]` 等字段)。git-ai install 是全 agent 操作,
//!   会顺带给 Claude `settings.json` 加官方 command hook;若用户 install 前是
//!   None,install 后剥掉这条以保持 Claude 现状(根因债见 [`check_and_repair`])
//! - 冷却:[`REPAIR_COOLDOWN_SECS`] 秒内忽略后续事件;在确定要修(锁到手、git-ai 可用)
//!   时即标记,覆盖 install 写 codex.toml + 回正 clean 写 claude settings.json 两次自触发
//! - 通知:通过 tauri event `cc-switch-watcher://event` 给前端 toast,不流详细日志

use std::path::Path;
use std::sync::{mpsc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use notify_debouncer_mini::{
    new_debouncer,
    notify::{RecommendedWatcher, RecursiveMode},
    DebounceEventResult, Debouncer,
};
use serde_json::json;
use tauri::{AppHandle, Emitter, Manager};

use crate::agents::{codex::CodexProbe, AgentProbe};
use crate::hooks::model::HooksMode;
use crate::paths::home_dir;
use crate::state::{AppSettings, AppState};

/// 防抖窗口。cc-switch 的 `write_text_file` 是单次 atomic 操作,500ms 足够等其它工具的
/// 多步写完整;同时 git-ai install-hooks 自己用 toml_edit 写入可能多步,debounce 防止
/// 我们二次触发自己。
const WATCHER_DEBOUNCE_MS: u64 = 500;

/// 一轮自动修复的冷却期。git-ai install 改 config.toml、回正 clean 改 settings.json
/// 都会再次触发 watcher,冷却避免无限循环。在确定要修(锁到手、git-ai 可用)、install
/// 动手前即标记;空跑/被锁/git-ai 缺失等早退路径都不设,允许下一事件立即重试。
const REPAIR_COOLDOWN_SECS: u64 = 30;

/// 全局冷却时间戳。
/// - 用全局而非 [`WatcherHandle`] 内字段:用户来回切开关时,新 watcher 实例不会清零,
///   防止"禁用→立刻启用"绕过冷却期。
/// - `std::sync::Mutex::new` 在 Rust 1.63+ 是 const fn,可用于 static 初始化。
static LAST_REPAIR: Mutex<Option<Instant>> = Mutex::new(None);

pub struct WatcherHandle {
    /// Drop 时显式 `send(())` 通知 worker 退出。不能只靠"字段 drop 顺序":本结构有显式
    /// Drop impl,它先于字段 drop 运行,若在那里直接 join,此刻 stop 通道与 debouncer 都还
    /// 活着,worker 的 `recv_timeout` 永远超时重转 → join 永久阻塞(本次根因)。
    stop_tx: Option<mpsc::Sender<()>>,
    worker: Option<JoinHandle<()>>,
    /// 文件去抖监听器。Drop 里先于 join 释放,断开 event 通道让 worker 的 `recv_timeout`
    /// 立即返回 Disconnected,无需干等满一个超时周期。
    debouncer: Option<Debouncer<RecommendedWatcher>>,
}

impl Drop for WatcherHandle {
    /// 确定性停机,三步顺序不可乱:
    /// ① `send(())` 置停止信号 → ② drop debouncer 断开 event 通道(worker 的 recv_timeout
    /// 立即解除阻塞)→ ③ join worker。缺 ① / ② 任一,join 都可能长期阻塞。
    fn drop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        drop(self.debouncer.take());
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

/// 设置变更入口:开关翻转就启 / 停 watcher,无需重启应用。
pub fn apply_enabled(app: &AppHandle, state: &tauri::State<'_, AppState>, enabled: bool) {
    let mut guard = match state.cc_switch_watcher.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    if enabled {
        if guard.is_none() {
            match spawn_watcher(app.clone()) {
                Ok(h) => {
                    *guard = Some(h);
                    emit_event(app, "info", "cc-switch 守护已启用");
                }
                Err(e) => {
                    log::error!("cc-switch watcher 启动失败: {e}");
                    emit_event(app, "error", &format!("启动 cc-switch 守护失败: {e}"));
                }
            }
        }
    } else {
        // 先取出 handle 并释放 cc_switch_watcher 锁,再 drop handle:Drop 里要 join worker,
        // 持锁 join 会拖住其它等该锁的调用。释放锁后再 drop,join 不阻塞临界区。
        let old = guard.take();
        drop(guard);
        if old.is_some() {
            emit_event(app, "info", "cc-switch 守护已停用");
        }
        // old 在此作用域结束 drop:send stop → drop debouncer → join worker(已不持锁)。
    }
}

/// 启动时按 `AppSettings.notifications.cc_switch_auto_repair` 决定是否恢复 watcher。
/// 这是"用户在前一次会话开过就继续开"的纯被动恢复,不弹 toast 避免开机噪声。
pub fn restore_on_startup(
    app: &AppHandle,
    state: &tauri::State<'_, AppState>,
) -> crate::error::Result<()> {
    // 1. 读取有效配置，读取失败交由启动入口处理
    let s = AppSettings::load()?;
    // 2. 按用户保存的开关恢复监控
    if s.notifications.cc_switch_auto_repair {
        let mut guard = match state.cc_switch_watcher.lock() {
            Ok(g) => g,
            Err(_) => return Ok(()),
        };
        if guard.is_none() {
            match spawn_watcher(app.clone()) {
                Ok(h) => *guard = Some(h),
                Err(e) => log::error!("cc-switch watcher 启动恢复失败: {e}"),
            }
        }
    }
    Ok(())
}

fn spawn_watcher(app: AppHandle) -> Result<WatcherHandle, String> {
    let codex_toml = home_dir().join(".codex").join("config.toml");
    let claude_json = home_dir().join(".claude").join("settings.json");

    let (event_tx, event_rx) = mpsc::channel::<DebounceEventResult>();
    let (stop_tx, stop_rx) = mpsc::channel::<()>();

    let mut debouncer = new_debouncer(Duration::from_millis(WATCHER_DEBOUNCE_MS), move |res| {
        let _ = event_tx.send(res);
    })
    .map_err(|e| format!("创建 debouncer 失败: {e}"))?;

    let codex_watched = watch_path_or_parent(&mut debouncer, &codex_toml);
    let claude_watched = watch_path_or_parent(&mut debouncer, &claude_json);

    // 边缘 case:用户开了守护但还没装 Codex / Claude,父目录都不存在 → 当前实例啥也没
    // 监听。提示用户,不静默失败。下次重启 studio(或 toggle 开关)会重试。
    // TODO(P2):用更高的祖父目录 watch + recursive,以便 ~/.codex 被首次创建时自动接上。
    if !codex_watched && !claude_watched {
        emit_event(
            &app,
            "warn",
            "未检测到 ~/.codex/ 或 ~/.claude/ 目录,守护未挂载到任何文件 — 装好 Codex/Claude 后请重开守护开关",
        );
    }

    let app_for_worker = app.clone();
    let worker = thread::spawn(move || {
        run_watcher_loop(stop_rx, event_rx, move || {
            let app_clone = app_for_worker.clone();
            tauri::async_runtime::spawn(async move {
                check_and_repair(app_clone).await;
            });
        });
    });

    Ok(WatcherHandle {
        stop_tx: Some(stop_tx),
        worker: Some(worker),
        debouncer: Some(debouncer),
    })
}

/// worker 线程主循环。抽成独立函数,便于单测覆盖"停机不阻塞"而无需起 tauri / notify。
///
/// 退出条件(任一即退出):收到 stop 信号、stop 通道断开、event 通道断开(debouncer 被 drop)。
/// `try_recv` 把 `Disconnected` 也当退出;叠加 debouncer drop 触发的 `recv_timeout`
/// Disconnected,两条路都能让循环立即结束,不必干等满一个 1s 超时。
fn run_watcher_loop(
    stop_rx: mpsc::Receiver<()>,
    event_rx: mpsc::Receiver<DebounceEventResult>,
    on_event: impl Fn(),
) {
    loop {
        match stop_rx.try_recv() {
            Ok(_) | Err(mpsc::TryRecvError::Disconnected) => break,
            Err(mpsc::TryRecvError::Empty) => {}
        }
        match event_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(_) => {
                // 冷却期检查:成功修复后 30s 内的事件全部跳过
                if in_cooldown() {
                    continue;
                }
                on_event();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

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

fn in_cooldown() -> bool {
    if let Ok(g) = LAST_REPAIR.lock() {
        if let Some(t) = *g {
            return t.elapsed() < Duration::from_secs(REPAIR_COOLDOWN_SECS);
        }
    }
    false
}

fn mark_repair_done() {
    if let Ok(mut g) = LAST_REPAIR.lock() {
        *g = Some(Instant::now());
    }
}

/// install 修 Codex 时,git-ai(全 agent 操作)会顺带给 Claude settings.json 加一条
/// 官方 command hook。仅当 install 前 Claude 处于 None(从未配过 git-ai hook)时,
/// 这条 command 属未经请求的副作用,需在 install 后剥掉以保持 Claude 现状。
/// Official 是用户本就要的,不动。
fn needs_claude_command_cleanup(pre_mode: HooksMode) -> bool {
    matches!(pre_mode, HooksMode::None)
}

async fn check_and_repair(app: AppHandle) {
    let probe = CodexProbe;
    let status = probe.probe().await;

    if !status.detected || status.configured {
        return;
    }

    emit_event(&app, "warn", "检测到 Codex hook 失效,正在自动恢复…");

    let state = app.state::<AppState>();
    let acquired = match state.hooks_lock.try_write() {
        Ok(mut g) if g.is_none() => {
            *g = Some("cc-switch-auto-repair".to_string());
            true
        }
        _ => false,
    };
    if !acquired {
        emit_event(
            &app,
            "info",
            "另一个 hooks 任务正在跑,跳过本次自动修复(下次文件变化再试)",
        );
        return;
    }

    let bin = match crate::git_ai::binary::resolve() {
        Ok(p) => p,
        Err(e) => {
            release_lock(&state);
            emit_event(&app, "error", &format!("git-ai 不可用,无法自动修复: {e}"));
            return;
        }
    };

    // install 前先读 Claude 当前模式:git-ai install 是全 agent 操作,会顺带改写
    // ~/.claude/settings.json,必须在它动手前取到用户真实模式。读/解析失败时
    // detect_mode 返回 None —— 与"真无 git-ai hook"同值,本场景下仍走 clean
    // (clean 等价于 merge_to_mode(None),无 command 即空操作),宁可漏清不可误删。
    let pre_mode = crate::hooks::settings_json::detect_mode();

    // 冷却前移到 install 之前:install 自身就写 Claude settings.json、其 watcher 事件
    // 早于"修完"产生,加之 release_lock 早于本标记,中间曾是无冷却保护窗口。提到此处
    // 后"已开始一轮修复"即进入 30s 冷却,天然覆盖 install + 回正 clean 两次自触发写入。
    // 锁失败 / git-ai 缺失等早退路径已在上方 return,"空跑不设冷却"的优化不受损。
    mark_repair_done();

    let mut cmd = tokio::process::Command::new(&bin);
    cmd.arg("install");
    crate::proc::apply_no_window_tokio(&mut cmd);
    let output = cmd.output().await;
    let install_ok = matches!(&output, Ok(out) if out.status.success());

    // 根因债:git-ai install 是全 agent 操作、顺手写 Claude;此处是"事后打扫"非根治
    // (Codex 专属最小恢复需复刻上游写逻辑,超本次范围)。install 成功且用户 install
    // 前没有 git-ai command hook(None)时,剥掉 git-ai 凭空塞进 Claude
    // 的那条 command,保持 Claude 的 git-ai hook 模式与 install 前一致。
    let cleanup: Option<Result<(), String>> =
        if install_ok && needs_claude_command_cleanup(pre_mode) {
            let joined = tokio::task::spawn_blocking(|| {
                crate::hooks::settings_json::merge_to_mode(HooksMode::None, None)
            })
            .await;
            Some(match joined {
                // 不只看 MergeReport.changed:二次校验模式回到 install 前,才能抓住
                // 上游写法漂移导致 is_git_ai_owned 不识别 command 的静默失败。
                Ok(Ok(_)) => {
                    if crate::hooks::settings_json::detect_mode() == pre_mode {
                        Ok(())
                    } else {
                        Err("回正后 Claude 模式与修复前不一致".to_string())
                    }
                }
                Ok(Err(e)) => Err(e.to_string()),
                Err(e) => Err(format!("清理任务 join 失败: {e}")),
            })
        } else {
            None
        };

    release_lock(&state);
    if let Ok(mut g) = state.diag_cache.write() {
        *g = None;
    }

    if install_ok {
        match cleanup {
            Some(Ok(())) => emit_event(
                &app,
                "success",
                "Codex hook 已恢复;已清理 git-ai 顺带给 Claude 重加的 hook,Claude 配置保持不变",
            ),
            Some(Err(e)) => emit_event(
                &app,
                "error",
                &format!("Codex hook 已恢复,但 Claude 自动回正未生效({e}),请到 Hooks 页检查"),
            ),
            None => emit_event(&app, "success", "Codex hook 已自动恢复"),
        }
    } else {
        match output {
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                emit_event(
                    &app,
                    "error",
                    &format!(
                        "自动恢复失败 (exit {}): {}",
                        out.status.code().unwrap_or(-1),
                        stderr.trim()
                    ),
                );
            }
            Err(e) => {
                emit_event(&app, "error", &format!("自动恢复失败: {e}"));
            }
        }
    }
}

fn release_lock(state: &tauri::State<'_, AppState>) {
    if let Ok(mut g) = state.hooks_lock.write() {
        *g = None;
    }
}

fn emit_event(app: &AppHandle, level: &str, message: &str) {
    let _ = app.emit(
        "cc-switch-watcher://event",
        json!({ "level": level, "message": message }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// 回归:复刻 WatcherHandle::drop 的停机序列(send stop → drop event sender),
    /// run_watcher_loop 必须立即退出、join 不卡死。修复前 Drop 先 join,而 stop 通道与
    /// debouncer 都还活着 → worker 干转、join 永久阻塞,set_app_settings 长期 pending。
    #[test]
    fn watcher_loop_stops_promptly_on_drop_sequence() {
        let (event_tx, event_rx) = mpsc::channel::<DebounceEventResult>();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let worker = thread::spawn(move || run_watcher_loop(stop_rx, event_rx, || {}));

        let start = Instant::now();
        let _ = stop_tx.send(());
        drop(event_tx); // 等价 drop debouncer:断开 event 通道,recv_timeout 立即返回 Disconnected
        worker.join().expect("worker 应能 join");
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "停机序列后 worker 应立即退出(<1s),实际 {:?}",
            start.elapsed()
        );
    }

    /// 仅断开 event 通道(等价只 drop debouncer、不发 stop)也应立即退出。
    #[test]
    fn watcher_loop_stops_when_event_channel_disconnects() {
        let (event_tx, event_rx) = mpsc::channel::<DebounceEventResult>();
        let (_stop_tx, stop_rx) = mpsc::channel::<()>();
        let worker = thread::spawn(move || run_watcher_loop(stop_rx, event_rx, || {}));

        let start = Instant::now();
        drop(event_tx);
        worker.join().expect("worker 应能 join");
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "event 通道断开后 worker 应立即退出,实际 {:?}",
            start.elapsed()
        );
    }
}
