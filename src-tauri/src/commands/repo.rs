use tauri::State;

use crate::proc::apply_no_window_std;
use crate::repo;
use crate::state::{AppSettings, AppState, RepoEntry};

/// 路径规范化:统一 Windows 正反斜杠、解析符号链接、去掉尾随分隔符。
fn normalize(input: &str) -> std::path::PathBuf {
    let p = std::path::PathBuf::from(input);
    dunce::canonicalize(&p).unwrap_or(p)
}

#[tauri::command]
pub async fn discover_repos(
    roots: Vec<String>,
    max_depth: Option<u32>,
) -> Result<Vec<RepoEntry>, String> {
    let normalized: Vec<String> = roots
        .into_iter()
        .map(|r| normalize(&r).display().to_string())
        .collect();
    let entries =
        tokio::task::spawn_blocking(move || repo::discover::scan_roots(&normalized, max_depth))
            .await
            .map_err(|e| format!("scan failed: {e}"))?;
    Ok(entries)
}

/// 验证并保存所选仓库；配置损坏或保存失败时不切换当前仓库。
#[tauri::command]
pub async fn select_repo(
    app: tauri::AppHandle,
    path: String,
    state: State<'_, AppState>,
) -> Result<RepoEntry, String> {
    // 1. 读取有效配置并验证所选仓库
    let mut settings = AppSettings::load()?;
    let normalized = normalize(&path);
    if !normalized.is_dir() {
        return Err(format!("目录不存在: {path}"));
    }
    let normalized_str = normalized.display().to_string();
    let scanned = repo::discover::scan_roots(std::slice::from_ref(&normalized_str), Some(1));
    let mut entry = scanned
        .into_iter()
        .find(|e| {
            // 两边都已 canonicalize,直接比较;退化情况下大小写不敏感
            e.path.eq_ignore_ascii_case(&normalized_str)
                || normalize(&e.path).display().to_string() == normalized_str
        })
        .ok_or_else(|| format!("不是一个 git 仓库(未发现 .git): {path}"))?;

    // 2. 选中后立刻同步查一次 dirty,失败保持 None
    entry.dirty = tokio::task::spawn_blocking({
        let p = std::path::PathBuf::from(&entry.path);
        move || repo::head::detect_dirty(&p)
    })
    .await
    .ok()
    .flatten();

    // 3. 先保存仓库记录；失败时当前仓库仍保持原选择
    settings.last_repo = Some(entry.path.clone());
    settings.recent_repos.retain(|p| p != &entry.path);
    settings.recent_repos.insert(0, entry.path.clone());
    settings.recent_repos.truncate(5);
    settings.save().map_err(|e| format!("写配置失败: {e}"))?;

    // 4. 切换当前仓库并失效依赖缓存
    if let Ok(mut g) = state.current_repo.write() {
        *g = Some(entry.clone());
    }
    if let Ok(mut g) = state.diag_cache.write() {
        *g = None;
    }
    if let Ok(mut g) = state.commits_cache.write() {
        *g = None;
    }
    // 5. 切仓后重启 refs/notes/ai watcher 指向新仓库(若用户开了实时低 AI 提醒)。
    // realtime_enabled 默认 true,需要同时开了低 AI 总开关才会真正启动。
    let realtime_active = settings.notifications.low_ai_share.enabled
        && settings
            .notifications
            .low_ai_share
            .realtime_enabled
            .unwrap_or(true);
    crate::repo_notes_watcher::apply_state(&app, &state, Some(&entry.path), realtime_active);

    Ok(entry)
}

#[tauri::command]
pub async fn current_repo(state: State<'_, AppState>) -> Result<Option<RepoEntry>, String> {
    Ok(state.current_repo.read().ok().and_then(|g| g.clone()))
}

/// 读取指定仓库**生效的** git 用户邮箱(`git config --get user.email`,在仓库目录内执行,
/// 故 local 缺失时自动继承 global),trim + 小写归一。
///
/// 失败 / 未配置 → `None`:调用方据此降级(单仓直接当无身份;聚合的「只看我」口径把这类仓
/// 显式排除为 failed_repo,绝不静默当全作者并入)。供 `current_git_user_email` 与跨仓
/// 「只看我」过滤(`history.rs`)共用,避免两处各写一份 git config 读取。
pub(crate) async fn read_git_user_email(repo: &std::path::Path) -> Option<String> {
    let git_exe = which::which("git").ok()?;
    let out = crate::proc::run_capture_with_timeout(
        &git_exe,
        &["config", "--get", "user.email"],
        Some(repo),
        std::time::Duration::from_secs(5),
    )
    .await
    .ok()?;
    if out.status != 0 {
        return None;
    }
    let email = out.stdout.trim().to_lowercase();
    (!email.is_empty()).then_some(email)
}

