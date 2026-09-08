//! Install 模块的 Tauri 命令层:版本列表、安装/卸载、自动更新开关、当前版本探测。
//!
//! 长任务通过 Tauri event `install://<job_id>/log` 流式回传:
//!   { stream: "stdout"|"stderr"|"exit", line?: string, code?: number, ts: number }
//!
//! 互斥:通过 `AppState.install_lock` 全局锁,同一时刻只能跑一个 install / uninstall。

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};

use crate::error::AppError;
use crate::git_ai::{self};
use crate::installer::{config_file, releases, scripts};
use crate::paths::{git_ai_dir, studio_data_dir};
use crate::proc::run_streaming;
use crate::state::AppState;

#[derive(Debug, Serialize, Deserialize)]
pub struct InstalledVersion {
    pub installed: bool,
    pub version: Option<String>,
    pub binary_path: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InstallHistoryEntry {
    pub at_unix_ms: i64,
    pub action: String, // install / upgrade / uninstall
    pub version_previous: Option<String>,
    pub version_current: Option<String>,
    pub outcome: String, // success / failed
    pub exit_code: Option<i32>,
}

const INSTALL_TIMEOUT_SECS: u64 = 600; // 10 min

#[tauri::command]
pub async fn list_releases(force: bool) -> Result<releases::ReleasesPayload, String> {
    Ok(releases::list(force).await?)
}

#[tauri::command]
pub async fn get_installed_version() -> Result<InstalledVersion, String> {
    match git_ai::binary::resolve() {
        Ok(p) => {
            // 调一次 git-ai --version
            let out = crate::proc::run_capture_with_timeout(
                &p,
                &["--version"],
                None,
                Duration::from_secs(5),
            )
            .await;
            let version = match out {
                Ok(c) if c.status == 0 => {
                    extract_version(&c.stdout).or_else(|| extract_version(&c.stderr))
                }
                _ => None,
            };
            Ok(InstalledVersion {
                installed: true,
                version,
                binary_path: Some(p.display().to_string()),
            })
        }
        Err(_) => Ok(InstalledVersion {
            installed: false,
            version: None,
            binary_path: None,
        }),
    }
}

pub(crate) fn extract_version(s: &str) -> Option<String> {
    // 严格匹配 semver `X.Y.Z`(可带 `-prerelease`),失败返回 None,由调用方让前端显示"版本未知"。
    static RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"\b(\d+\.\d+\.\d+(?:-[A-Za-z0-9.-]+)?)\b").unwrap()
    });
    RE.captures(s)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

#[tauri::command]
pub async fn is_install_running(state: State<'_, AppState>) -> Result<Option<String>, String> {
    // 非阻塞读;读不到锁(几乎不可能,仅写锁瞬时持有时)返回 None。
    Ok(state.install_lock.read().ok().and_then(|g| g.clone()))
}

pub(crate) fn acquire_lock(state: &AppState, job_id: &str) -> Result<(), String> {
    // 跨锁对称:hooks 切换进行中拒绝
    if let Ok(g) = state.hooks_lock.read() {
        if g.is_some() {
            return Err("Hooks 切换正在进行,稍后再试".into());
        }
    }
    let mut g = state
        .install_lock
        .try_write()
        .map_err(|_| "另一个安装 / 卸载任务在运行,请等待完成".to_string())?;
    if g.is_some() {
        return Err("已有一个安装 / 卸载任务在运行,请等待完成".to_string());
    }
    *g = Some(job_id.to_string());
    Ok(())
}

pub(crate) fn release_lock(state: &AppState) {
    if let Ok(mut g) = state.install_lock.write() {
        *g = None;
    }
}

