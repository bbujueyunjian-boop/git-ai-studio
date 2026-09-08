//! git-ai daemon 运行态探测；未占用的残留 lock 属于正常空闲状态。
//!
//! 上游 `git-ai/src/utils.rs::LockFile` 释放 OS 锁时保留文件。
//! PID 元信息与同名进程都不足以证明持锁者身份，未知持锁者必须由用户进一步排查。

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::paths;

/// daemon 诊断结果；异常仅表示锁不可用且未找到存活的记录 PID。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DaemonHealth {
    /// 无 lock 或残留 lock 未被占用；客户端命令可按上游流程启动 daemon。
    Idle,
    /// daemon 正常运行(lock 在 + PID 存活)。
    Running { pid: u32 },
    /// lock 无法独占打开，但记录 PID 不可用；持锁者身份尚未确认。
    BlockedLockUnknownPid {
        lock_path: String,
        pid_meta_path: String,
        last_pid: Option<u32>,
    },
}

/// 复查结果；未知持锁者返回错误，正常状态不会修改文件或进程。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonRepairResult {
    pub before: DaemonHealth,
    pub after: DaemonHealth,
}

/// 探测 daemon 运行态；PID 初次失活后重读，避开 daemon 重启窗口。
pub async fn detect_daemon_health() -> DaemonHealth {
    // 1. 从上游运行目录读取锁与 PID 元信息。
    detect_daemon_health_at(
        &paths::git_ai_daemon_lock_path(),
        &paths::git_ai_daemon_pid_meta_path(),
    )
    .await
}

/// 对指定运行目录进行只读探测，供真实诊断与隔离文件测试共用。
async fn detect_daemon_health_at(lock_path: &Path, pid_path: &Path) -> DaemonHealth {
    // 1. 缺少锁文件或锁可用均属于正常空闲；残留文件不需要清理。
    if !lock_path.exists() {
        return DaemonHealth::Idle;
    }
    #[cfg(target_os = "windows")]
    if !daemon_lock_is_held(lock_path) {
        return DaemonHealth::Idle;
    }

    // 2. 重读 PID 元信息，给正在启动的 daemon 留出写入时间。
    let first_pid = read_pid_from_meta(pid_path);
    if let Some(pid) = first_pid {
        if process_alive(pid).await {
            return DaemonHealth::Running { pid };
        }
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let second_pid = read_pid_from_meta(pid_path);
    if let Some(pid) = second_pid {
        if process_alive(pid).await {
            return DaemonHealth::Running { pid };
        }
    }

    // 3. 只有实际无法取得锁时才报告异常，不按进程名猜测持锁者。
    if daemon_lock_is_held(lock_path) {
        DaemonHealth::BlockedLockUnknownPid {
            lock_path: lock_path.display().to_string(),
            pid_meta_path: pid_path.display().to_string(),
            last_pid: second_pid.or(first_pid),
        }
    } else {
        DaemonHealth::Idle
    }
}

/// 复查 daemon 是否已经恢复；无法确认持锁者身份时停止处理并给出排查建议。
pub async fn repair_daemon_lock() -> Result<DaemonRepairResult, String> {
    // 1. 使用最新诊断结果，避免依据先前诊断中的旧 PID 作出判断。
    repair_after_probe(detect_daemon_health().await)
}

/// 只接受正常状态作为已恢复结果，未知持锁者不得触发强杀或删除运行文件。
fn repair_after_probe(before: DaemonHealth) -> Result<DaemonRepairResult, String> {
    // 1. 拒绝无法证明目标身份的修复，保留现场供用户排查。
    if matches!(&before, DaemonHealth::BlockedLockUnknownPid { .. }) {
        return Err("daemon lock 仍不可用，无法确认持锁进程身份。请先尝试官方命令 git-ai daemon shutdown；若失败，请核对实际持锁进程。Studio 未结束任何进程，也未删除运行文件。".into());
    }
    // 2. 正常空闲或运行状态无需修改文件和进程。
    Ok(DaemonRepairResult {
        before: before.clone(),
        after: before,
    })
}

/// 尝试独占打开既有锁文件；不创建、不截断上游运行文件。
#[cfg(target_os = "windows")]
fn daemon_lock_is_held(path: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;

    // 1. 对齐上游 Windows 独占句柄规则；文件已消失时按空闲处理。
    match std::fs::OpenOptions::new()
        .write(true)
        .share_mode(0)
        .open(path)
    {
        Ok(_) => false,
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    }
}

/// Unix 的现有探测仅依赖 PID；文件是否存在不能证明 advisory lock 被占用。
#[cfg(not(target_os = "windows"))]
fn daemon_lock_is_held(_path: &Path) -> bool {
    // 1. 不把普通残留文件判作持锁。
    false
}

/// 从上游元信息读取有效 PID；缺失或损坏由诊断结果表示。
fn read_pid_from_meta(path: &Path) -> Option<u32> {
    // 1. PID 必须可表示为平台 PID，禁止截断成另一个进程号。
    let raw = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    value
        .get("pid")?
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid > 0)
}