#[tauri::command]
pub async fn current_git_user_email(state: State<'_, AppState>) -> Result<Option<String>, String> {
    let repo_path: String = {
        let g = state
            .current_repo
            .read()
            .map_err(|_| "current_repo 锁中毒".to_string())?;
        match g.as_ref() {
            Some(r) => r.path.clone(),
            None => return Ok(None),
        }
    };
    Ok(read_git_user_email(&std::path::PathBuf::from(repo_path)).await)
}

#[tauri::command]
pub async fn detect_dirty(path: String) -> Result<Option<bool>, String> {
    let p = std::path::PathBuf::from(&path);
    tokio::task::spawn_blocking(move || repo::head::detect_dirty(&p))
        .await
        .map_err(|e| format!("dirty 探测失败: {e}"))
}

/// 读取最近打开的仓库，配置错误直接返回。
#[tauri::command]
pub async fn list_recent_repos() -> Result<Vec<String>, String> {
    // 1. 从有效配置读取最近仓库
    Ok(AppSettings::load()?.recent_repos)
}

fn clear_recent_repo_history(settings: &mut AppSettings) -> usize {
    let cleared = settings.recent_repos.len();
    settings.recent_repos.clear();
    cleared
}

/// 仅清空顶部仓库切换器的最近打开记录。
///
/// 当前仓库与 `last_repo` 继续保留,避免“清历史”意外改变正在查看的仓库或下次启动恢复行为。
#[tauri::command]
pub async fn clear_recent_repos() -> Result<usize, String> {
    // 1. 读取有效配置后清空历史并保存
    let mut settings = AppSettings::load()?;
    let cleared = clear_recent_repo_history(&mut settings);
    settings.save().map_err(|e| format!("写配置失败: {e}"))?;
    Ok(cleared)
}

/// 读取仓库扫描根目录，配置错误直接返回。
#[tauri::command]
pub async fn list_scan_roots() -> Result<Vec<String>, String> {
    // 1. 从有效配置读取扫描根目录
    Ok(AppSettings::load()?.scan_roots)
}

/// 更新扫描根目录，保留已有配置中的其它偏好。
#[tauri::command]
pub async fn set_scan_roots(roots: Vec<String>) -> Result<(), String> {
    // 1. 读取有效配置后更新扫描根目录并保存
    let mut settings = AppSettings::load()?;
    settings.scan_roots = roots;
    settings.save().map_err(|e| format!("写配置失败: {e}"))
}

/// `get_aggregate_repos` 的返回项:聚合集合里的一个仓库 + 其当前有效性。
/// 失效(路径已删 / 不再是 git 仓)时 `valid=false`、`entry=None` —— **不静默丢弃**,
/// 让前端能渲染"失效仓"并提供移除入口(M1)。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AggregateRepoEntry {
    /// 持久化在 config 里的原始(已规整)路径。
    pub path: String,
    /// 该路径当前是否仍是合法 git 仓库。
    pub valid: bool,
    /// 有效时填充完整 RepoEntry;失效为 None。
    pub entry: Option<RepoEntry>,
}

/// 按**精确规整路径**去重,保留首次出现顺序。入参应已 normalize()(canonicalize)。
///
/// 不做 lowercase:大小写折叠交给 `canonicalize` 按平台处理 —— macOS/Windows 文件系统大小写
/// 不敏感,canonicalize 会把同一目录的不同大小写写法解析成同一真实路径;Linux 大小写敏感,
/// `/data/Repo` 与 `/data/repo` 是**两个不同仓库**,绝不能被 lowercase 误并。纯函数,便于单测。
fn dedup_aggregate_paths(normalized: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for p in normalized {
        if seen.insert(p.clone()) {
            out.push(p);
        }
    }
    out
}

