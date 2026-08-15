//! 单 commit 改动文件 + AI 行查询(任务 #2:Dashboard / Commit 详情跳转代码)。
//!
//! 暴露 2 个命令:
//! - `list_changed_files_in_commit(sha)` → 该 commit 的改动文件列表
//!   (Added / Modified / Deleted / Renamed 等 git diff status code 透传)
//! - `list_ai_lines_in_commit(sha)`     → 该 commit 内被 git-ai 标为 AI 的文件 × 行段
//!
//! # 与 stats / notes 的分工
//! - `stats` 只给数字,不给文件 / 行号
//! - `notes_ai::run_show` 解析完整 authorship/3.0.0 log(含 attestations + metadata)
//! - 本模块的 `list_ai_lines_in_commit` 复用 `notes_ai::run_show` 拿到的 attestations
//!   段,只输出 (file, line_start, line_end) 三元组 —— 是上游字段的轻量投影,
//!   不重复解析或反演。
//!
//! # merge commit
//! `git diff-tree -m <sha>` 会对每个 parent 都 diff 一次;同一文件可能出现多次,
//! 这里按 (path, status) 去重保留首次出现的状态。
//!
//! # 错误归类
//! - 未选仓库 → degraded `RepoMissing`
//! - sha 不解析为合法 commit → degraded `InvalidSha`
//! - 没有 AI notes(`refs/notes/ai show <sha>` 失败 / ref 不存在)→ 空数组(初始态,非降级)
//! - 子进程 / 解析硬故障 → Err(String)

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
    Ok { files: Vec<ChangedFile> },
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

    // -m: 对 merge commit 逐 parent 输出;-r: 递归进子目录;--no-commit-id: 头一行不输出 commit sha
    // --name-status: 每行 `<status>\t<path>`(rename/copy 时 status 后还有第二列旧路径,统一在解析层处理)
    let out = run_capture_with_timeout(
        &git,
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-status",
            "-r",
            "-m",
            &sha,
        ],
        Some(&repo_path),
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

    let files = parse_diff_tree_name_status(&out.stdout);
    Ok(ChangedFilesResult::Ok { files })
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

    // 直接调 `git notes --ref=ai show <sha>` 复用 notes_ai 解析路径;
    // - notes ref 不存在 / 该 sha 无 note → 空数组(标准空态,非降级)
    // - 其它子进程异常 / parse 失败 → Err 透传
    let out = run_capture_with_timeout(
        &git,
        &["notes", "--ref", NOTES_REF, "show", &sha],
        Some(&repo_path),
        GIT_TIMEOUT,
    )
    .await
    .map_err(|e| e.to_string())?;
    if out.status != 0 {
        if is_missing_notes_ref(&out.stderr) {
            // notes ref 不存在:仓库尚未启用 git-ai,正常空态
            return Ok(AiLinesResult::Ok { lines: vec![] });
        }
        // 该 sha 没有 note 时上游也会非 0 退出,文案形如
        // "error: no note found for object <sha>." → 视为正常空态
        if stderr_means_no_note_for_sha(&out.stderr) {
            return Ok(AiLinesResult::Ok { lines: vec![] });
        }
        return Err(format!(
            "git notes show {} 退出码 {}: {}",
            sha,
            out.status,
            out.stderr.trim()
        ));
    }

    let log = notes_ai::parse_authorship_log(&out.stdout).map_err(|e| e.to_string())?;
    let lines = expand_ai_lines_from_attestations(&log);
    Ok(AiLinesResult::Ok { lines })
}

// ===== 解析层(纯函数,无 IO,方便单测覆盖)=====