#[cfg(target_os = "windows")]
async fn process_alive(pid: u32) -> bool {
    let mut cmd = Command::new("tasklist");
    cmd.args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"]);
    crate::proc::apply_no_window_tokio(&mut cmd);
    let out = cmd.output().await;
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            // CSV 格式存活行形如 `"foo.exe","12345","Console","1","..."`;PID 不存在时
            // tasklist 输出 `INFO: No tasks ...` 到 stdout,不会包含被引号包裹的 PID。
            s.contains(&format!("\"{pid}\""))
        }
        Err(_) => false,
    }
}

#[cfg(not(target_os = "windows"))]
async fn process_alive(pid: u32) -> bool {
    let status = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .await;
    status.map(|s| s.success()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 上游正常退出保留的未占用锁不能触发修复或被删除。
    #[tokio::test]
    async fn unheld_lock_is_idle_and_preserved() {
        // 1. 模拟上游退出后的残留文件，按真实探测入口复查。
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("daemon.lock");
        let pid_path = dir.path().join("daemon.pid.json");
        std::fs::write(&lock_path, b"retained lock").unwrap();
        let health = detect_daemon_health_at(&lock_path, &pid_path).await;
        assert!(matches!(health, DaemonHealth::Idle));
        let result = repair_after_probe(health).unwrap();
        assert!(matches!(result.after, DaemonHealth::Idle));
        assert_eq!(std::fs::read(&lock_path).unwrap(), b"retained lock");
        assert!(!pid_path.exists());
    }

    /// 读取不存在的运行目录不能创建锁文件。
    #[tokio::test]
    async fn absent_lock_is_idle_without_creating_files() {
        // 1. 探测空目录并确认无运行文件副作用。
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("daemon.lock");
        let pid_path = dir.path().join("daemon.pid.json");
        assert!(matches!(
            detect_daemon_health_at(&lock_path, &pid_path).await,
            DaemonHealth::Idle
        ));
        assert!(!lock_path.exists());
    }

    /// 未知 Windows 持锁者不能触发强杀或删除，释放后自然恢复空闲。
    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn unknown_lock_holder_is_preserved() {
        use std::os::windows::fs::OpenOptionsExt;

        // 1. 用本测试持有真实独占锁，不启动或结束用户进程。
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("daemon.lock");
        let pid_path = dir.path().join("daemon.pid.json");
        std::fs::write(&lock_path, b"held lock").unwrap();
        let holder = std::fs::OpenOptions::new()
            .write(true)
            .share_mode(0)
            .open(&lock_path)
            .unwrap();
        let health = detect_daemon_health_at(&lock_path, &pid_path).await;
        assert!(matches!(health, DaemonHealth::BlockedLockUnknownPid { .. }));
        let error = repair_after_probe(health).unwrap_err();
        assert!(error.contains("无法确认持锁进程身份"));
        assert!(daemon_lock_is_held(&lock_path));

        // 2. 正常释放锁后文件内容保持不变，诊断自然恢复。
        drop(holder);
        assert_eq!(std::fs::read(&lock_path).unwrap(), b"held lock");
        assert!(matches!(
            detect_daemon_health_at(&lock_path, &pid_path).await,
            DaemonHealth::Idle
        ));
        assert!(!pid_path.exists());
    }
    /// 未占用的锁即使残留 PID 已被复用，也不能被误判为运行中的 daemon。
    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn unheld_lock_with_reused_pid_is_idle() {
        // 1. 使用本测试的存活 PID 模拟元信息过期，不操作该进程。
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("daemon.lock");
        let pid_path = dir.path().join("daemon.pid.json");
        std::fs::write(&lock_path, b"retained lock").unwrap();
        std::fs::write(&pid_path, format!(r#"{{"pid":{}}}"#, std::process::id())).unwrap();
        assert!(matches!(
            detect_daemon_health_at(&lock_path, &pid_path).await,
            DaemonHealth::Idle
        ));
        assert!(lock_path.exists());
        assert!(pid_path.exists());
    }
}