#[tauri::command]
pub async fn install_git_ai(
    app: AppHandle,
    state: State<'_, AppState>,
    version: Option<String>,
    job_id: String,
) -> Result<i32, String> {
    acquire_lock(&state, &job_id)?;

    let topic = format!("install://{job_id}/log");
    let prev_version = get_installed_version().await.ok().and_then(|v| v.version);
    let result = do_install(app.clone(), version.clone(), &topic).await;
    release_lock(&state);

    let new_version = match &result {
        Ok(_) => get_installed_version().await.ok().and_then(|v| v.version),
        Err(_) => None,
    };
    let is_fresh_install = prev_version.is_none();
    let entry = InstallHistoryEntry {
        at_unix_ms: now_ms(),
        action: if is_fresh_install {
            "install".into()
        } else {
            "upgrade".into()
        },
        version_previous: prev_version,
        version_current: new_version,
        outcome: if result.is_ok() {
            "success".into()
        } else {
            "failed".into()
        },
        exit_code: result.as_ref().ok().copied(),
    };
    append_history(&entry);

    // 首次安装默认禁用 git-ai 后台自更新:本项目以 GitHub Releases 为唯一发布渠道、
    // 由 Studio 统一管理 git-ai 升级(对齐 PR-FAQ "no auto-update ping"),git-ai 自更新会绕过 Studio。
    // 只在首装写默认,升级不动以保留用户后续显式设置;写失败仅告警,不阻断安装。
    if is_fresh_install && result.is_ok() {
        if let Err(e) = config_file::write(&config_file::GitAiConfigPatch {
            disable_auto_updates: Some(true),
            update_channel: Some("none".into()),
        }) {
            log::warn!("首装默认禁用 git-ai 自更新失败(不阻断安装): {e}");
        }
    }

    // 失效路径缓存,让下一次 resolve 重新探测
    git_ai::binary::invalidate_cache();
    if let Ok(mut g) = state.diag_cache.write() {
        *g = None;
    }

    result.map_err(|e| e.to_string())
}

async fn do_install(
    app: AppHandle,
    version: Option<String>,
    topic: &str,
) -> crate::error::Result<i32> {
    let script = scripts::download_install_script().await?;
    let (prog, args, env) = scripts::build_install_invocation(&script, version.as_deref());
    let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
    let code = run_streaming(
        &app,
        &prog,
        &args_ref,
        None,
        &env,
        topic,
        Duration::from_secs(INSTALL_TIMEOUT_SECS),
    )
    .await?;
    if code != 0 {
        return Err(AppError::Other(format!("安装脚本退出码非 0: {code}")));
    }
    Ok(code)
}

/// 先由官方命令移除 hooks,成功后删除安装目录;所有失败路径均释放安装锁。
#[tauri::command]
pub async fn uninstall_git_ai(
    app: AppHandle,
    state: State<'_, AppState>,
    job_id: String,
    confirm_token: String,
) -> Result<(), String> {
    // 1. 校验确认口令并占用安装任务锁。
    if confirm_token != "uninstall" {
        return Err("二次确认 token 错误".into());
    }
    acquire_lock(&state, &job_id)?;

    let topic = format!("install://{job_id}/log");
    let previous = get_installed_version().await.ok().and_then(|v| v.version);

    // 2. 完成清理后统一释放锁,解析命令或执行失败都不会提前跳过释放。
    let result = async {
        let bin = git_ai::binary::resolve()?;
        do_uninstall(&bin, &git_ai_dir(), |stream, line| {
            let _ = app_log(&app, &topic, stream, line);
        })
        .await
    }
    .await;
    release_lock(&state);

    // 3. 回传结果并失效环境缓存,记录实际卸载结果。
    if let Err(error) = &result {
        let _ = app_log(&app, &topic, "stderr", &error.to_string());
    }
    let _ = app_log(
        &app,
        &topic,
        "exit",
        match &result {
            Ok(_) => "卸载完成。git notes 与 .git/ai/working_logs/ 未动。",
            Err(_) => "卸载失败,请查看日志手动清理残留",
        },
    );

    git_ai::binary::invalidate_cache();
    if let Ok(mut g) = state.diag_cache.write() {
        *g = None;
    }

    let outcome = if result.is_ok() { "success" } else { "failed" };
    append_history(&InstallHistoryEntry {
        at_unix_ms: now_ms(),
        action: "uninstall".into(),
        version_previous: previous,
        version_current: None,
        outcome: outcome.into(),
        exit_code: Some(if result.is_ok() { 0 } else { 1 }),
    });

    result.map_err(String::from)
}

