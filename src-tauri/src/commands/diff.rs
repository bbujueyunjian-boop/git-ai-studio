//! 单 commit 改动文件 + AI 行查询(任务 #2:Dashboard / Commit 详情跳转代码)。
//!
//! 暴露 2 个命令:
//! - `list_changed_files_in_commit(sha)` → 该 commit 的改动文件 + 逐文件新增行三桶
//!   (AI / known-human / unknown；删除行单列)
//! - `list_ai_lines_in_commit(sha)`     → 该 commit 内被 git-ai 标为 AI 的文件 × 行段
//!
//! # 与 stats / notes 的分工
//! - `stats` 只给数字,不给文件 / 行号
//! - `notes_ai::run_show` 解析完整 authorship/3.0.0 log(含 attestations + metadata)
//! - `list_changed_files_in_commit` 以原始 Git diff 为文件分母，将 attestation 范围与
//!   本 commit 的 new-side 新增行求交；不直接累加可能包含存量行的 note 范围
//! - `list_ai_lines_in_commit` 保留旧的 (file, line_start, line_end) 轻量投影接口
//!
//! # merge commit
//! 不任选某个 parent 计算占比；文件列表仍用 `git diff-tree -m` 展示，line_stats 为 null。
//!
//! # 错误归类
//! - 未选仓库 → degraded `RepoMissing`
//! - sha 不解析为合法 commit → degraded `InvalidSha`
//! - 没有 AI notes(`refs/notes/ai show <sha>` 失败 / ref 不存在)→ 空数组(初始态,非降级)
//! - 子进程 / 解析硬故障 → Err(String)

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::State;

use crate::git_ai::{is_missing_notes_ref, notes_ai, NOTES_REF};
use crate::proc::run_capture_with_timeout;
use crate::state::AppState;

const GIT_TIMEOUT: Duration = Duration::from_secs(15);

/// 单条改动文件:path 已 POSIX 化(`/` 分隔),status 透传 `git diff --name-status`
/// 第一列字符(A/M/D/R/C/T/U/X/B)。前端按字符渲染色块,后端不做语义抽象。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct ChangedFile {
    pub path: String,
    pub status: String,
    /// `None` 仅用于 merge commit 或二进制文件；文本文件即使没有新增行也返回 `Some`。
    pub line_stats: Option<FileLineStats>,
}

/// 单文件在本 commit 的新增行三桶。分母恒为 additions，删除行只单列、不进入占比。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct FileLineStats {
    pub additions: u32,
    pub deletions: u32,
    pub ai_additions: u32,
    pub human_additions: u32,
    pub unknown_additions: u32,
}

