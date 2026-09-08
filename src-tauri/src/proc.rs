//! 跨平台子进程封装:统一超时、Windows 隐藏窗口、UTF-8 输出收集、流式回调。

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::error::{AppError, Result};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// 给 **std** `Command` 打 Windows `CREATE_NO_WINDOW` flag,避免 release 下子进程
/// 弹一闪而过的黑色控制台。tokio::process::Command 不需要 trait 直接有 `creation_flags`;
/// 但 std 版本必须经 `CommandExt` 引入。这里集中暴露给非 proc.rs 的 spawn 点
/// (`repo/head.rs` git、`hooks/server/status.rs` schtasks、`commands/*` explorer 等)。
#[cfg(windows)]
pub fn apply_no_window_std(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
pub fn apply_no_window_std(_cmd: &mut std::process::Command) {}

/// `apply_no_window_std` 的 tokio 版:`tokio::process::Command` 在 Windows 上直接有
/// `creation_flags`(无需 `CommandExt` trait),但仍需显式打 `CREATE_NO_WINDOW`,否则
/// release 下子进程会弹一闪而过的黑色控制台。集中暴露给不走 `proc.rs` 通道、直接
/// 用 tokio 起进程的 spawn 点(`git_ai/daemon.rs` tasklist、`cc_switch_watcher.rs`
/// git-ai install 等)。
#[cfg(windows)]
pub fn apply_no_window_tokio(cmd: &mut tokio::process::Command) {
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
pub fn apply_no_window_tokio(_cmd: &mut tokio::process::Command) {}

pub struct CaptureOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

/// 一次性命令(短时间任务,15s 内完成),返回完整 stdout/stderr。
pub async fn run_capture(
    program: &Path,
    args: &[&str],
    cwd: Option<&Path>,
) -> Result<CaptureOutput> {
    run_capture_with_timeout(program, args, cwd, Duration::from_secs(15)).await
}

pub async fn run_capture_with_timeout(
    program: &Path,
    args: &[&str],
    cwd: Option<&Path>,
    timeout: Duration,
) -> Result<CaptureOutput> {
    run_capture_internal(program, args, cwd, None, &[], timeout).await
}

/// 同 run_capture_with_timeout,但额外向子进程注入 env(键值覆盖继承环境)。用于把
/// `env_path` 的真实 PATH 镜像传给探测子进程(如 `claude --version` 的
/// `#!/usr/bin/env node` shebang 需要 node 在 PATH),不依赖全局 set_var。
pub async fn run_capture_with_env_timeout(
    program: &Path,
    args: &[&str],
    cwd: Option<&Path>,
    env: &[(String, String)],
    timeout: Duration,
) -> Result<CaptureOutput> {
    run_capture_internal(program, args, cwd, None, env, timeout).await
}

/// 同 run_capture_with_timeout,但允许向子进程 stdin 一次性写入 `stdin_input` 然后关闭。
/// 用于 `git cat-file --batch-check` 这类"逐行 stdin"批查询接口。
pub async fn run_capture_with_stdin(
    program: &Path,
    args: &[&str],
    cwd: Option<&Path>,
    stdin_input: &str,
    timeout: Duration,
) -> Result<CaptureOutput> {
    run_capture_internal(program, args, cwd, Some(stdin_input), &[], timeout).await
}

/// 并发处理标准输入输出；超时或 I/O 失败时终止并回收本次子进程。
async fn run_capture_internal(
    program: &Path,
    args: &[&str],
    cwd: Option<&Path>,
    stdin_input: Option<&str>,
    env: &[(String, String)],
    timeout: Duration,
) -> Result<CaptureOutput> {
    // 1. 配置独立子进程及标准流。
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .stdin(if stdin_input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    // git/git-ai stderr 英文化(评审 P6 #33 全局 hardening):中文 locale 下
    // git 错误信息会本地化(如"致命错误"代替"fatal: ..."),让我们的 stderr 关键词
    // 匹配(`is_missing_notes_ref` / `is_empty_repo_stderr` 等)漂移。统一 LC_ALL=C。
    // 注意:不影响 commit subject / file content 等用户数据(它们走 stdout,且 git 不本地化数据)。
    cmd.env("LC_ALL", "C").env("LANG", "C");
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let mut child = cmd.spawn().map_err(AppError::Io)?;
    let mut stdin = child.stdin.take();
    let mut stdout = child.stdout.take().expect("stdout 已配置为管道");
    let mut stderr = child.stderr.take().expect("stderr 已配置为管道");
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();

    // 2. 同时写入输入、读取输出和等待退出，超时覆盖整个通信过程。
    let communication = async {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let write_input = async {
            if let Some(input) = stdin_input {
                let mut pipe = stdin.take().expect("有输入时 stdin 已配置为管道");
                pipe.write_all(input.as_bytes()).await?;
                pipe.shutdown().await?;
            }
            Ok::<_, std::io::Error>(())
        };
        let (_, _, _, status) = tokio::try_join!(
            write_input,
            stdout.read_to_end(&mut stdout_bytes),
            stderr.read_to_end(&mut stderr_bytes),
            child.wait(),
        )?;
        Ok::<_, std::io::Error>(status)
    };
    let status = match tokio::time::timeout(timeout, communication).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            // 3. 通信失败后也回收进程；调用方取消任务时由 kill_on_drop 终止。
            child.kill().await.map_err(AppError::Io)?;
            return Err(AppError::Io(error));
        }
        Err(_) => {
            child.kill().await.map_err(AppError::Io)?;
            return Err(AppError::Other(format!(
                "command timed out after {}s: {}",
                timeout.as_secs(),
                program.display()
            )));
        }
    };
    Ok(CaptureOutput {
        status: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
    })
}

/// 长时任务:以 Tauri event 流式回传 stdout/stderr,每行一次 emit。
/// `event_topic` 形如 `"install://<job_id>/log"`,前端订阅。
/// payload:`{ "stream": "stdout"|"stderr"|"exit", "line"?: string, "code"?: number, "ts": number }`
pub async fn run_streaming(
    app: &AppHandle,
    program: &Path,
    args: &[&str],
    cwd: Option<&Path>,
    env: &[(String, String)],
    event_topic: &str,
    timeout: Duration,
) -> Result<i32> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let mut child = cmd.spawn().map_err(AppError::Io)?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::Other("无法获取子进程 stdout".to_string()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AppError::Other("无法获取子进程 stderr".to_string()))?;

    let app_out = app.clone();
    let topic_out = event_topic.to_string();
    let h_out = tokio::spawn(async move {
        let mut r = BufReader::new(stdout).lines();
        while let Ok(Some(l)) = r.next_line().await {
            let _ = app_out.emit(
                &topic_out,
                serde_json::json!({"stream":"stdout","line":l,"ts": now_ms()}),
            );
        }
    });
    let app_err = app.clone();
    let topic_err = event_topic.to_string();
    let h_err = tokio::spawn(async move {
        let mut r = BufReader::new(stderr).lines();
        while let Ok(Some(l)) = r.next_line().await {
            let _ = app_err.emit(
                &topic_err,
                serde_json::json!({"stream":"stderr","line":l,"ts": now_ms()}),
            );
        }
    });

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(s) => s.map_err(AppError::Io)?,
        Err(_) => {
            let _ = child.start_kill();
            let _ = app.emit(
                event_topic,
                serde_json::json!({"stream":"exit","code":-9,"timeout":true,"ts":now_ms()}),
            );
            return Err(AppError::Other(format!(
                "命令执行超时({}s)",
                timeout.as_secs()
            )));
        }
    };
    let _ = h_out.await;
    let _ = h_err.await;

    let code = status.code().unwrap_or(-1);
    let _ = app.emit(
        event_topic,
        serde_json::json!({"stream":"exit","code":code,"ts":now_ms()}),
    );
    Ok(code)
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

    /// 作为独立子进程执行，验证管道阻塞、正常输出与终止后的文件副作用。
    #[test]
    #[ignore = "仅由本模块通过独立子进程调用"]
    fn capture_child_fixture() {
        // 1. 根据父进程传入的场景产生真实 I/O 或延迟副作用。
        use std::io::{Read, Write};
        let mode = std::env::var("STUDIO_CAPTURE_TEST_MODE").unwrap();
        if mode == "roundtrip" {
            std::io::stdout()
                .write_all(&vec![b'o'; 1024 * 1024])
                .unwrap();
            std::io::stderr().write_all(b"stderr-marker").unwrap();
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input).unwrap();
            assert_eq!(input.len(), 1024 * 1024);
            println!("INPUT_RECEIVED");
        } else {
            let dir =
                std::path::PathBuf::from(std::env::var_os("STUDIO_CAPTURE_TEST_DIR").unwrap());
            std::fs::write(dir.join("started"), std::process::id().to_string()).unwrap();
            std::thread::sleep(Duration::from_secs(2));
            std::fs::write(dir.join("finished"), b"unexpected late side effect").unwrap();
        }
    }

    /// 生成只传给测试子进程的环境，避免改写测试宿主的全局环境。
    fn fixture_env(mode: &str, dir: &Path) -> Vec<(String, String)> {
        // 1. 使用独立临时目录记录子进程是否实际启动与完成。
        vec![
            ("STUDIO_CAPTURE_TEST_MODE".into(), mode.into()),
            (
                "STUDIO_CAPTURE_TEST_DIR".into(),
                dir.to_string_lossy().into_owned(),
            ),
        ]
    }

    const FIXTURE_ARGS: &[&str] = &[
        "--exact",
        "proc::tests::capture_child_fixture",
        "--ignored",
        "--nocapture",
    ];

    /// 超时返回后，已启动的子进程不得继续执行延迟写盘。
    #[tokio::test]
    async fn capture_timeout_terminates_child() {
        // 1. 启动真实子进程，并在其延迟写盘之前触发超时。
        let dir = tempfile::tempdir().unwrap();
        let result = run_capture_internal(
            &std::env::current_exe().unwrap(),
            FIXTURE_ARGS,
            None,
            None,
            &fixture_env("slow", dir.path()),
            Duration::from_secs(1),
        )
        .await;
        assert!(matches!(result, Err(AppError::Other(message)) if message.contains("timed out")));
        assert!(dir.path().join("started").exists());
        // 2. 等过原子进程完成时间，验证已无后续副作用。
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(!dir.path().join("finished").exists());
    }

    /// 子进程不读 stdin 时，写入也必须受同一个超时约束。
    #[tokio::test]
    async fn capture_timeout_includes_blocked_stdin() {
        // 1. 写入超过管道容量的数据，验证不会在开始计时前永久阻塞。
        let dir = tempfile::tempdir().unwrap();
        let input = "x".repeat(1024 * 1024);
        let result = run_capture_internal(
            &std::env::current_exe().unwrap(),
            FIXTURE_ARGS,
            None,
            Some(&input),
            &fixture_env("slow", dir.path()),
            Duration::from_secs(1),
        )
        .await;
        assert!(matches!(result, Err(AppError::Other(message)) if message.contains("timed out")));
        assert!(dir.path().join("started").exists());
        // 2. 超时后的子进程不得继续写盘。
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(!dir.path().join("finished").exists());
    }

    /// 同时填满输入与输出管道仍可完成，保证新增超时处理不引入双向死锁。
    #[tokio::test]
    async fn capture_drains_output_while_writing_input() {
        // 1. 子进程先输出大块内容再读取输入，父进程必须并发收发。
        let dir = tempfile::tempdir().unwrap();
        let input = "x".repeat(1024 * 1024);
        let result = run_capture_internal(
            &std::env::current_exe().unwrap(),
            FIXTURE_ARGS,
            None,
            Some(&input),
            &fixture_env("roundtrip", dir.path()),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(result.status, 0);
        assert!(result.stdout.contains(&"o".repeat(1024 * 1024)));
        assert!(result.stdout.contains("INPUT_RECEIVED"));
        assert!(result.stderr.contains("stderr-marker"));
    }

    /// 调用方取消命令任务时，同样终止已经启动的子进程。
    #[tokio::test]
    async fn cancelling_capture_terminates_child() {
        // 1. 等待子进程实际启动，再取消父任务。
        let dir = tempfile::tempdir().unwrap();
        let env = fixture_env("slow", dir.path());
        let task = tokio::spawn(async move {
            run_capture_with_env_timeout(
                &std::env::current_exe().unwrap(),
                FIXTURE_ARGS,
                None,
                &env,
                Duration::from_secs(10),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !dir.path().join("started").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(matches!(task.await, Err(error) if error.is_cancelled()));
        // 2. 取消后不再产生延迟副作用。
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(!dir.path().join("finished").exists());
    }
}