/// 使用指定的 git-ai 清理 hooks,只有官方命令成功时才删除对应安装目录。
async fn do_uninstall(
    bin: &Path,
    dir: &Path,
    mut emit_log: impl FnMut(&str, &str),
) -> crate::error::Result<()> {
    // 1. 执行上游真实卸载命令并保留诊断输出。
    let output = crate::proc::run_capture_with_timeout(
        bin,
        &["uninstall-hooks", "--dry-run=false"],
        None,
        Duration::from_secs(30),
    )
    .await?;
    for (stream, content) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
        for line in content.lines() {
            emit_log(stream, line);
        }
    }
    if output.status != 0 {
        return Err(AppError::GitAiFailed {
            code: output.status,
            stderr: output.stderr,
        });
    }

    // 1.1 上游各 agent 的清理失败仍可能退出 0,按其明确诊断阻止删除。
    // git-ai/src/commands/install_hooks.rs:976-1005 的错误输出没有结构化 stdout。
    let partial_failure = output.stderr.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with("Error:")
            || line.starts_with("Error checking ")
            || line.starts_with("Error uninstalling extras ")
    });
    if partial_failure {
        return Err(AppError::Other(format!(
            "git-ai uninstall-hooks 未完成,已保留安装目录: {}",
            output.stderr.trim()
        )));
    }

    // 2. 删除安装目录,保留各仓库的 git notes 和 .git/ai 数据。
    if dir.is_dir() {
        emit_log("stdout", &format!("正在删除目录: {}", dir.display()));
        let dir_owned = dir.to_path_buf();
        tokio::task::spawn_blocking(move || std::fs::remove_dir_all(dir_owned))
            .await
            .map_err(|e| AppError::Other(format!("spawn 失败: {e}")))?
            .map_err(|e| AppError::Other(format!("删除 {} 失败: {e}", dir.display())))?;
    } else {
        emit_log("stdout", "~/.git-ai/ 不存在,跳过");
    }
    Ok(())
}

#[tauri::command]
pub async fn get_git_ai_config() -> Result<config_file::GitAiConfig, String> {
    Ok(config_file::read()?)
}

#[tauri::command]
pub async fn set_git_ai_config(
    patch: config_file::GitAiConfigPatch,
) -> Result<config_file::GitAiConfig, String> {
    Ok(config_file::write(&patch)?)
}

#[tauri::command]
pub async fn set_auto_update(enabled: bool) -> Result<config_file::GitAiConfig, String> {
    // 仅写 disable_auto_updates;不主动覆盖 update_channel(否则会破坏用户原值)。
    // 禁用更新时把 channel 设 "none" 是 git-ai 官方文档的建议,但只在禁用路径写。
    let patch = if enabled {
        config_file::GitAiConfigPatch {
            disable_auto_updates: Some(false),
            update_channel: None,
        }
    } else {
        config_file::GitAiConfigPatch {
            disable_auto_updates: Some(true),
            update_channel: Some("none".into()),
        }
    };
    Ok(config_file::write(&patch)?)
}

#[tauri::command]
pub async fn install_history() -> Result<Vec<InstallHistoryEntry>, String> {
    let p = studio_data_dir().join("install-history.json");
    if !p.exists() {
        return Ok(vec![]);
    }
    let raw = std::fs::read_to_string(&p).map_err(|e| e.to_string())?;
    Ok(serde_json::from_str::<Vec<InstallHistoryEntry>>(&raw).unwrap_or_default())
}

fn append_history(entry: &InstallHistoryEntry) {
    let p = studio_data_dir().join("install-history.json");
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut list: Vec<InstallHistoryEntry> = std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    list.push(InstallHistoryEntry {
        at_unix_ms: entry.at_unix_ms,
        action: entry.action.clone(),
        version_previous: entry.version_previous.clone(),
        version_current: entry.version_current.clone(),
        outcome: entry.outcome.clone(),
        exit_code: entry.exit_code,
    });
    // 限制 200 条
    if list.len() > 200 {
        let extra = list.len() - 200;
        list.drain(0..extra);
    }
    let _ = std::fs::write(&p, serde_json::to_string_pretty(&list).unwrap_or_default());
}