/// 读聚合仓库集合,逐个校验是否仍为合法 git 仓(失效项标注返回,不丢弃)。
#[tauri::command]
pub async fn get_aggregate_repos() -> Result<Vec<AggregateRepoEntry>, String> {
    // 1. 严格读取聚合集合后校验每个仓库
    let paths = AppSettings::load()?.aggregate_repos;
    tokio::task::spawn_blocking(move || {
        paths
            .into_iter()
            .map(|path| {
                let normalized = normalize(&path);
                let entry = if normalized.is_dir() {
                    let ns = normalized.display().to_string();
                    repo::discover::scan_roots(std::slice::from_ref(&ns), Some(1))
                        .into_iter()
                        .find(|e| {
                            e.path.eq_ignore_ascii_case(&ns)
                                || normalize(&e.path).display().to_string() == ns
                        })
                } else {
                    None
                };
                AggregateRepoEntry {
                    path,
                    valid: entry.is_some(),
                    entry,
                }
            })
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|e| format!("校验聚合仓库失败: {e}"))
}

/// 设置聚合仓库集合:每条 normalize(canonicalize)+ 大小写不敏感去重后持久化。
/// **绝不触碰** current_repo / recent_repos(与下钻焦点正交,M1)。
#[tauri::command]
pub async fn set_aggregate_repos(repos: Vec<String>) -> Result<(), String> {
    // 1. 规范化仓库路径并合并到有效配置
    let normalized: Vec<String> = repos
        .iter()
        .map(|r| normalize(r).display().to_string())
        .collect();
    let mut settings = AppSettings::load()?;
    settings.aggregate_repos = dedup_aggregate_paths(normalized);
    settings.save().map_err(|e| format!("写配置失败: {e}"))
}

/// 从有效配置恢复上次仓库；配置损坏时不恢复错误的默认仓库状态。
#[tauri::command]
pub async fn restore_last_repo(state: State<'_, AppState>) -> Result<Option<RepoEntry>, String> {
    // 1. 读取有效配置并验证上次仓库仍可访问
    let settings = AppSettings::load()?;
    let Some(last) = settings.last_repo else {
        return Ok(None);
    };
    let normalized = normalize(&last);
    if !normalized.is_dir() {
        return Ok(None);
    }
    let normalized_str = normalized.display().to_string();
    let scanned = repo::discover::scan_roots(std::slice::from_ref(&normalized_str), Some(1));
    let mut entry = match scanned.into_iter().find(|e| {
        e.path.eq_ignore_ascii_case(&normalized_str)
            || normalize(&e.path).display().to_string() == normalized_str
    }) {
        Some(e) => e,
        None => return Ok(None),
    };
    entry.dirty = tokio::task::spawn_blocking({
        let p = std::path::PathBuf::from(&entry.path);
        move || repo::head::detect_dirty(&p)
    })
    .await
    .ok()
    .flatten();
    if let Ok(mut g) = state.current_repo.write() {
        *g = Some(entry.clone());
    }
    Ok(Some(entry))
}

#[tauri::command]
pub async fn open_in_explorer(path: String) -> Result<(), String> {
    #[cfg(windows)]
    {
        let mut cmd = std::process::Command::new("explorer");
        cmd.arg(&path);
        apply_no_window_std(&mut cmd);
        cmd.spawn().map_err(|e| format!("打开文件夹失败: {e}"))?;
    }
    #[cfg(target_os = "macos")]
    {
        let mut cmd = std::process::Command::new("open");
        cmd.arg(&path);
        apply_no_window_std(&mut cmd);
        cmd.spawn().map_err(|e| format!("打开文件夹失败: {e}"))?;
    }
    #[cfg(target_os = "linux")]
    {
        let mut cmd = std::process::Command::new("xdg-open");
        cmd.arg(&path);
        apply_no_window_std(&mut cmd);
        cmd.spawn().map_err(|e| format!("打开文件夹失败: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{clear_recent_repo_history, dedup_aggregate_paths};
    use crate::state::AppSettings;

    #[test]
    fn dedup_removes_exact_duplicates_order_preserving() {
        // 入参已 canonicalize;去重按精确路径,保留首次顺序。
        // 大小写差异不在此处折叠(交给 canonicalize 按平台处理),故大小写不同视为不同仓。
        let out = dedup_aggregate_paths(vec![
            "/ws/repo-a".to_string(),
            "/ws/repo-b".to_string(),
            "/ws/repo-a".to_string(), // 完全重复 → 去重
        ]);
        assert_eq!(out, vec!["/ws/repo-a", "/ws/repo-b"]);
    }

    #[test]
    fn dedup_empty_stays_empty() {
        assert!(dedup_aggregate_paths(vec![]).is_empty());
    }

    #[test]
    fn clearing_recent_history_preserves_other_repository_settings() {
        let mut settings = AppSettings {
            recent_repos: vec!["/ws/repo-a".to_string(), "/ws/repo-b".to_string()],
            last_repo: Some("/ws/repo-a".to_string()),
            scan_roots: vec!["/ws".to_string()],
            aggregate_repos: vec!["/ws/repo-b".to_string()],
            ..AppSettings::default()
        };

        assert_eq!(clear_recent_repo_history(&mut settings), 2);
        assert!(settings.recent_repos.is_empty());
        assert_eq!(settings.last_repo.as_deref(), Some("/ws/repo-a"));
        assert_eq!(settings.scan_roots, vec!["/ws"]);
        assert_eq!(settings.aggregate_repos, vec!["/ws/repo-b"]);
    }
}