/// 单条 AI 行段:`(file, line_start, line_end)` 闭区间。
/// 由 `notes_ai::AttestationEntry.line_ranges` 字符串展开而来 —— 一个 entry 的
/// `"1-10,15,20-25"` 会被拆成 3 段。前端 Stats 页据此显示"本 commit 改了 N 行 AI"。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct AiLineRef {
    pub file: String,
    pub line_start: u32,
    pub line_end: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiffDegradedReason {
    RepoMissing,
    /// 用户传入的 sha 无法 peel 到 commit 对象(空仓 / 拼写错 / 仓库内不存在该 commit)。
    InvalidSha {
        sha: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ChangedFilesResult {
    Ok {
        files: Vec<ChangedFile>,
        /// merge 不任选某个 parent 计算分母，与 git-ai commit stats 的 merge 口径一致。
        is_merge: bool,
    },
    Degraded { reason: DiffDegradedReason },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AiLinesResult {
    Ok { lines: Vec<AiLineRef> },
    Degraded { reason: DiffDegradedReason },
}

/// 校验 sha 能 peel 到 commit。语义与 `commands::blame::verify_ref_is_commit` 一致,
/// 但这里只服务 diff 命令,不复用以保持模块独立。
async fn verify_sha_is_commit(
    git: &std::path::Path,
    repo: &std::path::Path,
    sha: &str,
) -> Result<bool, String> {
    let spec = format!("{sha}^{{commit}}");
    let out = run_capture_with_timeout(
        git,
        &["rev-parse", "--verify", "--quiet", &spec],
        Some(repo),
        GIT_TIMEOUT,
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(out.status == 0)
}

#[tauri::command]
pub async fn list_changed_files_in_commit(
    sha: String,
    state: State<'_, AppState>,
) -> Result<ChangedFilesResult, String> {
    let Some(repo_path) = take_repo_path(&state)? else {
        return Ok(ChangedFilesResult::Degraded {
            reason: DiffDegradedReason::RepoMissing,
        });
    };
    let git = which::which("git").map_err(|_| "未找到 git 二进制".to_string())?;

    if !verify_sha_is_commit(&git, &repo_path, &sha).await? {
        return Ok(ChangedFilesResult::Degraded {
            reason: DiffDegradedReason::InvalidSha { sha },
        });
    }

    let parents = commit_parents(&git, &repo_path, &sha).await?;
    if parents.len() > 1 {
        let files = list_merge_changed_files(&git, &repo_path, &sha).await?;
        return Ok(ChangedFilesResult::Ok {
            files,
            is_merge: true,
        });
    }

    let base = parents
        .first()
        .map(String::as_str)
        .unwrap_or(crate::git_ai::stats::EMPTY_TREE_HASH);
    let mut files = list_non_merge_changed_files(&git, &repo_path, base, &sha).await?;
    let added_ranges = list_added_line_ranges(&git, &repo_path, base, &sha).await?;
    let authorship = load_authorship_log(&git, &repo_path, &sha).await?;
    attach_file_line_stats(&mut files, &added_ranges, authorship.as_ref())?;

    Ok(ChangedFilesResult::Ok {
        files,
        is_merge: false,
    })
}

#[tauri::command]
pub async fn list_ai_lines_in_commit(
    sha: String,
    state: State<'_, AppState>,
) -> Result<AiLinesResult, String> {
    let Some(repo_path) = take_repo_path(&state)? else {
        return Ok(AiLinesResult::Degraded {
            reason: DiffDegradedReason::RepoMissing,
        });
    };
    let git = which::which("git").map_err(|_| "未找到 git 二进制".to_string())?;

    if !verify_sha_is_commit(&git, &repo_path, &sha).await? {
        return Ok(AiLinesResult::Degraded {
            reason: DiffDegradedReason::InvalidSha { sha },
        });
    }

    let Some(log) = load_authorship_log(&git, &repo_path, &sha).await? else {
        return Ok(AiLinesResult::Ok { lines: vec![] });
    };
    let lines = expand_ai_lines_from_attestations(&log)?;
    Ok(AiLinesResult::Ok { lines })
}

// ===== IO helper =====

async fn commit_parents(
    git: &std::path::Path,
    repo: &std::path::Path,
    sha: &str,
) -> Result<Vec<String>, String> {
    let out = run_capture_with_timeout(
        git,
        &["rev-list", "--parents", "-n", "1", sha],
        Some(repo),
        GIT_TIMEOUT,
    )
    .await
    .map_err(|e| e.to_string())?;
    if out.status != 0 {
        return Err(format!(
            "git rev-list 退出码 {}: {}",
            out.status,
            out.stderr.trim()
        ));
    }
    parse_commit_parents(&out.stdout)
}

async fn list_merge_changed_files(
    git: &std::path::Path,
    repo: &std::path::Path,
    sha: &str,
) -> Result<Vec<ChangedFile>, String> {
    let out = run_capture_with_timeout(
        git,
        &[
            "-c",
            "core.quotePath=false",
            "diff-tree",
            "--root",
            "--no-commit-id",
            "--name-status",
            "-r",
            "-m",
            "--find-renames=1%",
            sha,
        ],
        Some(repo),
        GIT_TIMEOUT,
    )
    .await
    .map_err(|e| e.to_string())?;
    if out.status != 0 {
        return Err(format!(
            "git diff-tree 退出码 {}: {}",
            out.status,
            out.stderr.trim()
        ));
    }
    Ok(parse_diff_tree_name_status(&out.stdout))
}

async fn list_non_merge_changed_files(
    git: &std::path::Path,
    repo: &std::path::Path,
    base: &str,
    sha: &str,
) -> Result<Vec<ChangedFile>, String> {
    let out = run_capture_with_timeout(
        git,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "--raw",
            "--numstat",
            "-z",
            "--find-renames=1%",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            base,
            sha,
            "--",
        ],
        Some(repo),
        GIT_TIMEOUT,
    )
    .await
    .map_err(|e| e.to_string())?;
    if out.status != 0 {
        return Err(format!(
            "git diff --raw --numstat 退出码 {}: {}",
            out.status,
            out.stderr.trim()
        ));
    }
    parse_raw_numstat_z(&out.stdout)
}

async fn list_added_line_ranges(
    git: &std::path::Path,
    repo: &std::path::Path,
    base: &str,
    sha: &str,
) -> Result<BTreeMap<String, Vec<LineRange>>, String> {
    let args = added_line_diff_args(base, sha);
    let out = run_capture_with_timeout(git, &args, Some(repo), GIT_TIMEOUT)
        .await
        .map_err(|e| e.to_string())?;
    if out.status != 0 {
        return Err(format!(
            "git diff -U0 退出码 {}: {}",
            out.status,
            out.stderr.trim()
        ));
    }
    parse_zero_context_added_ranges(&out.stdout)
}

fn added_line_diff_args<'a>(base: &'a str, sha: &'a str) -> [&'a str; 13] {
    [
        "-c",
        "core.quotePath=false",
        "diff",
        "-U0",
        // `-U0` 不会覆盖用户的 diff.interHunkContext；显式归零才能保证 hunk
        // new-side count 只含新增行，不把合并 hunk 之间的上下文算进分母。
        "--inter-hunk-context=0",
        "--find-renames=1%",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--no-prefix",
        base,
        sha,
        "--",
    ]
}

async fn load_authorship_log(
    git: &std::path::Path,
    repo: &std::path::Path,
    sha: &str,
) -> Result<Option<notes_ai::AuthorshipLog>, String> {
    let out = run_capture_with_timeout(
        git,
        &["notes", "--ref", NOTES_REF, "show", sha],
        Some(repo),
        GIT_TIMEOUT,
    )
    .await
    .map_err(|e| e.to_string())?;
    if out.status != 0 {
        if is_missing_notes_ref(&out.stderr) || stderr_means_no_note_for_sha(&out.stderr) {
            return Ok(None);
        }
        return Err(format!(
            "git notes show {} 退出码 {}: {}",
            sha,
            out.status,
            out.stderr.trim()
        ));
    }
    notes_ai::parse_authorship_log(&out.stdout)
        .map(Some)
        .map_err(|e| e.to_string())
}

// ===== 解析层(纯函数,无 IO,方便单测覆盖)=====

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LineRange {
    start: u32,
    end: u32,
}

fn parse_commit_parents(stdout: &str) -> Result<Vec<String>, String> {
    let mut parts = stdout.split_whitespace();
    if parts.next().is_none() {
        return Err("git rev-list 返回空输出".to_string());
    }
    Ok(parts.map(str::to_string).collect())
}

/// 解析 `git diff --raw --numstat -z` 的两个连续 NUL 分隔区块。
/// raw 区块提供 status/rename 新路径，numstat 区块提供增删数；二进制的两个数均为 `-`。
fn parse_raw_numstat_z(stdout: &str) -> Result<Vec<ChangedFile>, String> {
    #[derive(Debug)]
    struct RawFile {
        path: String,
        status: String,
    }

    let fields: Vec<&str> = stdout.split('\0').collect();
    let mut index = 0usize;
    let mut raw_files = Vec::new();

    while index < fields.len() {
        let header = fields[index];
        if header.is_empty() {
            index += 1;
            continue;
        }
        if !header.starts_with(':') {
            break;
        }
        let status_raw = header
            .split_whitespace()
            .last()
            .ok_or_else(|| format!("raw diff header 缺少 status: {header:?}"))?;
        let status = status_raw
            .chars()
            .next()
            .filter(|c| c.is_ascii_alphabetic())
            .map(|c| c.to_ascii_uppercase().to_string())
            .ok_or_else(|| format!("raw diff status 非法: {status_raw:?}"))?;
        index += 1;

        let path = if status == "R" || status == "C" {
            let old_path = fields
                .get(index)
                .filter(|p| !p.is_empty())
                .ok_or_else(|| format!("{status} raw diff 缺少旧路径"))?;
            let new_path = fields
                .get(index + 1)
                .filter(|p| !p.is_empty())
                .ok_or_else(|| format!("{status} raw diff 缺少新路径"))?;
            let _ = old_path;
            index += 2;
            *new_path
        } else {
            let path = fields
                .get(index)
                .filter(|p| !p.is_empty())
                .ok_or_else(|| format!("{status} raw diff 缺少路径"))?;
            index += 1;
            *path
        };
        raw_files.push(RawFile {
            path: path.replace('\\', "/"),
            status,
        });
    }

    let mut counts: BTreeMap<String, Option<(u32, u32)>> = BTreeMap::new();
    while index < fields.len() {
        let record = fields[index];
        index += 1;
        if record.is_empty() {
            continue;
        }
        let mut parts = record.splitn(3, '\t');
        let additions_raw = parts
            .next()
            .ok_or_else(|| format!("numstat 缺少 additions: {record:?}"))?;
        let deletions_raw = parts
            .next()
            .ok_or_else(|| format!("numstat 缺少 deletions: {record:?}"))?;
        let inline_path = parts
            .next()
            .ok_or_else(|| format!("numstat 缺少 path: {record:?}"))?;

        let path = if inline_path.is_empty() {
            let old_path = fields
                .get(index)
                .filter(|p| !p.is_empty())
                .ok_or_else(|| "rename numstat 缺少旧路径".to_string())?;
            let new_path = fields
                .get(index + 1)
                .filter(|p| !p.is_empty())
                .ok_or_else(|| "rename numstat 缺少新路径".to_string())?;
            let _ = old_path;
            index += 2;
            *new_path
        } else {
            inline_path
        }
        .replace('\\', "/");

        let additions = parse_numstat_value(additions_raw)?;
        let deletions = parse_numstat_value(deletions_raw)?;
        let value = match (additions, deletions) {
            (Some(a), Some(d)) => Some((a, d)),
            (None, None) => None,
            _ => return Err(format!("numstat 二进制标记不成对: {record:?}")),
        };
        if counts.insert(path.clone(), value).is_some() {
            return Err(format!("numstat 出现重复路径: {path}"));
        }
    }

    let mut files = Vec::with_capacity(raw_files.len());
    for raw in raw_files {
        let counts_for_file = counts
            .remove(&raw.path)
            .ok_or_else(|| format!("raw diff 路径缺少 numstat: {}", raw.path))?;
        files.push(ChangedFile {
            path: raw.path,
            status: raw.status,
            line_stats: counts_for_file.map(|(additions, deletions)| FileLineStats {
                additions,
                deletions,
                ai_additions: 0,
                human_additions: 0,
                unknown_additions: 0,
            }),
        });
    }
    if let Some(path) = counts.keys().next() {
        return Err(format!("numstat 路径缺少 raw diff: {path}"));
    }
    Ok(files)
}

fn parse_numstat_value(raw: &str) -> Result<Option<u32>, String> {
    if raw == "-" {
        return Ok(None);
    }
    raw.parse::<u32>()
        .map(Some)
        .map_err(|_| format!("numstat 行数非法: {raw:?}"))
}

/// `-U0` 没有上下文行，因此 hunk header 的 new-side count 就是本次新增行数。
fn parse_zero_context_added_ranges(
    stdout: &str,
) -> Result<BTreeMap<String, Vec<LineRange>>, String> {
    let mut result: BTreeMap<String, Vec<LineRange>> = BTreeMap::new();
    let mut current_path: Option<String> = None;
    let mut saw_old_header = false;
    let mut in_hunks = false;

    for line in stdout.lines() {
        if line.starts_with("diff --git ") {
            current_path = None;
            saw_old_header = false;
            in_hunks = false;
            continue;
        }
        if !in_hunks && line.starts_with("--- ") {
            saw_old_header = true;
            continue;
        }
        if !in_hunks && saw_old_header && line.starts_with("+++ ") {
            let path = line.trim_start_matches("+++ ");
            current_path = (path != "/dev/null").then(|| path.replace('\\', "/"));
            continue;
        }
        if line.starts_with("@@ ") {
            in_hunks = true;
            let (new_start, new_count) = parse_hunk_new_range(line)?;
            if new_count == 0 {
                continue;
            }
            let path = current_path
                .as_ref()
                .ok_or_else(|| format!("hunk 缺少 new file header: {line:?}"))?;
            let end = new_start
                .checked_add(new_count - 1)
                .ok_or_else(|| format!("hunk new range 溢出: {line:?}"))?;
            result
                .entry(path.clone())
                .or_default()
                .push(LineRange {
                    start: new_start,
                    end,
                });
        }
    }

    for (path, ranges) in &mut result {
        ranges.sort_by_key(|r| (r.start, r.end));
        for pair in ranges.windows(2) {
            if pair[0].end >= pair[1].start {
                return Err(format!("diff 新增区间重叠: {path}"));
            }
        }
    }
    Ok(result)
}

fn parse_hunk_new_range(line: &str) -> Result<(u32, u32), String> {
    let body = line
        .strip_prefix("@@ ")
        .and_then(|s| s.split_once(" @@").map(|(coords, _)| coords))
        .ok_or_else(|| format!("hunk header 格式非法: {line:?}"))?;
    let mut coords = body.split_whitespace();
    let old = coords
        .next()
        .ok_or_else(|| format!("hunk header 缺少 old range: {line:?}"))?;
    let new = coords
        .next()
        .ok_or_else(|| format!("hunk header 缺少 new range: {line:?}"))?;
    if !old.starts_with('-') || !new.starts_with('+') || coords.next().is_some() {
        return Err(format!("hunk header 坐标非法: {line:?}"));
    }
    parse_hunk_range(&new[1..], line)
}

fn parse_hunk_range(raw: &str, line: &str) -> Result<(u32, u32), String> {
    let (start_raw, count_raw) = raw.split_once(',').unwrap_or((raw, "1"));
    let start = start_raw
        .parse::<u32>()
        .map_err(|_| format!("hunk new start 非法: {line:?}"))?;
    let count = count_raw
        .parse::<u32>()
        .map_err(|_| format!("hunk new count 非法: {line:?}"))?;
    if count > 0 && start == 0 {
        return Err(format!("hunk 非空 new range 从 0 开始: {line:?}"));
    }
    Ok((start, count))
}

fn attach_file_line_stats(
    files: &mut [ChangedFile],
    added_ranges: &BTreeMap<String, Vec<LineRange>>,
    log: Option<&notes_ai::AuthorshipLog>,
) -> Result<(), String> {
    for file in files.iter_mut() {
        let Some(stats) = file.line_stats.as_mut() else {
            continue;
        };
        let ranges = added_ranges.get(&file.path).map(Vec::as_slice).unwrap_or(&[]);
        let additions_from_hunks = ranges.iter().try_fold(0u32, |sum, range| {
            sum.checked_add(range.end - range.start + 1)
                .ok_or_else(|| format!("diff 新增行数溢出: {}", file.path))
        })?;
        if additions_from_hunks != stats.additions {
            return Err(format!(
                "diff hunk 与 numstat 新增数不一致: {} (hunk={}, numstat={})",
                file.path, additions_from_hunks, stats.additions
            ));
        }

        let (ai, human) = count_attributed_additions(&file.path, ranges, log)?;
        let attributed = ai
            .checked_add(human)
            .ok_or_else(|| format!("文件归因行数溢出: {}", file.path))?;
        let unknown = stats.additions.checked_sub(attributed).ok_or_else(|| {
            format!(
                "文件归因行数超过新增数: {} (AI={}, human={}, additions={})",
                file.path, ai, human, stats.additions
            )
        })?;
        stats.ai_additions = ai;
        stats.human_additions = human;
        stats.unknown_additions = unknown;
    }

    if let Some(path) = added_ranges.keys().find(|path| !files.iter().any(|f| &f.path == *path)) {
        return Err(format!("diff hunk 路径缺少 raw/numstat: {path}"));
    }
    Ok(())
}

fn count_attributed_additions(
    file_path: &str,
    added_ranges: &[LineRange],
    log: Option<&notes_ai::AuthorshipLog>,
) -> Result<(u32, u32), String> {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Owner {
        Ai,
        Human,
    }

    let Some(log) = log else {
        return Ok((0, 0));
    };
    let Some(attestation) = log
        .attestations
        .iter()
        .find(|entry| entry.file_path == file_path)
    else {
        return Ok((0, 0));
    };

    let mut intersections: Vec<(LineRange, Owner)> = Vec::new();
    for entry in &attestation.entries {
        let owner = if entry.hash.starts_with("h_") {
            Owner::Human
        } else {
            Owner::Ai
        };
        for attested in parse_line_ranges(&entry.line_ranges)? {
            for added in added_ranges {
                let start = attested.start.max(added.start);
                let end = attested.end.min(added.end);
                if start <= end {
                    intersections.push((LineRange { start, end }, owner));
                }
            }
        }
    }
    intersections.sort_by_key(|(range, _)| (range.start, range.end));
    for pair in intersections.windows(2) {
        if pair[0].0.end >= pair[1].0.start {
            return Err(format!("authorship 归因区间重叠: {file_path}"));
        }
    }

    intersections
        .into_iter()
        .try_fold((0u32, 0u32), |(ai, human), (range, owner)| {
            let count = range.end - range.start + 1;
            match owner {
                Owner::Ai => ai
                    .checked_add(count)
                    .map(|next| (next, human))
                    .ok_or_else(|| format!("AI 归因行数溢出: {file_path}")),
                Owner::Human => human
                    .checked_add(count)
                    .map(|next| (ai, next))
                    .ok_or_else(|| format!("人工归因行数溢出: {file_path}")),
            }
        })
}

/// 解析 `git diff-tree --name-status -r -m <sha>` 的 stdout。
///
/// 行格式:
/// - 普通改动:`<S>\t<path>`         (S ∈ {A,M,D,T,U,X,B})
/// - rename/copy:`<S><score>\t<old>\t<new>` (S ∈ {R,C},score 是相似度百分比数字)
///
/// 输出:按 path 去重 —— merge commit `-m` 会对每个 parent 重复输出，同一路径在
/// 不同 parent 下的 status 也可能不同；这里只保留首次出现的 status，避免前端重复行。
pub fn parse_diff_tree_name_status(stdout: &str) -> Vec<ChangedFile> {
    use std::collections::HashSet;
    let mut out: Vec<ChangedFile> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for raw in stdout.lines() {
        if raw.trim().is_empty() {
            continue;
        }
        // 用 tab 切;空 tab 行视为格式异常,忽略
        let mut parts = raw.split('\t');
        let Some(status_raw) = parts.next() else {
            continue;
        };
        // rename/copy 的 status 形如 "R100" / "C75",首字符是字母,后跟相似度
        let status_char = status_raw
            .chars()
            .next()
            .map(|c| c.to_ascii_uppercase().to_string())
            .unwrap_or_default();
        if status_char.is_empty() {
            continue;
        }

        let path = if status_char == "R" || status_char == "C" {
            // rename/copy 行有两个路径列:`<old>\t<new>`。归到 <new>,因为 UI 想跳的是新路径
            let _old = parts.next();
            let Some(new_path) = parts.next() else {
                continue;
            };
            new_path
        } else {
            let Some(p) = parts.next() else {
                continue;
            };
            p
        };
        let path_posix = path.replace('\\', "/");
        if seen.insert(path_posix.clone()) {
            out.push(ChangedFile {
                path: path_posix,
                status: status_char,
                line_stats: None,
            });
        }
    }
    out
}

/// 把 `notes_ai::AuthorshipLog.attestations` 展开为 `AiLineRef` 列表。
///
/// 算法:
/// 1. 跳过 `h_` known-human attestation;`s_` session 与无前缀 legacy prompt 都属于 AI
/// 2. `line_ranges` 字符串按上游 `format_line_ranges` 真源解析:逗号分隔,每段 `n` 或 `start-end`
/// 3. 同 file 不同 entry 的段直接展开,本函数不去重或合并
fn expand_ai_lines_from_attestations(
    log: &notes_ai::AuthorshipLog,
) -> Result<Vec<AiLineRef>, String> {
    let mut out: Vec<AiLineRef> = Vec::new();
    for file in &log.attestations {
        for entry in &file.entries {
            // v3:`h_` 是 known human;`s_<session>::t_<trace>` 与 legacy prompt 都是 AI。
            if entry.hash.starts_with("h_") {
                continue;
            }
            for range in parse_line_ranges(&entry.line_ranges)? {
                out.push(AiLineRef {
                    file: file.file_path.clone(),
                    line_start: range.start,
                    line_end: range.end,
                });
            }
        }
    }
    Ok(out)
}

/// 解析 attestation 的 `line_ranges` 字符串。算法与前端 `parseLineRanges`
/// (`src/lib/types.ts`)严格一致,上游真源:
/// `git-ai/src/authorship/authorship_log_serialization.rs:576-598` `format_line_ranges`。
///
/// 失败语义:任一段格式错 / 空段 / 起点为 0 / start > end → Err，不把坏 note 静默算成 unknown。
fn parse_line_ranges(s: &str) -> Result<Vec<LineRange>, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    for seg in trimmed.split(',') {
        let part = seg.trim();
        if part.is_empty() {
            return Err(format!("line_ranges 含空段: {s:?}"));
        }
        let (a, b) = if let Some(dash) = part.find('-') {
            let lhs = &part[..dash];
            let rhs = &part[dash + 1..];
            match (lhs.parse::<u32>(), rhs.parse::<u32>()) {
                (Ok(x), Ok(y)) => (x, y),
                _ => return Err(format!("line_ranges 区间非法: {part:?}")),
            }
        } else {
            match part.parse::<u32>() {
                Ok(x) => (x, x),
                _ => return Err(format!("line_ranges 行号非法: {part:?}")),
            }
        };
        if a < 1 || b < a {
            return Err(format!("line_ranges 边界非法: {part:?}"));
        }
        out.push(LineRange { start: a, end: b });
    }
    Ok(out)
}

/// `git notes show <sha>` 当 sha 没有 note 时退出码非 0,stderr 形如
/// `error: no note found for object <sha>.` —— 视为正常空态,不上抛错误。
///
/// **不**用宽泛 `contains("no note")` 否则会吞掉真正的 "no note found for object: bad permissions" 类错;
/// 只要求精确匹配上游 message 关键字。
fn stderr_means_no_note_for_sha(stderr: &str) -> bool {
    let s = stderr.trim();
    // 上游 git: builtin/notes.c — "no note found for object <oid>"
    s.contains("no note found for object")
}

// ===== helper =====

fn take_repo_path(state: &State<'_, AppState>) -> Result<Option<PathBuf>, String> {
    let g = state
        .current_repo
        .read()
        .map_err(|_| "current_repo 锁中毒".to_string())?;
    Ok(g.as_ref().map(|r| PathBuf::from(&r.path)))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== parse_diff_tree_name_status =====

    #[test]
    fn parse_diff_tree_simple_status() {
        let s = "A\tsrc/foo.rs\nM\tsrc/bar.rs\nD\tdocs/old.md\n";
        let v = parse_diff_tree_name_status(s);
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].status, "A");
        assert_eq!(v[0].path, "src/foo.rs");
        assert_eq!(v[1].status, "M");
        assert_eq!(v[1].path, "src/bar.rs");
        assert_eq!(v[2].status, "D");
        assert_eq!(v[2].path, "docs/old.md");
    }

    #[test]
    fn parse_diff_tree_rename_uses_new_path() {
        // rename 行有 3 列:`R<score>\t<old>\t<new>` —— 归到新路径
        let s = "R100\tsrc/old_name.rs\tsrc/new_name.rs\nC75\tdocs/a.md\tdocs/b.md\n";
        let v = parse_diff_tree_name_status(s);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].status, "R");
        assert_eq!(v[0].path, "src/new_name.rs");
        assert_eq!(v[1].status, "C");
        assert_eq!(v[1].path, "docs/b.md");
    }

    #[test]
    fn parse_diff_tree_deduplicates_merge_parent_repeat_by_path() {
        // 同一路径相对不同 parent 的 status 可能不同，仍只展示一行并保留首状态。
        let s = "M\tsrc/foo.rs\nA\tsrc/foo.rs\nA\tnew.rs\n";
        let v = parse_diff_tree_name_status(s);
        assert_eq!(v.len(), 2, "重复行应去重: {v:?}");
        assert_eq!(v[0].path, "src/foo.rs");
        assert_eq!(v[0].status, "M");
        assert_eq!(v[1].path, "new.rs");
    }

    #[test]
    fn parse_diff_tree_normalizes_backslash_to_slash() {
        // git 在 Windows 上也用正斜杠输出,但保险起见做归一
        let s = "M\tsrc\\foo.rs\n";
        let v = parse_diff_tree_name_status(s);
        assert_eq!(v[0].path, "src/foo.rs");
    }

    #[test]
    fn parse_diff_tree_empty_and_malformed_skipped() {
        let s = "\n   \nM\n\tsrc/x.rs\nM\tsrc/y.rs\n";
        let v = parse_diff_tree_name_status(s);
        // - 空行跳过
        // - "M\n" 只有 status 没有 path → 跳过(parts.next 拿不到 path)
        // - "\tsrc/x.rs" status_char 空 → 跳过
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].path, "src/y.rs");
    }

    // ===== raw + numstat / hunk =====

    #[test]
    fn parse_raw_numstat_handles_text_rename_and_binary() {
        let raw = concat!(
            ":100644 100644 aaaaaaa bbbbbbb M\0src/a.rs\0",
            ":100644 100644 ccccccc ddddddd R090\0old.rs\0new.rs\0",
            ":100644 100644 eeeeeee fffffff M\0assets/a.png\0",
            "3\t1\tsrc/a.rs\0",
            "2\t4\t\0old.rs\0new.rs\0",
            "-\t-\tassets/a.png\0",
        );
        let files = parse_raw_numstat_z(raw).unwrap();
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].status, "M");
        assert_eq!(files[0].line_stats.as_ref().unwrap().additions, 3);
        assert_eq!(files[0].line_stats.as_ref().unwrap().deletions, 1);
        assert_eq!(files[1].status, "R");
        assert_eq!(files[1].path, "new.rs");
        assert_eq!(files[1].line_stats.as_ref().unwrap().additions, 2);
        assert!(files[2].line_stats.is_none());
    }

    #[test]
    fn parse_raw_numstat_rejects_missing_pair() {
        let raw = ":100644 100644 aaaaaaa bbbbbbb M\0src/a.rs\0";
        assert!(parse_raw_numstat_z(raw).is_err());
    }

    #[test]
    fn parse_hunk_new_ranges_supports_omitted_zero_and_multiple_counts() {
        let patch = concat!(
            "diff --git src/a.rs src/a.rs\n",
            "--- src/a.rs\n",
            "+++ src/a.rs\n",
            "@@ -2,0 +3,2 @@\n",
            "+a\n+b\n",
            "@@ -10 +12 @@\n",
            "-old\n+new\n",
            "diff --git src/deleted.rs src/deleted.rs\n",
            "--- src/deleted.rs\n",
            "+++ /dev/null\n",
            "@@ -1,3 +0,0 @@\n",
            "-a\n-b\n-c\n",
        );
        let parsed = parse_zero_context_added_ranges(patch).unwrap();
        assert_eq!(
            parsed.get("src/a.rs").unwrap(),
            &vec![
                LineRange { start: 3, end: 4 },
                LineRange { start: 12, end: 12 },
            ]
        );
        assert!(!parsed.contains_key("src/deleted.rs"));
    }

    #[test]
    fn parse_commit_parents_covers_root_normal_and_merge() {
        assert!(parse_commit_parents("self\n").unwrap().is_empty());
        assert_eq!(parse_commit_parents("self parent\n").unwrap(), vec!["parent"]);
        assert_eq!(
            parse_commit_parents("self p1 p2\n").unwrap(),
            vec!["p1", "p2"]
        );
        assert!(parse_commit_parents("").is_err());
    }

    #[test]
    fn added_line_diff_disables_configured_inter_hunk_context() {
        let args = added_line_diff_args("base", "sha");
        assert!(args.contains(&"-U0"));
        assert!(args.contains(&"--inter-hunk-context=0"));
        assert_eq!(&args[args.len() - 3..], &["base", "sha", "--"]);
    }

    // ===== parse_line_ranges =====

    #[test]
    fn parse_line_ranges_single() {
        assert_eq!(
            parse_line_ranges("5").unwrap(),
            vec![LineRange { start: 5, end: 5 }]
        );
    }

    #[test]
    fn parse_line_ranges_range() {
        assert_eq!(
            parse_line_ranges("1-10").unwrap(),
            vec![LineRange { start: 1, end: 10 }]
        );
    }

    #[test]
    fn parse_line_ranges_multi() {
        assert_eq!(
            parse_line_ranges("5,10-15,20-25").unwrap(),
            vec![
                LineRange { start: 5, end: 5 },
                LineRange { start: 10, end: 15 },
                LineRange { start: 20, end: 25 },
            ]
        );
    }

    #[test]
    fn parse_line_ranges_empty_and_whitespace() {
        assert!(parse_line_ranges("").unwrap().is_empty());
        assert!(parse_line_ranges("   ").unwrap().is_empty());
    }

    #[test]
    fn parse_line_ranges_fail_fast() {
        for bad in ["10-5", "0", "abc", "5,bad,10", "5,,10"] {
            assert!(parse_line_ranges(bad).is_err(), "应拒绝 {bad:?}");
        }
    }

    // ===== file attribution =====

    fn attribution_log(entries: &str) -> notes_ai::AuthorshipLog {
        let raw = format!(
            "src/x.rs\n{entries}---\n{{\"schema_version\":\"authorship/3.0.0\",\"base_commit_sha\":\"base\"}}\n"
        );
        notes_ai::parse_authorship_log(&raw).unwrap()
    }

    #[test]
    fn attribution_only_counts_intersection_with_added_lines() {
        let log = attribution_log("  legacy_hash 1-100\n");
        let added = [
            LineRange { start: 50, end: 50 },
            LineRange { start: 80, end: 80 },
        ];
        assert_eq!(
            count_attributed_additions("src/x.rs", &added, Some(&log)).unwrap(),
            (2, 0)
        );
    }

    #[test]
    fn attribution_separates_ai_human_and_unknown() {
        let log = attribution_log(
            "  s_abcdef0123456::t_1234567890abcd 1-2\n  h_31dce776f88375 3-5\n",
        );
        let mut files = vec![ChangedFile {
            path: "src/x.rs".into(),
            status: "M".into(),
            line_stats: Some(FileLineStats {
                additions: 10,
                deletions: 4,
                ai_additions: 0,
                human_additions: 0,
                unknown_additions: 0,
            }),
        }];
        let ranges = BTreeMap::from([(
            "src/x.rs".into(),
            vec![LineRange { start: 1, end: 10 }],
        )]);
        attach_file_line_stats(&mut files, &ranges, Some(&log)).unwrap();
        let stats = files[0].line_stats.as_ref().unwrap();
        assert_eq!(stats.ai_additions, 2);
        assert_eq!(stats.human_additions, 3);
        assert_eq!(stats.unknown_additions, 5);
        assert_eq!(
            stats.ai_additions + stats.human_additions + stats.unknown_additions,
            stats.additions
        );
    }

    #[test]
    fn no_note_marks_all_additions_unknown() {
        let mut files = vec![ChangedFile {
            path: "src/x.rs".into(),
            status: "A".into(),
            line_stats: Some(FileLineStats {
                additions: 3,
                deletions: 0,
                ai_additions: 0,
                human_additions: 0,
                unknown_additions: 0,
            }),
        }];
        let ranges = BTreeMap::from([(
            "src/x.rs".into(),
            vec![LineRange { start: 1, end: 3 }],
        )]);
        attach_file_line_stats(&mut files, &ranges, None).unwrap();
        assert_eq!(files[0].line_stats.as_ref().unwrap().unknown_additions, 3);
    }

    #[test]
    fn malformed_or_overlapping_attestation_fails_explicitly() {
        let malformed = attribution_log("  legacy_hash bad\n");
        let added = [LineRange { start: 1, end: 5 }];
        assert!(count_attributed_additions("src/x.rs", &added, Some(&malformed)).is_err());

        let overlapping = attribution_log("  legacy_hash 1-3\n  h_31dce776f88375 3-5\n");
        assert!(count_attributed_additions("src/x.rs", &added, Some(&overlapping)).is_err());
    }

    // ===== expand_ai_lines_from_attestations =====

    #[test]
    fn expand_ai_lines_includes_legacy_and_session_skips_human() {
        // legacy prompt(无前缀)和 s_ session 都进入结果;仅 h_ known human 跳过
        let log_text = r#"src/main.rs
  abcd1234abcd1234 1-10,15
  h_31dce776f88375 11-14
src/lib.rs
  s_abcdef0123456::t_1234567890abcd 1-50
---
{
  "schema_version": "authorship/3.0.0",
  "base_commit_sha": "x",
  "prompts": {
    "abcd1234abcd1234": {
      "agent_id": {"tool":"claude_code","id":"s","model":"m"},
      "messages": [],
      "total_additions": 11, "total_deletions": 0,
      "accepted_lines": 11, "overriden_lines": 0
    }
  },
  "humans": { "h_31dce776f88375": {"author":"Alice"} },
  "sessions": {
    "s_abcdef0123456": {
      "agent_id": {"tool":"codex","id":"session-1","model":"gpt-5.6-sol"},
      "human_author": "Alice"
    }
  }
}
"#;
        let log = notes_ai::parse_authorship_log(log_text).unwrap();
        let lines = expand_ai_lines_from_attestations(&log).unwrap();
        // legacy prompt 两段 + session 一段进入结果;human 段被过滤
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[0],
            AiLineRef {
                file: "src/main.rs".into(),
                line_start: 1,
                line_end: 10,
            }
        );
        assert_eq!(
            lines[1],
            AiLineRef {
                file: "src/main.rs".into(),
                line_start: 15,
                line_end: 15,
            }
        );
        assert_eq!(
            lines[2],
            AiLineRef {
                file: "src/lib.rs".into(),
                line_start: 1,
                line_end: 50,
            }
        );
    }

    #[test]
    fn expand_ai_lines_empty_when_no_attestations() {
        let log_text = "---\n{\"schema_version\":\"authorship/3.0.0\",\"base_commit_sha\":\"x\"}\n";
        let log = notes_ai::parse_authorship_log(log_text).unwrap();
        let lines = expand_ai_lines_from_attestations(&log).unwrap();
        assert!(lines.is_empty());
    }

    // ===== stderr_means_no_note_for_sha =====

    #[test]
    fn no_note_for_sha_recognized() {
        assert!(stderr_means_no_note_for_sha(
            "error: no note found for object 1234abcd."
        ));
        assert!(stderr_means_no_note_for_sha(
            "  no note found for object deadbeef  "
        ));
    }

    #[test]
    fn no_note_for_sha_does_not_swallow_real_errors() {
        assert!(!stderr_means_no_note_for_sha("fatal: not a git repository"));
        assert!(!stderr_means_no_note_for_sha(
            "error: bad ref refs/notes/ai"
        ));
        assert!(!stderr_means_no_note_for_sha(""));
    }

    // ===== serde tag 稳定性(前端按 status 分发,不能改名)=====

    #[test]
    fn changed_files_result_serializes_with_status_tag() {
        let r = ChangedFilesResult::Ok {
            files: vec![ChangedFile {
                path: "x".into(),
                status: "M".into(),
                line_stats: Some(FileLineStats {
                    additions: 2,
                    deletions: 1,
                    ai_additions: 1,
                    human_additions: 0,
                    unknown_additions: 1,
                }),
            }],
            is_merge: false,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"status\":\"ok\""));
        assert!(s.contains("\"files\""));
    }

    #[test]
    fn ai_lines_result_degraded_serializes_invalid_sha() {
        let r = AiLinesResult::Degraded {
            reason: DiffDegradedReason::InvalidSha { sha: "abc".into() },
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"status\":\"degraded\""));
        assert!(s.contains("\"kind\":\"invalid_sha\""));
        assert!(s.contains("\"sha\":\"abc\""));
    }
}