fn app_log(app: &AppHandle, topic: &str, stream: &str, line: &str) -> crate::error::Result<()> {
    use tauri::Emitter;
    app.emit(
        topic,
        serde_json::json!({"stream": stream, "line": line, "ts": now_ms()}),
    )
    .map_err(|e| AppError::Other(format!("emit failed: {e}")))
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// 在临时目录创建只接受真实卸载参数的 CLI 替身。
    fn mock_uninstaller(root: &Path, exit_code: i32, stderr: Option<&str>) -> PathBuf {
        // 1. 用本机脚本模拟官方命令的输出和退出状态。
        #[cfg(windows)]
        let (name, script) = (
            "git-ai stub.cmd",
            format!(
                "@echo off\r\nif not \"%~1\"==\"uninstall-hooks\" exit /b 91\r\nif not \"%~2\"==\"--dry-run=false\" exit /b 92\r\necho cleanup output\r\n{}exit /b {exit_code}\r\n",
                stderr.map(|line| format!("echo {line} 1>&2\r\n")).unwrap_or_default()
            ),
        );
        #[cfg(not(windows))]
        let (name, script) = (
            "git-ai stub.sh",
            format!(
                "#!/bin/sh\n[ \"$1\" = uninstall-hooks ] || exit 91\n[ \"$2\" = --dry-run=false ] || exit 92\necho 'cleanup output'\n{}exit {exit_code}\n",
                stderr.map(|line| format!("echo '{line}' >&2\n")).unwrap_or_default()
            ),
        );
        let path = root.join(name);
        fs::write(&path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        path
    }

    /// 仅成功的真实卸载参数组合可以进入安装目录删除步骤。
    #[tokio::test]
    async fn uninstall_removes_directory_after_hook_cleanup() {
        // 1. 准备独立安装目录并执行替身 CLI。
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".git-ai");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("keep.txt"), "installed").unwrap();
        let bin = mock_uninstaller(root.path(), 0, None);
        let mut logs = Vec::new();
        do_uninstall(&bin, &dir, |stream, line| {
            logs.push((stream.to_string(), line.to_string()));
        })
        .await
        .unwrap();

        // 2. 核对执行结果与原始输出均被保留。
        assert!(!dir.exists());
        assert!(logs.contains(&("stdout".into(), "cleanup output".into())));
    }

    /// CLI 返回非零状态时保留安装文件,便于修复 hooks 后重新卸载。
    #[tokio::test]
    async fn uninstall_preserves_directory_on_nonzero_exit() {
        // 1. 模拟官方命令执行失败。
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".git-ai");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("keep.txt"), "installed").unwrap();
        let bin = mock_uninstaller(root.path(), 7, Some("cleanup failed"));
        let error = do_uninstall(&bin, &dir, |_, _| {}).await.unwrap_err();

        // 2. 原始退出码和错误消息可见,安装文件未删除。
        assert!(matches!(error, AppError::GitAiFailed { code: 7, .. }));
        assert!(error.to_string().contains("cleanup failed"));
        assert_eq!(
            fs::read_to_string(dir.join("keep.txt")).unwrap(),
            "installed"
        );
    }

    /// 上游明确报告部分失败时,即使退出 0 也不能删除安装目录。
    #[tokio::test]
    async fn uninstall_preserves_directory_on_reported_partial_failure() {
        // 1. 覆盖上游三处 stderr 诊断契约。
        for diagnostic in [
            "  Error: permission denied",
            "  Error checking Claude Code: permission denied",
            "  Error uninstalling extras for Claude Code: permission denied",
        ] {
            let root = tempfile::tempdir().unwrap();
            let dir = root.path().join(".git-ai");
            fs::create_dir(&dir).unwrap();
            fs::write(dir.join("keep.txt"), "installed").unwrap();
            let bin = mock_uninstaller(root.path(), 0, Some(diagnostic));
            let mut stderr = Vec::new();
            let error = do_uninstall(&bin, &dir, |stream, line| {
                if stream == "stderr" {
                    stderr.push(line.to_string());
                }
            })
            .await
            .unwrap_err();

            // 1.1 每一种失败都保留安装文件与完整诊断。
            assert!(error.to_string().contains(diagnostic.trim()));
            assert!(stderr.iter().any(|line| line.contains(diagnostic)));
            assert_eq!(
                fs::read_to_string(dir.join("keep.txt")).unwrap(),
                "installed"
            );
        }
    }

    /// 无法启动清理命令时保留安装目录,不能跳过 hooks 清理。
    #[tokio::test]
    async fn uninstall_preserves_directory_when_cli_cannot_start() {
        // 1. 指向确定不存在的临时命令路径。
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".git-ai");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("keep.txt"), "installed").unwrap();
        let result = do_uninstall(&root.path().join("missing-cli"), &dir, |_, _| {}).await;

        // 2. 执行失败不会触碰安装文件。
        assert!(result.is_err());
        assert_eq!(
            fs::read_to_string(dir.join("keep.txt")).unwrap(),
            "installed"
        );
    }
}