/// 解析 `git diff-tree --name-status -r -m <sha>` 的 stdout。
///
/// 行格式:
/// - 普通改动:`<S>\t<path>`         (S ∈ {A,M,D,T,U,X,B})
/// - rename/copy:`<S><score>\t<old>\t<new>` (S ∈ {R,C},score 是相似度百分比数字)
///
/// 输出:按 (path, status) 去重 —— merge commit `-m` 会对每个 parent 重复输出,
/// 这里只保留首次出现的 status,避免前端列表出现重复行。
pub fn parse_diff_tree_name_status(stdout: &str) -> Vec<ChangedFile> {
    use std::collections::HashSet;
    let mut out: Vec<ChangedFile> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
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
        let key = (path_posix.clone(), status_char.clone());
        if seen.insert(key) {
            out.push(ChangedFile {
                path: path_posix,
                status: status_char,
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
pub fn expand_ai_lines_from_attestations(log: &notes_ai::AuthorshipLog) -> Vec<AiLineRef> {
    let mut out: Vec<AiLineRef> = Vec::new();
    for file in &log.attestations {
        for entry in &file.entries {
            // v3:`h_` 是 known human;`s_<session>::t_<trace>` 与 legacy prompt 都是 AI。
            if entry.hash.starts_with("h_") {
                continue;
            }
            for (start, end) in parse_line_ranges(&entry.line_ranges) {
                out.push(AiLineRef {
                    file: file.file_path.clone(),
                    line_start: start,
                    line_end: end,
                });
            }
        }
    }
    out
}

/// 解析 attestation 的 `line_ranges` 字符串。算法与前端 `parseLineRanges`
/// (`src/lib/types.ts`)严格一致,上游真源:
/// `git-ai/src/authorship/authorship_log_serialization.rs:576-598` `format_line_ranges`。
///
/// 失败语义:任一段格式错 / 起点为 0 / start > end → 整体返空 Vec(no-fallback fail-fast)。
pub fn parse_line_ranges(s: &str) -> Vec<(u32, u32)> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return vec![];
    }
    let mut out = Vec::new();
    for seg in trimmed.split(',') {
        let part = seg.trim();
        if part.is_empty() {
            continue;
        }
        let (a, b) = if let Some(dash) = part.find('-') {
            let lhs = &part[..dash];
            let rhs = &part[dash + 1..];
            match (lhs.parse::<u32>(), rhs.parse::<u32>()) {
                (Ok(x), Ok(y)) => (x, y),
                _ => return vec![],
            }
        } else {
            match part.parse::<u32>() {
                Ok(x) => (x, x),
                _ => return vec![],
            }
        };
        if a < 1 || b < a {
            return vec![];
        }
        out.push((a, b));
    }
    out
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
    fn parse_diff_tree_deduplicates_merge_parent_repeat() {
        // merge commit 加 -m 后,同一文件会出现在每个 parent diff 中
        let s = "M\tsrc/foo.rs\nM\tsrc/foo.rs\nA\tnew.rs\n";
        let v = parse_diff_tree_name_status(s);
        assert_eq!(v.len(), 2, "重复行应去重: {v:?}");
        assert_eq!(v[0].path, "src/foo.rs");
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

    // ===== parse_line_ranges =====

    #[test]
    fn parse_line_ranges_single() {
        assert_eq!(parse_line_ranges("5"), vec![(5, 5)]);
    }

    #[test]
    fn parse_line_ranges_range() {
        assert_eq!(parse_line_ranges("1-10"), vec![(1, 10)]);
    }

    #[test]
    fn parse_line_ranges_multi() {
        assert_eq!(
            parse_line_ranges("5,10-15,20-25"),
            vec![(5, 5), (10, 15), (20, 25)]
        );
    }

    #[test]
    fn parse_line_ranges_empty_and_whitespace() {
        assert_eq!(parse_line_ranges(""), Vec::<(u32, u32)>::new());
        assert_eq!(parse_line_ranges("   "), Vec::<(u32, u32)>::new());
    }

    #[test]
    fn parse_line_ranges_fail_fast() {
        // start > end / 0 起点 / 非数字 → 整体返空(no-fallback)
        assert_eq!(parse_line_ranges("10-5"), Vec::<(u32, u32)>::new());
        assert_eq!(parse_line_ranges("0"), Vec::<(u32, u32)>::new());
        assert_eq!(parse_line_ranges("abc"), Vec::<(u32, u32)>::new());
        assert_eq!(parse_line_ranges("5,bad,10"), Vec::<(u32, u32)>::new());
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
        let lines = expand_ai_lines_from_attestations(&log);
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
        let lines = expand_ai_lines_from_attestations(&log);
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
            }],
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
