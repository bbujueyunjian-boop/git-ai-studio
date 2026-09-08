//! P5 Dashboard 的历史聚合命令。
//!
//! # 总体流程
//! 1. 取当前仓库 path 与 git-ai 二进制(任一不存在 → degraded 空态)
//! 2. `repo::commits::list_recent(max=500)` 拉最近 commit,filter by authored_at >= now - window_days
//! 3. 一次性 `git notes --ref=ai list` 拉全量 sha→oid map(失效判定基础)
//! 4. `db::stats_cache::batch_get` 一次 SQL 读所有 sha 的缓存
//! 5. 对每个 commit:cache.notes_oid == current_oid → 命中;否则进 miss 列表
//! 6. miss 列表用 `buffer_unordered(3)` 并发跑 `git-ai stats <sha> --json`,写回 cache
//! 7. 把所有 commit 按 `authored_at` 的**本地日期**分桶,生成 daily_buckets
//!
//! 注意:range 聚合(`git-ai stats <first>..<last> --json` 的 hook 覆盖率)**不再**由
//! `get_history` 计算 —— 该命令固有耗时 50s+ 且无缓存,曾把整个 `get_history` 拖超时/失败。
//! 现已解耦为独立命令 [`get_range_summary`](带自己的缓存与 180s 超时),前端独立 query 驱动。
//! `get_history` 从此只返 per_commit/daily_buckets 等缓存数据,**永不因 range stats 慢/失败而失败**。
//!
//! # 口径决策
//! - "N 天累计" UI 用 **per-commit 累加视角**(sum of ai_additions),与时间序列分桶自洽
//! - `range_summary.range_stats` 不在 Dashboard 顶部展示(squash 视角,与累加值不同会混淆用户)
//! - `range_summary.authorship_stats` 用于 hook_coverage_rate 卡片(走 `get_range_summary`)
//!
//! # 跨锁约束
//! 整个流程不在持有 `state.db.lock()` 期间再去 `current_repo.read()` —— 入口处先把 repo path
//! clone 出来,db 操作走 `spawn_blocking`,严格遵循 `db/mod.rs` 注释的约定。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use chrono::{
    DateTime, Datelike, Duration as ChronoDuration, Local, NaiveDate, TimeZone, Timelike,
};
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::db::stats_cache;
use crate::error::AppError;
use crate::git_ai;
use crate::repo::commits::{self, CommitBrief};
use crate::state::AppState;

/// per-commit `git-ai stats <sha>` 的并发上限。
///
/// # 取值依据
/// Windows `CreateProcess` 冷启重(单 git-ai 子进程约 100-200ms IO),N 越大冷启总耗时越短:
/// - N=3(历史值):150 commit 冷启 ≈ 25s,默认 30 天窗口必超 15s `STATS_TIMEOUT`
/// - N=8(当前):150 commit 冷启 ≈ 10s,7-30 天典型窗口能压在 15s 内
/// - 再大收益边际(CPU 已上 8+ 核心才扛得住更高并发),且 git-notes IO 会争锁
///
/// 用户冷启缓慢的核心症结:**全表 cache miss × 限流**(详见审查报告)。
const STATS_CONCURRENCY: usize = 8;
/// 单次最多拉的 commit 上限(防 list_recent 返回过多)。
const MAX_COMMITS_HARD_CAP: u32 = 500;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PerCommitStat {
    pub sha: String,
    pub short: String,
    /// ISO-8601 with TZ(`%cI`)。
    pub authored_at: String,
    pub is_merge: bool,
    pub stats: git_ai::stats::AiStats,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct DailyBucket {
    /// 本地时区的 `YYYY-MM-DD`。
    pub date: String,
    pub human_additions: u64,
    pub unknown_additions: u64,
    pub ai_additions: u64,
    pub commit_count: u32,
}

/// 时间范围筛选维度。前后端镜像;serde tagged enum 让前端 union 类型自然。
///
/// 周第一天 = 周一(ISO / 国内习惯)。
/// `today` / `this_week` / `this_month` 的 end 是**当前时刻**(实时,不是当日 23:59)。
/// `yesterday` / `last_week` / `last_month` 的 end 是**当日 23:59:59.999999999**。
/// `last_n_days` 维持滑动窗口语义:end = now,start = now - N 天(向后兼容旧 windowDays 7/30/90)。
/// `custom` 接受任意 unix_ms 范围;上限由前端校验。
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TimeRange {
    /// 默认 Today,对齐前端 `Dashboard.tsx:55` 的 DEFAULT_RANGE = { kind: "today" }。
    /// Today 窗口冷启最快(通常 <10 commit),首次安装也能在 1-2s 内出数据。
    #[default]
    Today,
    Yesterday,
    ThisWeek,
    LastWeek,
    ThisMonth,
    LastMonth,
    LastNDays {
        days: u32,
    },
    Custom {
        start_unix_ms: i64,
        end_unix_ms: i64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct HistoryPayload {
    /// 本次查询的时间范围(echo 回前端,便于 UI 显示当前选中态)。
    pub range: TimeRange,
    /// 范围 start 的本地时刻 unix_ms(给前端 chart X 轴 domain 用)。
    pub range_start_unix_ms: i64,
    /// 范围 end 的本地时刻 unix_ms(给前端 chart X 轴 domain 用)。
    pub range_end_unix_ms: i64,
    pub total_commits_in_window: u32,
    pub per_commit: Vec<PerCommitStat>,
    pub daily_buckets: Vec<DailyBucket>,
    /// 本次 fetch 中,命中 SQLite cache 的 commit 数(用于 UI 透出"x / N 命中缓存")。
    pub cache_hits: u32,
    /// 数据库里该 repo 总共已缓存的 commit 数(用于 Settings 显示)。
    pub cached_repo_total: u64,
    /// `git-ai stats <sha>` 子进程失败的 commit sha 列表 —— 这些 commit 在 per_commit
    /// 里以 0 桶占位,**前端必须显式提示用户有 N 条数据采集失败**,而不是把 0 当真实数据。
    pub failed_shas: Vec<String>,
    /// `list_recent(500)` 取到刚好 500 条 commit ⇒ 仓库可能有更老 commit 未参与本窗口计算。
    /// 前端要显式提示(评审 P5 #24:不能让 UI 静默漏算)。
    pub truncated: bool,
    /// 数据耗时(ms),包含子进程与 db 时间。
    pub took_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum HistoryResult {
    Ok { payload: HistoryPayload },
    Degraded { reason: DegradedReason },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DegradedReason {
    RepoMissing,
    GitAiMissing,
}

/// `get_range_summary` 的返回。Ok 携带 range 聚合;空态走 Degraded;真失败走 `Err(String)`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum RangeSummaryResult {
    Ok {
        range_summary: git_ai::stats::RangeAuthorshipStats,
    },
    Degraded {
        reason: RangeSummaryDegradedReason,
    },
}

/// range 聚合的预期内空态。比 [`DegradedReason`] 多一个 `EmptyWindow`(选中时间范围内无 commit,
/// 无从推导 start/end),前端只需渲染"该卡无数据",不弹红 toast。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RangeSummaryDegradedReason {
    RepoMissing,
    GitAiMissing,
    EmptyWindow,
}

#[tauri::command]
pub async fn get_history(
    range: TimeRange,
    state: State<'_, AppState>,
) -> Result<HistoryResult, String> {
    let started = Instant::now();
    // 时间维度约定:`now` / `range_start` / `range_end` / `bucket_by_local_date` 全部使用
    // **本机本地时区**(`Local::now()`)。commit 自带 ISO-8601 + TZ,但分桶时按本地日期归并。
    //
    // 跨时区团队的"今天"会与作者机的"今天"差最多 24h,daily_buckets 的 X 轴标签可能与
    // 别人记忆中的日期对不齐 — 这是显式选择,优先服务"本地用户视角"。窗口聚合总数不受
    // 影响(同一窗口内 commit 集合相同)。
    let now = Local::now();
    let (range_start, range_end) = time_range_bounds(&range, now);
    let range_start_unix_ms = range_start.timestamp_millis();
    let range_end_unix_ms = range_end.timestamp_millis();
    // 1. 取仓库 path(clone 出 RwLock,后续不再持锁)
    let repo_path: String = {
        let g = state
            .current_repo
            .read()
            .map_err(|_| "current_repo 锁中毒".to_string())?;
        match g.as_ref() {
            Some(r) => r.path.clone(),
            None => {
                return Ok(HistoryResult::Degraded {
                    reason: DegradedReason::RepoMissing,
                });
            }
        }
    };
    let repo_path_buf = PathBuf::from(&repo_path);

    // 2. git-ai 二进制
    let git_ai_bin = match git_ai::binary::resolve() {
        Ok(p) => p,
        Err(_) => {
            return Ok(HistoryResult::Degraded {
                reason: DegradedReason::GitAiMissing,
            });
        }
    };

    // 3. list_recent + window filter
    let recent = commits::list_recent(&repo_path_buf, MAX_COMMITS_HARD_CAP)
        .await
        .map_err(|e| e.to_string())?;
    let truncated = is_window_truncated(&recent, MAX_COMMITS_HARD_CAP as usize, range_start);
    let window = filter_by_range(&recent, &range, now);

    if window.is_empty() {
        return Ok(HistoryResult::Ok {
            payload: HistoryPayload {
                range: range.clone(),
                range_start_unix_ms,
                range_end_unix_ms,
                total_commits_in_window: 0,
                per_commit: vec![],
                daily_buckets: vec![],
                cache_hits: 0,
                cached_repo_total: read_cached_total(&state, &repo_path)?,
                failed_shas: vec![],
                truncated,
                took_ms: started.elapsed().as_millis() as u64,
            },
        });
    }

    // 4-8. 解析 window 内各 commit 的 stats(notes oid + ignore hash + cache 命中 + miss 并发 +
    // 写回)。抽到 resolve_window_stats 与 get_aggregate_history 共用,避免复制缓存/并发/写回逻辑。
    let shas: Vec<String> = window.iter().map(|c| c.sha.clone()).collect();
    let (hits, cache_hits, failed_shas) =
        resolve_window_stats(&repo_path, &shas, &git_ai_bin, &state).await?;

    // 9. 组装 per_commit + 按本地日期分桶
    let per_commit: Vec<PerCommitStat> = window
        .iter()
        .map(|c| PerCommitStat {
            sha: c.sha.clone(),
            short: c.short.clone(),
            authored_at: c.authored_at.clone(),
            is_merge: c.is_merge,
            stats: hits.get(&c.sha).cloned().unwrap_or_default(),
        })
        .collect();
    let daily_buckets = bucket_by_local_date(&per_commit);

    // range 聚合(hook 覆盖率)已解耦到独立的 `get_range_summary` 命令,这里不再计算 —
    // 详见模块头注释:它固有耗时长且无缓存,曾把整个 get_history 拖超时。

    let cached_repo_total = read_cached_total(&state, &repo_path)?;

    Ok(HistoryResult::Ok {
        payload: HistoryPayload {
            range,
            range_start_unix_ms,
            range_end_unix_ms,
            total_commits_in_window: per_commit.len() as u32,
            per_commit,
            daily_buckets,
            cache_hits,
            cached_repo_total,
            failed_shas,
            truncated,
            took_ms: started.elapsed().as_millis() as u64,
        },
    })
}

/// 解析一个 window(已过滤的 commit 集)内各 commit 的 stats:
/// notes oid map + `.git-ai-ignore` hash → batch_get 缓存 → split 命中/miss →
/// miss 用 `buffer_unordered(STATS_CONCURRENCY)` 并发跑 `git-ai stats <sha>` → 写回 cache。
///
/// 返回 `(sha→stats 命中表, 缓存命中数, 采集失败的 sha)`。`get_history`(单仓)与
/// `get_aggregate_history`(跨仓,按仓循环调用)共用,避免两处复制缓存/并发/写回逻辑。
/// 失败的 sha **不**进命中表(调用方以 0 桶占位 + 显式 failed 提示,绝不当真实数据)。
async fn resolve_window_stats(
    repo_path: &str,
    shas: &[String],
    git_ai_bin: &Path,
    state: &AppState,
) -> Result<(HashMap<String, git_ai::stats::AiStats>, u32, Vec<String>), String> {
    let repo_path_buf = PathBuf::from(repo_path);

    let oid_map = git_ai::notes::read_all_notes_oids(&repo_path_buf)
        .await
        .map_err(|e| e.to_string())?;
    let current_ignore_hash =
        git_ai::ignore::compute_ignore_hash(&repo_path_buf).map_err(|e| e.to_string())?;

    let cached_map = {
        let conn_arc = state.db.clone();
        let repo_clone = repo_path.to_string();
        let shas_clone = shas.to_vec();
        tokio::task::spawn_blocking(move || {
            let conn = conn_arc
                .lock()
                .map_err(|_| AppError::Other("db 锁中毒".into()))?;
            stats_cache::batch_get(&conn, &repo_clone, &shas_clone)
        })
        .await
        .map_err(|e| format!("spawn_blocking 失败: {e}"))?
        .map_err(|e| e.to_string())?
    };

    let (mut hits, miss_shas) =
        split_hits_and_misses(shas, &oid_map, &current_ignore_hash, &cached_map);

    let miss_results: Vec<(String, Result<git_ai::stats::AiStats, AppError>)> =
        stream::iter(miss_shas.clone())
            .map(|sha| {
                let bin = git_ai_bin.to_path_buf();
                let rp = repo_path_buf.clone();
                async move {
                    let stats = git_ai::stats::run_stats(&bin, &rp, Some(&sha)).await;
                    (sha, stats)
                }
            })
            .buffer_unordered(STATS_CONCURRENCY)
            .collect()
            .await;

    let mut put_buf: Vec<(String, String, git_ai::stats::AiStats)> = Vec::new();
    let mut failed_shas: Vec<String> = Vec::new();
    for (sha, res) in miss_results {
        match res {
            Ok(stats) => {
                let oid = oid_map.get(&sha).cloned().unwrap_or_default();
                hits.insert(sha.clone(), stats.clone());
                put_buf.push((sha, oid, stats));
            }
            Err(e) => {
                log::warn!("git-ai stats {} 失败: {}", sha, e);
                failed_shas.push(sha);
            }
        }
    }
    let cache_hits = (shas.len() - miss_shas.len()) as u32;

    if !put_buf.is_empty() {
        let conn_arc = state.db.clone();
        let repo_clone = repo_path.to_string();
        let ignore_hash_clone = current_ignore_hash.clone();
        tokio::task::spawn_blocking(move || -> Result<(), AppError> {
            let mut conn = conn_arc
                .lock()
                .map_err(|_| AppError::Other("db 锁中毒".into()))?;
            let tx = conn
                .transaction()
                .map_err(|e| AppError::Other(format!("tx begin: {e}")))?;
            for (sha, oid, stats) in &put_buf {
                stats_cache::put(&tx, &repo_clone, sha, oid, &ignore_hash_clone, stats)?;
            }
            tx.commit()
                .map_err(|e| AppError::Other(format!("tx commit: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| format!("spawn_blocking 写 cache 失败: {e}"))?
        .map_err(|e| e.to_string())?;
    }

    Ok((hits, cache_hits, failed_shas))
}

// ============ 提交归因(Stats 页):单仓 commit 浏览器的数据源 ============
// 与 get_history(时间窗口聚合 + 分桶,Dashboard 用)正交:这里只要"最近 N 个 commit 各自的
// AI 三桶 + 作者/标题元信息"的扁平列表,供 Stats 页 commit 列表 + 详情渲染。复用 resolve_window_stats
// 的缓存/并发/写回,不重造 git-ai 子进程逻辑。读 current_repo(下钻焦点),与跨仓聚合无关。

/// 单个 commit 的元信息 + AI 三桶(commit 浏览器每行的数据)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CommitWithStats {
    pub sha: String,
    pub short: String,
    /// ISO-8601 with TZ。
    pub authored_at: String,
    pub author_name: String,
    pub author_email: String,
    pub subject: String,
    pub is_merge: bool,
    pub stats: git_ai::stats::AiStats,
    /// 提示性 note(merge / 空新增 / 缺 hook),复用 stats.rs::derive_note_kind 单一口径,前端渲染注脚条。
    pub note_kind: Option<crate::commands::stats::NoteKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RecentCommitsPayload {
    pub commits: Vec<CommitWithStats>,
    /// `git-ai stats <sha>` 失败的 commit:在 commits 里以 0 桶占位,前端必须显式提示,绝不当真实数据。
    pub failed_shas: Vec<String>,
    /// `list_recent` 取到刚好 max_count 条 ⇒ 仓库可能有更老 commit 未纳入,前端显式提示。
    pub truncated: bool,
    /// 命中 SQLite cache 的 commit 数(UI 可透出"x / N 命中缓存")。
    pub cache_hits: u32,
    pub took_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum RecentCommitsResult {
    Ok { payload: RecentCommitsPayload },
    Degraded { reason: DegradedReason },
}

/// 最近 N 个 commit + 各自 AI 三桶(单仓提交归因浏览器数据源)。
///
/// 流程:`list_recent(max_count)` → `resolve_window_stats`(缓存命中/miss 并发/写回)→ 组装。
/// 预期空态(未选仓库 / git-ai 未装)→ `Ok(Degraded)`;真失败 → `Err(String)`。失败的单 commit
/// 进 `failed_shas`、在 commits 里以 0 桶占位,绝不当真实数据(响亮失败)。
#[tauri::command]
pub async fn list_recent_commits_with_stats(
    max_count: u32,
    state: State<'_, AppState>,
) -> Result<RecentCommitsResult, String> {
    let started = Instant::now();
    let repo_path: String = {
        let g = state
            .current_repo
            .read()
            .map_err(|_| "current_repo 锁中毒".to_string())?;
        match g.as_ref() {
            Some(r) => r.path.clone(),
            None => {
                return Ok(RecentCommitsResult::Degraded {
                    reason: DegradedReason::RepoMissing,
                });
            }
        }
    };
    let repo_path_buf = PathBuf::from(&repo_path);

    let git_ai_bin = match git_ai::binary::resolve() {
        Ok(p) => p,
        Err(_) => {
            return Ok(RecentCommitsResult::Degraded {
                reason: DegradedReason::GitAiMissing,
            });
        }
    };

    let cap = max_count.clamp(1, MAX_COMMITS_HARD_CAP);
    let recent = commits::list_recent(&repo_path_buf, cap)
        .await
        .map_err(|e| e.to_string())?;
    // 取到刚好 cap 条 ⇒ 可能还有更老的没拿到。
    let truncated = recent.len() == cap as usize;

    if recent.is_empty() {
        return Ok(RecentCommitsResult::Ok {
            payload: RecentCommitsPayload {
                commits: vec![],
                failed_shas: vec![],
                truncated,
                cache_hits: 0,
                took_ms: started.elapsed().as_millis() as u64,
            },
        });
    }

    let shas: Vec<String> = recent.iter().map(|c| c.sha.clone()).collect();
    let (hits, cache_hits, failed_shas) =
        resolve_window_stats(&repo_path, &shas, &git_ai_bin, &state).await?;

    let commits: Vec<CommitWithStats> = recent
        .iter()
        .map(|c| {
            let stats = hits.get(&c.sha).cloned().unwrap_or_default();
            let total = stats.total_additions();
            let note_kind = crate::commands::stats::derive_note_kind(&stats, total, c.is_merge);
            CommitWithStats {
                sha: c.sha.clone(),
                short: c.short.clone(),
                authored_at: c.authored_at.clone(),
                author_name: c.author_name.clone(),
                author_email: c.author_email.clone(),
                subject: c.subject.clone(),
                is_merge: c.is_merge,
                stats,
                note_kind,
            }
        })
        .collect();

    Ok(RecentCommitsResult::Ok {
        payload: RecentCommitsPayload {
            commits,
            failed_shas,
            truncated,
            cache_hits,
            took_ms: started.elapsed().as_millis() as u64,
        },
    })
}

// ============ 跨仓聚合(M2):Dashboard 默认跨仓的数据源 ============
// 与单仓 get_history 正交:get_history 读 current_repo(下钻焦点),get_aggregate_history 读
// AppSettings.aggregate_repos(用户显式勾选集)。单仓下钻命令保持不动,LowAiShare 等不受影响。

/// 跨仓 per-commit:带 repo_path 以支持跨仓识别与下钻。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AggregatePerCommit {
    pub repo_path: String,
    pub sha: String,
    pub short: String,
    pub authored_at: String,
    pub is_merge: bool,
    pub stats: git_ai::stats::AiStats,
}

/// 整仓采集失败(list_recent / notes / cache 等)。前端必须显式列出"未纳入统计",
/// 其数据**绝不**当 0 并入聚合(响亮失败)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct FailedRepo {
    pub repo_path: String,
    pub reason: String,
}

/// 单 commit 采集失败(`git-ai stats` 子进程失败)。带 repo_path 限定:跨仓 sha 不全局唯一。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct FailedCommit {
    pub repo_path: String,
    pub sha: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AggregateHistoryPayload {
    pub range: TimeRange,
    pub range_start_unix_ms: i64,
    pub range_end_unix_ms: i64,
    pub total_commits_in_window: u32,
    pub per_commit: Vec<AggregatePerCommit>,
    pub daily_buckets: Vec<DailyBucket>,
    pub cache_hits: u32,
    /// 采集失败的仓(数据未并入聚合,前端必须显式提示)。
    pub failed_repos: Vec<FailedRepo>,
    /// 单 commit 采集失败(0 桶占位,前端必须显式提示)。
    pub failed_shas: Vec<FailedCommit>,
    /// 命中 500 cap 的仓(可能有更老 commit 未计入,前端显式提示)。
    pub truncated_repos: Vec<String>,
    pub took_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum AggregateHistoryResult {
    Ok { payload: AggregateHistoryPayload },
    Degraded { reason: AggregateDegradedReason },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AggregateDegradedReason {
    /// `aggregate_repos` 为空(用户尚未勾选任何仓库)—— 预期空态,前端引导去勾选。
    NoReposSelected,
    /// git-ai 二进制缺失(全局前置,所有仓都做不了)。
    GitAiMissing,
}

/// 单仓在聚合里的切片产物。
struct AggregateRepoSlice {
    per_commit: Vec<AggregatePerCommit>,
    cache_hits: u32,
    truncated: bool,
    failed_shas: Vec<String>,
}

/// 采集单个仓库在 range 窗口内的 per-commit 切片(list_recent → filter → 可选作者过滤 →
/// resolve_window_stats)。整仓级失败返回 `Err(reason)`,由调用方记入 failed_repos、不拖垮其余仓。
///
/// `only_mine_email`:`Some(email)` 时只保留 author_email 命中该邮箱的 commit(「只看我」口径);
/// `None` 时不按作者过滤(全作者聚合)。
async fn gather_aggregate_repo(
    repo_path: &str,
    range: &TimeRange,
    now: DateTime<Local>,
    git_ai_bin: &Path,
    only_mine_email: Option<&str>,
    state: &AppState,
) -> Result<AggregateRepoSlice, String> {
    let repo_path_buf = PathBuf::from(repo_path);
    let recent = commits::list_recent(&repo_path_buf, MAX_COMMITS_HARD_CAP)
        .await
        .map_err(|e| e.to_string())?;
    let range_start = time_range_bounds(range, now).0;
    let truncated = is_window_truncated(&recent, MAX_COMMITS_HARD_CAP as usize, range_start);
    let in_range = filter_by_range(&recent, range, now);
    let window = match only_mine_email {
        Some(email) => filter_by_author(&in_range, email),
        None => in_range,
    };
    if window.is_empty() {
        return Ok(AggregateRepoSlice {
            per_commit: vec![],
            cache_hits: 0,
            truncated,
            failed_shas: vec![],
        });
    }
    let shas: Vec<String> = window.iter().map(|c| c.sha.clone()).collect();
    let (hits, cache_hits, failed_shas) =
        resolve_window_stats(repo_path, &shas, git_ai_bin, state).await?;
    let per_commit = window
        .iter()
        .map(|c| AggregatePerCommit {
            repo_path: repo_path.to_string(),
            sha: c.sha.clone(),
            short: c.short.clone(),
            authored_at: c.authored_at.clone(),
            is_merge: c.is_merge,
            stats: hits.get(&c.sha).cloned().unwrap_or_default(),
        })
        .collect();
    Ok(AggregateRepoSlice {
        per_commit,
        cache_hits,
        truncated,
        failed_shas,
    })
}

/// 跨仓聚合历史:Dashboard 默认视图的数据源。读 `aggregate_repos`,按仓**顺序**采集
/// (每仓内部 per-commit `buffer_unordered(8)`,仓间串行 ⇒ 全局最多 8 子进程、单层 fan-out
/// 无死锁;跨仓并行留作后续优化),合并 per_commit 后按本地日期分桶。
///
/// 失败三分:空集 → `Degraded(NoReposSelected)`;全仓失败 → `Err`;部分仓失败 →
/// `Ok` + `failed_repos`(成功仓正常聚合,失败仓显式列出、绝不当 0 并入)。
///
/// `only_mine`(默认前端传 true):「只看我」口径 —— 逐仓解析该仓生效的 git user.email,只统计
/// 命中的 commit。某仓未配置 user.email 时无法确定「我」,该仓显式记入 failed_repos(响亮失败,
/// 绝不退化成全作者聚合)。
#[tauri::command]
pub async fn get_aggregate_history(
    range: TimeRange,
    only_mine: bool,
    state: State<'_, AppState>,
) -> Result<AggregateHistoryResult, String> {
    let started = Instant::now();
    let now = Local::now();
    let (range_start, range_end) = time_range_bounds(&range, now);

    let repos = crate::state::AppSettings::load()?.aggregate_repos;
    if repos.is_empty() {
        return Ok(AggregateHistoryResult::Degraded {
            reason: AggregateDegradedReason::NoReposSelected,
        });
    }

    let git_ai_bin = match git_ai::binary::resolve() {
        Ok(p) => p,
        Err(_) => {
            return Ok(AggregateHistoryResult::Degraded {
                reason: AggregateDegradedReason::GitAiMissing,
            });
        }
    };

    let mut all_per_commit: Vec<AggregatePerCommit> = Vec::new();
    let mut failed_repos: Vec<FailedRepo> = Vec::new();
    let mut failed_shas: Vec<FailedCommit> = Vec::new();
    let mut truncated_repos: Vec<String> = Vec::new();
    let mut cache_hits: u32 = 0;

    for repo_path in &repos {
        // 「只看我」:逐仓解析生效的 git user.email。该仓未配置(local/global 皆无)⇒ 无法确定
        // 「我」是谁,显式记 failed_repo 并跳过,绝不退化成全作者(响亮失败)。
        let only_mine_email = if only_mine {
            match crate::commands::repo::read_git_user_email(&PathBuf::from(repo_path)).await {
                Some(email) => Some(email),
                None => {
                    failed_repos.push(FailedRepo {
                        repo_path: repo_path.clone(),
                        reason: "「只看我」口径下该仓未配置 git user.email,无法确定当前用户,已排除"
                            .to_string(),
                    });
                    continue;
                }
            }
        } else {
            None
        };
        match gather_aggregate_repo(
            repo_path,
            &range,
            now,
            &git_ai_bin,
            only_mine_email.as_deref(),
            &state,
        )
        .await
        {
            Ok(slice) => {
                cache_hits += slice.cache_hits;
                if slice.truncated {
                    truncated_repos.push(repo_path.clone());
                }
                for sha in slice.failed_shas {
                    failed_shas.push(FailedCommit {
                        repo_path: repo_path.clone(),
                        sha,
                    });
                }
                all_per_commit.extend(slice.per_commit);
            }
            Err(reason) => {
                log::warn!("聚合仓库 {} 采集失败: {}", repo_path, reason);
                failed_repos.push(FailedRepo {
                    repo_path: repo_path.clone(),
                    reason,
                });
            }
        }
    }

    // 全部仓都失败 ⇒ 无任何可信数据,响亮失败(Err)。注意区别于"窗口空":窗口空时各仓
    // gather 返回 Ok(空切片)、failed_repos 为空,走下面的 Ok(空聚合)。
    if failed_repos.len() == repos.len() {
        return Err(format!(
            "全部 {} 个聚合仓库采集失败,无可用数据(示例:{})",
            repos.len(),
            failed_repos
                .first()
                .map(|f| f.reason.as_str())
                .unwrap_or("")
        ));
    }

    // 跨仓合并后全局按 authored_at 倒序:各仓内部已倒序,但仓间是顺序拼接 —— 不全局排序会让
    // 前端"最近 commit"(slice 头 N 条)只取到第一个仓的,漏掉其它仓更新的 commit(红队 M4#5)。
    // 解析失败的 authored_at 排到末尾(罕见)。
    all_per_commit.sort_by(|a, b| {
        let pa = DateTime::parse_from_rfc3339(&a.authored_at).ok();
        let pb = DateTime::parse_from_rfc3339(&b.authored_at).ok();
        pb.cmp(&pa)
    });

    // 合并所有仓 per_commit 后按本地日期分桶:纯按行加法(saturating_add),**不预算率**,
    // AI 占比由前端按 sum 后再算,避免"各仓先算率再平均"。
    let merged: Vec<PerCommitStat> = all_per_commit
        .iter()
        .map(|c| PerCommitStat {
            sha: c.sha.clone(),
            short: c.short.clone(),
            authored_at: c.authored_at.clone(),
            is_merge: c.is_merge,
            stats: c.stats.clone(),
        })
        .collect();
    let daily_buckets = bucket_by_local_date(&merged);

    Ok(AggregateHistoryResult::Ok {
        payload: AggregateHistoryPayload {
            range,
            range_start_unix_ms: range_start.timestamp_millis(),
            range_end_unix_ms: range_end.timestamp_millis(),
            total_commits_in_window: all_per_commit.len() as u32,
            per_commit: all_per_commit,
            daily_buckets,
            cache_hits,
            failed_repos,
            failed_shas,
            truncated_repos,
            took_ms: started.elapsed().as_millis() as u64,
        },
    })
}

// ============ 跨仓「本地未提交」聚合:Dashboard 的独立快照卡数据源 ============
// 与时间窗口的"已提交"聚合正交:这是**当前**工作树未提交改动(`git ai status`)的跨仓求和,
// 没有 commit 时间、不进时间序列、不缓存(工作树易变),前端用独立卡呈现、只在窗口含"现在"时显示。

/// 单仓的未提交切片(三桶)。只收录有改动(total>0)的仓,供卡片列出/排序。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct WorkingRepoSlice {
    pub repo_path: String,
    pub human_additions: u64,
    pub unknown_additions: u64,
    pub ai_additions: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AggregateWorkingStatusPayload {
    /// 有未提交改动(三桶 total>0)的仓库数。
    pub repos_with_changes: u32,
    pub human_additions: u64,
    pub unknown_additions: u64,
    pub ai_additions: u64,
    /// 仅含有改动的仓,按 ai_additions 倒序。
    pub per_repo: Vec<WorkingRepoSlice>,
    /// 读取 `git ai status` 失败的仓(无 HEAD / 子进程错误等),显式列出,绝不当 0 并入。
    pub failed_repos: Vec<FailedRepo>,
    pub took_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum AggregateWorkingStatusResult {
    Ok {
        payload: AggregateWorkingStatusPayload,
    },
    Degraded {
        reason: AggregateDegradedReason,
    },
}

/// 跨仓「本地未提交」聚合:对 `aggregate_repos` 逐仓跑 `git ai status --json`,把三桶求和。
///
/// 与 `get_aggregate_history` 同样的失败三分:空集 → `Degraded(NoReposSelected)`;全仓失败 →
/// `Err`;部分失败 → `Ok` + `failed_repos`。工作树易变 ⇒ **不缓存**,每次调用现读;`git ai status`
/// 只看工作树,单仓很快(共享 15s `STATS_TIMEOUT`)。天然"只看我"(就是本机当前未提交改动),
/// 不受 Dashboard 的 only_mine 开关影响。
#[tauri::command]
pub async fn get_aggregate_working_status(
    state: State<'_, AppState>,
) -> Result<AggregateWorkingStatusResult, String> {
    let started = Instant::now();
    let _ = &state; // 不读 current_repo / db:聚合集来自 settings,工作树状态不缓存

    let repos = crate::state::AppSettings::load()?.aggregate_repos;
    if repos.is_empty() {
        return Ok(AggregateWorkingStatusResult::Degraded {
            reason: AggregateDegradedReason::NoReposSelected,
        });
    }
    let git_ai_bin = match git_ai::binary::resolve() {
        Ok(p) => p,
        Err(_) => {
            return Ok(AggregateWorkingStatusResult::Degraded {
                reason: AggregateDegradedReason::GitAiMissing,
            });
        }
    };

    let mut human_additions: u64 = 0;
    let mut unknown_additions: u64 = 0;
    let mut ai_additions: u64 = 0;
    let mut per_repo: Vec<WorkingRepoSlice> = Vec::new();
    let mut failed_repos: Vec<FailedRepo> = Vec::new();

    for repo_path in &repos {
        match git_ai::stats::run_status(&git_ai_bin, &PathBuf::from(repo_path)).await {
            Ok(status) => {
                let st = &status.stats;
                human_additions = human_additions.saturating_add(st.human_additions);
                unknown_additions = unknown_additions.saturating_add(st.unknown_additions);
                ai_additions = ai_additions.saturating_add(st.ai_additions);
                if st.total_additions() > 0 {
                    per_repo.push(WorkingRepoSlice {
                        repo_path: repo_path.clone(),
                        human_additions: st.human_additions,
                        unknown_additions: st.unknown_additions,
                        ai_additions: st.ai_additions,
                    });
                }
            }
            Err(e) => {
                log::warn!("聚合仓库 {} 读取未提交状态失败: {}", repo_path, e);
                failed_repos.push(FailedRepo {
                    repo_path: repo_path.clone(),
                    reason: e.to_string(),
                });
            }
        }
    }

    if failed_repos.len() == repos.len() {
        return Err(format!(
            "全部 {} 个聚合仓库读取未提交状态失败(示例:{})",
            repos.len(),
            failed_repos
                .first()
                .map(|f| f.reason.as_str())
                .unwrap_or("")
        ));
    }

    per_repo.sort_by_key(|s| std::cmp::Reverse(s.ai_additions));

    Ok(AggregateWorkingStatusResult::Ok {
        payload: AggregateWorkingStatusPayload {
            repos_with_changes: per_repo.len() as u32,
            human_additions,
            unknown_additions,
            ai_additions,
            per_repo,
            failed_repos,
            took_ms: started.elapsed().as_millis() as u64,
        },
    })
}

/// range 聚合的窗口边界:`start..end`。
/// - `start` = 窗口最旧 commit 的 `^`(有 parent)或 [`git_ai::stats::EMPTY_TREE_HASH`](无 parent,即根 commit)
/// - `end` = 窗口最新 commit 的 sha
///
/// 与 git-ai 上游 `range_authorship.rs:224` / `repository.rs:375 CommitRange::is_valid` 对
/// EMPTY_TREE_HASH 的特判一致:这让"只有 1 个 commit 的新仓库 / 窗口覆盖整段历史"也能正常聚合。
pub struct RangeWindow {
    pub start: String,
    pub end: String,
}

/// 由 [`TimeRange`] 推导 range 聚合的窗口边界(`get_history` 与 `get_range_summary` 共用同一推导,
/// 避免两处各写一份漂移)。
///
/// 流程:`list_recent(cap)` → `filter_by_range` → 取窗口最旧/最新 → `has_parent` 决定 start。
/// 窗口为空 → `Ok(None)`(调用方据此返回 EmptyWindow 空态)。
async fn derive_range_window(
    repo_path: &Path,
    range: &TimeRange,
    now: DateTime<Local>,
) -> Result<Option<RangeWindow>, String> {
    let recent = commits::list_recent(repo_path, MAX_COMMITS_HARD_CAP)
        .await
        .map_err(|e| e.to_string())?;
    let window = filter_by_range(&recent, range, now);
    if window.is_empty() {
        return Ok(None);
    }
    // list_recent 默认按时间倒序:末尾为最旧、首位为最新。
    let first = &window[window.len() - 1].sha;
    let last = &window[0].sha;
    let start = match commits::has_parent(repo_path, first).await {
        Ok(true) => format!("{first}^"),
        Ok(false) => git_ai::stats::EMPTY_TREE_HASH.to_string(),
        Err(e) => {
            return Err(classify_range_summary_error(
                "解析窗口起点 commit 失败",
                &AppError::Other(e.to_string()),
                Some(first),
            ));
        }
    };
    Ok(Some(RangeWindow {
        start,
        end: last.clone(),
    }))
}

/// 独立的 range 聚合命令(从 `get_history` 第 10 步解耦而来)。
///
/// # 为什么独立
/// `git-ai stats <start>..<end> --json` 对整段做行级 blame(squash 视角,`range_authorship.rs:119,131-135`),
/// 大/长历史仓库固有耗时 50s+ 且**无 per-commit 缓存**。塞在 `get_history` 里会把秒回的
/// per-commit 数据一起拖到超时/失败。拆开后:Dashboard 主体走 `get_history` 立即渲染,
/// hook 覆盖率卡走本命令的独立 query(自带 loading / error / degraded)。
///
/// # 缓存(`db::stats_cache::range`)
/// 缓存键 = (repo, start_sha, end_sha);失效维度 = (`refs/notes/ai` 的 ref OID, `.git-ai-ignore` 哈希)。
/// 命中且失效维度一致 → 直接返回缓存;否则跑 `run_range_stats`(180s 超时)成功后写缓存再返回。
///
/// # 失败语义
/// 预期空态(未选仓库 / git-ai 未装 / 窗口为空)→ `Ok(Degraded)`;真失败 → `Err(String)`,
/// 由 `classify_range_summary_error` 翻译为"原因 + 建议"。
#[tauri::command]
pub async fn get_range_summary(
    range: TimeRange,
    state: State<'_, AppState>,
) -> Result<RangeSummaryResult, String> {
    let now = Local::now();

    // 1. 仓库 path(clone 出锁,后续不再持锁)
    let repo_path: String = {
        let g = state
            .current_repo
            .read()
            .map_err(|_| "current_repo 锁中毒".to_string())?;
        match g.as_ref() {
            Some(r) => r.path.clone(),
            None => {
                return Ok(RangeSummaryResult::Degraded {
                    reason: RangeSummaryDegradedReason::RepoMissing,
                });
            }
        }
    };
    let repo_path_buf = PathBuf::from(&repo_path);

    // 2. git-ai 二进制
    let git_ai_bin = match git_ai::binary::resolve() {
        Ok(p) => p,
        Err(_) => {
            return Ok(RangeSummaryResult::Degraded {
                reason: RangeSummaryDegradedReason::GitAiMissing,
            });
        }
    };

    // 3. 推导窗口边界(与 get_history 共用)。空窗口 → EmptyWindow 空态。
    let window = match derive_range_window(&repo_path_buf, &range, now).await? {
        Some(w) => w,
        None => {
            return Ok(RangeSummaryResult::Degraded {
                reason: RangeSummaryDegradedReason::EmptyWindow,
            });
        }
    };

    // 4. 算当前失效维度:refs/notes/ai 的 ref OID + .git-ai-ignore 哈希(复用 get_history 同源 helper)
    let notes_ref_oid = git_ai::notes::read_notes_ref_oid(&repo_path_buf)
        .await
        .map_err(|e| classify_range_summary_error("读取 refs/notes/ai 状态失败", &e, None))?
        .unwrap_or_default();
    let ignore_hash =
        git_ai::ignore::compute_ignore_hash(&repo_path_buf).map_err(|e| e.to_string())?;

    // 5. 查缓存:命中且 (notes_ref_oid, ignore_hash) 一致 → 直接返回
    {
        let conn_arc = state.db.clone();
        let repo_clone = repo_path.clone();
        let start_clone = window.start.clone();
        let end_clone = window.end.clone();
        let cached = tokio::task::spawn_blocking(move || {
            let conn = conn_arc
                .lock()
                .map_err(|_| AppError::Other("db 锁中毒".into()))?;
            stats_cache::range::get(&conn, &repo_clone, &start_clone, &end_clone)
        })
        .await
        .map_err(|e| format!("spawn_blocking 失败: {e}"))?
        .map_err(|e| e.to_string())?;
        if let Some(c) = cached {
            if c.notes_ref_oid == notes_ref_oid && c.ignore_hash == ignore_hash {
                return Ok(RangeSummaryResult::Ok {
                    range_summary: c.summary,
                });
            }
        }
    }

    // 6. miss:跑 run_range_stats(180s 超时),失败 → Err(翻译后的字符串)
    let summary = match git_ai::stats::run_range_stats(
        &git_ai_bin,
        &repo_path_buf,
        &window.start,
        &window.end,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            return Err(classify_range_summary_error(
                "采集范围聚合指标(hook 覆盖率)失败",
                &e,
                None,
            ));
        }
    };

    // 7. 写回缓存(单条 upsert)
    {
        let conn_arc = state.db.clone();
        let repo_clone = repo_path.clone();
        let start_clone = window.start.clone();
        let end_clone = window.end.clone();
        let notes_ref_oid_clone = notes_ref_oid.clone();
        let ignore_hash_clone = ignore_hash.clone();
        let summary_clone = summary.clone();
        tokio::task::spawn_blocking(move || -> Result<(), AppError> {
            let conn = conn_arc
                .lock()
                .map_err(|_| AppError::Other("db 锁中毒".into()))?;
            stats_cache::range::put(
                &conn,
                &repo_clone,
                &start_clone,
                &end_clone,
                &notes_ref_oid_clone,
                &ignore_hash_clone,
                &summary_clone,
            )
        })
        .await
        .map_err(|e| format!("spawn_blocking 写 range cache 失败: {e}"))?
        .map_err(|e| e.to_string())?;
    }

    Ok(RangeSummaryResult::Ok {
        range_summary: summary,
    })
}

/// 把 `git-ai stats <range>` / `git rev-parse` 失败翻译成用户友好的
/// "为什么 + 怎么办" 错误字符串,前端 `call<T>` 会把它弹成红 toast。
///
/// # 决策表(根因优先级)
/// 1. `GitAiNotFound` → git-ai CLI 未装或不在 PATH(去"安装"页装最新版)
/// 2. `GitAiFailed { stderr }` 含 `refs/notes/ai` 缺失 → 仓库还没有 AI 归因 notes
///    (在该仓库 `git ai install` 注册 hook,提交几次代码后再回)
/// 3. `GitAiFailed { stderr }` 含 "not a git repository" → 当前路径非 git 仓库
///    (重新选择仓库)
/// 4. `GitAiFailed { stderr }` 含 "bad revision"/"unknown revision" → 起止 commit 无效
///    (刷新仓库或切换时间范围)
/// 5. `GitAiFailed { stderr }` 含 "permission denied" → 仓库目录权限不足
///    (检查文件系统权限)
/// 6. `Json` → git-ai 输出格式变化,版本不匹配(去 Settings → 安装 升级到最新版)
/// 7. `Io` 含 "timed out"/超时 → 仓库较大或 IO 慢(缩小时间范围或重试)
/// 8. 其它 → 透传裸 stderr,附通用兜底指引("查看 Diagnostic → debug-report")
fn classify_range_summary_error(context: &str, err: &AppError, first_sha: Option<&str>) -> String {
    let (reason, hint) = match err {
        AppError::GitAiNotFound => (
            "git-ai CLI 未安装或不在 PATH".to_string(),
            "前往「安装」页装最新版 git-ai 后重试。".to_string(),
        ),
        AppError::GitAiFailed { stderr, .. } => {
            let s = stderr.to_ascii_lowercase();
            // os error 206 = Windows ERROR_FILENAME_EXCED_RANGE,git-ai 上游
            // `range_authorship.rs` 把 range 内 changed_files 作为命令行 args 传给 git
            // 子进程,文件多时超过 CreateProcess 32KB 限制。这是上游已知架构限制,
            // Studio 无法绕过,只能引导用户缩小时间窗口。
            if s.contains("os error 206") || s.contains("文件名或扩展名太长") {
                (
                    "Windows 命令行长度超限(git-ai 上游已知问题)".to_string(),
                    "切到更小时间窗口(推荐「今天」/「近 1 天」),git-ai 内部把 range 内所有 changed files 作为 args 传给 git 子进程,Windows 32KB 命令行硬上限导致失败。这不是 Studio bug — 详见 USER_MANUAL FAQ。".to_string(),
                )
            } else if s.contains("refs/notes/ai")
                && (s.contains("does not exist") || s.contains("not found"))
            {
                (
                    "仓库还没有 AI 归因 notes(refs/notes/ai 不存在)".to_string(),
                    "在该仓库执行 `git ai install` 注册 hook,提交几次代码后再回到 Dashboard。"
                        .to_string(),
                )
            } else if s.contains("not a git repository") {
                (
                    "当前路径不是 git 仓库".to_string(),
                    "请在 Repo 页重新选择一个 git 仓库。".to_string(),
                )
            } else if s.contains("bad revision") || s.contains("unknown revision") {
                (
                    format!(
                        "git-ai 无法解析窗口边界 commit{}",
                        first_sha
                            .map(|sha| format!("(起点 {})", &sha[..sha.len().min(7)]))
                            .unwrap_or_default()
                    ),
                    "可能仓库刚做过 rebase / push --force 导致 ref 失效。回到 Repo 页刷新仓库,或切换时间范围后重试。".to_string(),
                )
            } else if s.contains("permission denied") || s.contains("access is denied") {
                (
                    "git-ai 无权读取仓库或 git-notes 目录".to_string(),
                    "检查仓库目录及 `.git/` 子目录权限。".to_string(),
                )
            } else {
                (
                    "git-ai 子进程报错".to_string(),
                    "下方详情来自 git-ai stderr;若反复出现,前往 Diagnostic → 跑 debug-report 并提 Issue。".to_string(),
                )
            }
        }
        AppError::Json(_) => (
            "git-ai 输出 JSON 解析失败".to_string(),
            "可能是 git-ai 版本与 Studio 不匹配。前往 Settings → 安装 升级到最新版本。".to_string(),
        ),
        AppError::Io(io) if io.kind() == std::io::ErrorKind::TimedOut => (
            "git-ai 调用超时(默认 15s)".to_string(),
            "仓库较大或 IO 慢;请缩小时间范围,或在 Diagnostic 跑 debug-report 排查。".to_string(),
        ),
        _ => (
            "调用 git-ai 时发生意外错误".to_string(),
            "重试一次;若仍失败,前往 Diagnostic 跑 debug-report 协助排查。".to_string(),
        ),
    };
    format!("{context}。\n原因:{reason}。\n详情:{err}\n建议:{hint}")
}

/// 把 commit sha 列表按"当前 (notes_oid, ignore_hash) 是否与缓存一致"切分为命中 / miss。
///
/// **失效模型核心**(从 history.rs 中抽出为纯函数,便于测试):
/// - cached_map[sha] 不存在 → miss
/// - cached_map[sha].notes_oid == oid_map[sha](默认 "")
///   且 cached_map[sha].ignore_hash == current_ignore_hash → hit
/// - 任一不一致 → miss(notes 被改 / `.git-ai-ignore` 被改,均需重跑)
pub fn split_hits_and_misses(
    shas: &[String],
    oid_map: &HashMap<String, String>,
    current_ignore_hash: &str,
    cached_map: &HashMap<String, crate::db::stats_cache::CachedStats>,
) -> (HashMap<String, git_ai::stats::AiStats>, Vec<String>) {
    let mut hits = HashMap::new();
    let mut misses = Vec::new();
    for sha in shas {
        let current_oid = oid_map.get(sha).cloned().unwrap_or_default();
        match cached_map.get(sha) {
            Some(c) if c.notes_oid == current_oid && c.ignore_hash == current_ignore_hash => {
                hits.insert(sha.clone(), c.stats.clone());
            }
            _ => misses.push(sha.clone()),
        }
    }
    (hits, misses)
}

#[tauri::command]
pub async fn clear_stats_cache(scope: String, state: State<'_, AppState>) -> Result<u64, String> {
    let conn_arc = state.db.clone();
    let repo_path: Option<String> = if scope == "current_repo" {
        let g = state
            .current_repo
            .read()
            .map_err(|_| "current_repo 锁中毒".to_string())?;
        Some(g.as_ref().map(|r| r.path.clone()).unwrap_or_default())
    } else {
        None
    };
    // per-commit 与 range 两张缓存表同时清(同一 scope),返回 per-commit 受影响行数(UI 口径不变)。
    let affected = tokio::task::spawn_blocking(move || -> Result<usize, AppError> {
        let conn = conn_arc
            .lock()
            .map_err(|_| AppError::Other("db 锁中毒".into()))?;
        match repo_path {
            Some(p) if !p.is_empty() => {
                let n = stats_cache::clear_repo(&conn, &p)?;
                stats_cache::range::clear_repo(&conn, &p)?;
                Ok(n)
            }
            _ => {
                let n = stats_cache::clear_all(&conn)?;
                stats_cache::range::clear_all(&conn)?;
                Ok(n)
            }
        }
    })
    .await
    .map_err(|e| format!("spawn_blocking 失败: {e}"))?
    .map_err(|e| e.to_string())?;
    Ok(affected as u64)
}

// ===== 私有 helper(被单测覆盖) =====

fn read_cached_total(state: &State<'_, AppState>, repo: &str) -> Result<u64, String> {
    let conn = state.db.lock().map_err(|_| "db 锁中毒".to_string())?;
    stats_cache::count_for_repo(&conn, repo).map_err(|e| e.to_string())
}

/// 把 [`TimeRange`] 解析为本地时区的 (start, end) 闭区间。
/// `now` 显式注入便于单测。
///
/// # 决策
/// - 周第一天 = 周一(ISO / 国内 / 项目用户多在亚太)
/// - `Today / ThisWeek / ThisMonth` 的 end = now(实时进行中,不是当日结束)
/// - `Yesterday / LastWeek / LastMonth` 的 end = 当日 23:59:59.999999999(已结束区间)
/// - `LastNDays` 维持滑动窗口语义:start = now - N 天,end = now(兼容旧 windowDays 口径)
/// - `Custom`:start > end 时返回反序,由调用方过滤逻辑自然产出空集
pub fn time_range_bounds(
    range: &TimeRange,
    now: DateTime<Local>,
) -> (DateTime<Local>, DateTime<Local>) {
    match range {
        TimeRange::Today => (start_of_day(now), now),
        TimeRange::Yesterday => {
            let yest_start = start_of_day(now) - ChronoDuration::days(1);
            let yest_end = end_of_day(yest_start);
            (yest_start, yest_end)
        }
        TimeRange::ThisWeek => (start_of_week(now), now),
        TimeRange::LastWeek => {
            let this_week_start = start_of_week(now);
            let last_week_start = this_week_start - ChronoDuration::days(7);
            let last_week_end = end_of_day(this_week_start - ChronoDuration::days(1));
            (last_week_start, last_week_end)
        }
        TimeRange::ThisMonth => (start_of_month(now), now),
        TimeRange::LastMonth => {
            let this_month_start = start_of_month(now);
            let last_month_end_day = this_month_start - ChronoDuration::days(1);
            let last_month_start = start_of_month(last_month_end_day);
            let last_month_end = end_of_day(last_month_end_day);
            (last_month_start, last_month_end)
        }
        TimeRange::LastNDays { days } => {
            let start = now - ChronoDuration::days(*days as i64);
            (start, now)
        }
        TimeRange::Custom {
            start_unix_ms,
            end_unix_ms,
        } => {
            let start = Local
                .timestamp_millis_opt(*start_unix_ms)
                .single()
                .unwrap_or(now);
            let end = Local
                .timestamp_millis_opt(*end_unix_ms)
                .single()
                .unwrap_or(now);
            (start, end)
        }
    }
}

/// 按 [`TimeRange`] 解析出的 (start, end) 区间过滤 commit。
/// 闭区间:`start <= authored_at <= end`。authored_at 解析失败的 commit 静默跳过(罕见,`%cI` 已是 RFC3339)。
pub fn filter_by_range(
    all: &[CommitBrief],
    range: &TimeRange,
    now: DateTime<Local>,
) -> Vec<CommitBrief> {
    let (start, end) = time_range_bounds(range, now);
    all.iter()
        .filter(|c| match DateTime::parse_from_rfc3339(&c.authored_at) {
            Ok(dt) => {
                let local = dt.with_timezone(&Local);
                local >= start && local <= end
            }
            Err(_) => false,
        })
        .cloned()
        .collect()
}

/// 「只看我」口径:只保留 author_email 命中 `email` 的 commit(大小写不敏感)。
/// 纯函数,便于单测;email 入参应已 trim + lowercase(由 `read_git_user_email` 保证)。
pub fn filter_by_author(commits: &[CommitBrief], email: &str) -> Vec<CommitBrief> {
    commits
        .iter()
        .filter(|c| c.author_email.eq_ignore_ascii_case(email))
        .cloned()
        .collect()
}

/// 判断 500 cap 是否**真的**可能漏算了窗口内的更老 commit。
///
/// 关键:`list_recent(cap)` 取的是"全仓最近 cap 条"(与时间窗口无关),所以**不能**仅凭
/// `recent.len() >= cap` 就报截断 —— 否则只要仓库历史总提交 ≥cap,哪怕只看"今天"也会误报
/// (今天的 commit 明明都在最近 cap 条里,根本没漏)。
///
/// 真正的截断只在:取满 cap 条,**且**这 cap 条里最旧一条仍晚于窗口起点 —— 说明窗口起点到
/// 该最旧 commit 之间可能还有没被取到的窗口内 commit。否则(最旧 commit 已早于窗口起点)
/// 窗口被这 cap 条完整覆盖,不算截断。`list_recent` 按时间倒序,末尾即最旧。纯函数,便于单测。
pub fn is_window_truncated(
    recent: &[CommitBrief],
    cap: usize,
    range_start: DateTime<Local>,
) -> bool {
    if recent.len() < cap {
        return false;
    }
    match recent
        .last()
        .and_then(|c| DateTime::parse_from_rfc3339(&c.authored_at).ok())
    {
        Some(oldest) => oldest.with_timezone(&Local) > range_start,
        None => true, // 最旧 commit 时间解析失败:保守按截断处理(极罕见,%cI 已是 RFC3339)
    }
}

fn start_of_day(dt: DateTime<Local>) -> DateTime<Local> {
    Local
        .with_ymd_and_hms(dt.year(), dt.month(), dt.day(), 0, 0, 0)
        .single()
        .unwrap_or(dt)
}

fn end_of_day(dt: DateTime<Local>) -> DateTime<Local> {
    Local
        .with_ymd_and_hms(dt.year(), dt.month(), dt.day(), 23, 59, 59)
        .single()
        .unwrap_or(dt)
        .with_nanosecond(999_999_999)
        .unwrap_or(dt)
}

fn start_of_week(dt: DateTime<Local>) -> DateTime<Local> {
    // ISO 周:周一是第 1 天。chrono::Weekday::num_days_from_monday() Mon=0..Sun=6
    let days_from_monday = dt.weekday().num_days_from_monday() as i64;
    start_of_day(dt) - ChronoDuration::days(days_from_monday)
}

fn start_of_month(dt: DateTime<Local>) -> DateTime<Local> {
    NaiveDate::from_ymd_opt(dt.year(), dt.month(), 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .and_then(|naive| Local.from_local_datetime(&naive).single())
        .unwrap_or(dt)
}

/// 按 commit `authored_at` 的本地日期分桶。日期升序,空日不出现在结果。
pub fn bucket_by_local_date(commits: &[PerCommitStat]) -> Vec<DailyBucket> {
    let mut map: HashMap<String, DailyBucket> = HashMap::new();
    for c in commits {
        let date = match DateTime::parse_from_rfc3339(&c.authored_at) {
            Ok(dt) => dt.with_timezone(&Local).format("%Y-%m-%d").to_string(),
            Err(_) => continue,
        };
        let b = map.entry(date.clone()).or_insert_with(|| DailyBucket {
            date,
            ..DailyBucket::default()
        });
        b.human_additions = b.human_additions.saturating_add(c.stats.human_additions);
        b.unknown_additions = b
            .unknown_additions
            .saturating_add(c.stats.unknown_additions);
        b.ai_additions = b.ai_additions.saturating_add(c.stats.ai_additions);
        b.commit_count = b.commit_count.saturating_add(1);
    }
    let mut out: Vec<DailyBucket> = map.into_values().collect();
    out.sort_by(|a, b| a.date.cmp(&b.date));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn short_of(sha: &str) -> String {
        sha.chars().take(7).collect()
    }

    fn brief(sha: &str, authored: &str) -> CommitBrief {
        CommitBrief {
            sha: sha.to_string(),
            short: short_of(sha),
            authored_at: authored.to_string(),
            author_name: "Test User".to_string(),
            author_email: "test@example.com".to_string(),
            subject: "test".to_string(),
            parents: vec![],
            is_merge: false,
        }
    }

    fn per(sha: &str, authored: &str, human: u64, unknown: u64, ai: u64) -> PerCommitStat {
        let s = git_ai::stats::AiStats {
            human_additions: human,
            unknown_additions: unknown,
            ai_additions: ai,
            ai_accepted: ai,
            ..Default::default()
        };
        PerCommitStat {
            sha: sha.to_string(),
            short: short_of(sha),
            authored_at: authored.to_string(),
            is_merge: false,
            stats: s,
        }
    }

    /// 用运行机本地时区构造 RFC3339 时间戳,与测试里的 `now`(Local)同源。
    /// fixture 不能硬编码固定 offset(如 +08:00),否则在非该时区的 runner(CI=UTC)上
    /// 会跨日界,使范围/分桶断言失败。
    fn local_iso(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> String {
        Local
            .with_ymd_and_hms(y, mo, d, h, mi, s)
            .unwrap()
            .to_rfc3339()
    }

    #[test]
    fn last_n_days_keeps_only_within() {
        // now = 2026-05-12 12:00 +08:00,window = 7 天
        let now = Local.with_ymd_and_hms(2026, 5, 12, 12, 0, 0).unwrap();
        let commits = vec![
            brief("aaaaaaa1", &local_iso(2026, 5, 12, 10, 0, 0)), // 今天
            brief("aaaaaaa2", &local_iso(2026, 5, 6, 10, 0, 0)),  // 边界 6 天前,在窗口
            brief("aaaaaaa3", &local_iso(2026, 5, 4, 10, 0, 0)),  // 8 天前,在窗口外
            brief("aaaaaaa4", "bad-date"),                        // 解析失败,跳过
        ];
        let kept = filter_by_range(&commits, &TimeRange::LastNDays { days: 7 }, now);
        let shas: Vec<_> = kept.iter().map(|c| c.sha.as_str()).collect();
        assert_eq!(shas, vec!["aaaaaaa1", "aaaaaaa2"]);
    }

    #[test]
    fn truncation_not_flagged_when_window_fully_covered() {
        // now = 2026-05-12 12:00;窗口 = 今天。仓库总提交达 cap=3(满),但最旧一条在 5-01(早于
        // 今天 00:00)⇒ 今天的 commit 全在这 3 条里,没漏 ⇒ 不报截断(修复前会误报)。
        let now = Local.with_ymd_and_hms(2026, 5, 12, 12, 0, 0).unwrap();
        let range_start = start_of_day(now);
        let recent = vec![
            brief("c3", &local_iso(2026, 5, 12, 10, 0, 0)), // 最新
            brief("c2", &local_iso(2026, 5, 10, 10, 0, 0)),
            brief("c1", &local_iso(2026, 5, 1, 10, 0, 0)), // 最旧,早于今天起点
        ];
        assert!(!is_window_truncated(&recent, 3, range_start));
    }

    #[test]
    fn truncation_flagged_when_cap_may_hide_older_window_commits() {
        // 取满 cap=3,且最旧一条(5-12 08:00)仍晚于窗口起点(今天 00:00)⇒ 起点到 08:00 之间
        // 可能还有没取到的今天的 commit ⇒ 报截断。
        let now = Local.with_ymd_and_hms(2026, 5, 12, 12, 0, 0).unwrap();
        let range_start = start_of_day(now);
        let recent = vec![
            brief("c3", &local_iso(2026, 5, 12, 11, 0, 0)),
            brief("c2", &local_iso(2026, 5, 12, 9, 0, 0)),
            brief("c1", &local_iso(2026, 5, 12, 8, 0, 0)), // 最旧,仍在今天内
        ];
        assert!(is_window_truncated(&recent, 3, range_start));
    }

    #[test]
    fn truncation_never_flagged_below_cap() {
        // 没取满 cap ⇒ 仓库提交本就不足 cap,绝不可能因 cap 漏算 ⇒ 永不报截断。
        let now = Local.with_ymd_and_hms(2026, 5, 12, 12, 0, 0).unwrap();
        let recent = vec![brief("c1", &local_iso(2026, 5, 12, 11, 0, 0))];
        assert!(!is_window_truncated(&recent, 3, start_of_day(now)));
    }

    #[test]
    fn filter_by_author_is_case_insensitive_and_self_only() {
        // 「只看我」口径:只保留命中当前用户邮箱的 commit,大小写不敏感;他人 commit 被剔除。
        let mut mine = brief("aaaaaaa1", "2026-05-12T10:00:00+08:00");
        mine.author_email = "Me@Example.com".to_string(); // 大小写混写,应仍命中
        let mut other = brief("bbbbbbb2", "2026-05-12T11:00:00+08:00");
        other.author_email = "teammate@example.com".to_string();
        let commits = vec![mine, other];
        let kept = filter_by_author(&commits, "me@example.com");
        let shas: Vec<_> = kept.iter().map(|c| c.sha.as_str()).collect();
        assert_eq!(shas, vec!["aaaaaaa1"]);
    }

    #[test]
    fn today_starts_at_local_midnight() {
        // now = 2026-05-12 12:00 +08:00 → 今天 = [2026-05-12 00:00, 12:00]
        let now = Local.with_ymd_and_hms(2026, 5, 12, 12, 0, 0).unwrap();
        let commits = vec![
            brief("a1", &local_iso(2026, 5, 12, 0, 0, 1)), // 凌晨刚过,在
            brief("a2", &local_iso(2026, 5, 12, 11, 59, 0)), // 在
            brief("a3", &local_iso(2026, 5, 11, 23, 59, 59)), // 昨天 23:59,不在
            brief("a4", &local_iso(2026, 5, 12, 12, 0, 1)), // 1s after now → 不在(>end)
        ];
        let kept = filter_by_range(&commits, &TimeRange::Today, now);
        let shas: Vec<_> = kept.iter().map(|c| c.sha.as_str()).collect();
        assert_eq!(shas, vec!["a1", "a2"]);
    }

    #[test]
    fn yesterday_is_full_day_closed_interval() {
        // now = 2026-05-12 09:00 → 昨天 = [2026-05-11 00:00, 23:59:59.999...]
        let now = Local.with_ymd_and_hms(2026, 5, 12, 9, 0, 0).unwrap();
        let commits = vec![
            brief("a1", &local_iso(2026, 5, 11, 0, 0, 0)), // 昨天 00:00 边界,在
            brief("a2", &local_iso(2026, 5, 11, 23, 59, 59)), // 昨天 23:59,在
            brief("a3", &local_iso(2026, 5, 12, 0, 0, 0)), // 今天 00:00,不在
            brief("a4", &local_iso(2026, 5, 10, 23, 59, 59)), // 前天 23:59,不在
        ];
        let kept = filter_by_range(&commits, &TimeRange::Yesterday, now);
        let shas: Vec<_> = kept.iter().map(|c| c.sha.as_str()).collect();
        assert_eq!(shas, vec!["a1", "a2"]);
    }

    #[test]
    fn this_week_starts_on_monday_local() {
        // now = 2026-05-13 (周三) 12:00 → 本周 = [2026-05-11 00:00 (周一), now]
        let now = Local.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
        let commits = vec![
            brief("a1", &local_iso(2026, 5, 11, 0, 0, 0)), // 周一 00:00,在
            brief("a2", &local_iso(2026, 5, 13, 11, 59, 0)), // 周三现在前,在
            brief("a3", &local_iso(2026, 5, 10, 23, 59, 59)), // 上周日,不在
        ];
        let kept = filter_by_range(&commits, &TimeRange::ThisWeek, now);
        let shas: Vec<_> = kept.iter().map(|c| c.sha.as_str()).collect();
        assert_eq!(shas, vec!["a1", "a2"]);
    }

    #[test]
    fn last_week_is_monday_to_sunday_closed() {
        // now = 2026-05-13 (周三) → 上周 = [2026-05-04 周一, 2026-05-10 周日 23:59:59.999]
        let now = Local.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
        let commits = vec![
            brief("a1", &local_iso(2026, 5, 4, 0, 0, 0)), // 上周一 00:00,在
            brief("a2", &local_iso(2026, 5, 10, 23, 59, 59)), // 上周日 23:59,在
            brief("a3", &local_iso(2026, 5, 11, 0, 0, 0)), // 本周一 00:00,不在
            brief("a4", &local_iso(2026, 5, 3, 23, 59, 59)), // 上上周日,不在
        ];
        let kept = filter_by_range(&commits, &TimeRange::LastWeek, now);
        let shas: Vec<_> = kept.iter().map(|c| c.sha.as_str()).collect();
        assert_eq!(shas, vec!["a1", "a2"]);
    }

    #[test]
    fn this_month_starts_at_first_of_month() {
        // now = 2026-05-13 → 本月 = [2026-05-01 00:00, now]
        let now = Local.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
        let commits = vec![
            brief("a1", &local_iso(2026, 5, 1, 0, 0, 0)), // 本月 1 号 00:00,在
            brief("a2", &local_iso(2026, 5, 13, 11, 59, 0)), // 本月 13 号现在前,在
            brief("a3", &local_iso(2026, 4, 30, 23, 59, 59)), // 上月最后一天,不在
        ];
        let kept = filter_by_range(&commits, &TimeRange::ThisMonth, now);
        let shas: Vec<_> = kept.iter().map(|c| c.sha.as_str()).collect();
        assert_eq!(shas, vec!["a1", "a2"]);
    }

    #[test]
    fn last_month_handles_month_rollover() {
        // now = 2026-05-13 → 上月 = [2026-04-01 00:00, 2026-04-30 23:59:59.999]
        let now = Local.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
        let commits = vec![
            brief("a1", &local_iso(2026, 4, 1, 0, 0, 0)), // 上月 1 号,在
            brief("a2", &local_iso(2026, 4, 30, 23, 59, 59)), // 上月最后一天,在
            brief("a3", &local_iso(2026, 5, 1, 0, 0, 0)), // 本月 1 号,不在
            brief("a4", &local_iso(2026, 3, 31, 23, 59, 59)), // 上上月,不在
        ];
        let kept = filter_by_range(&commits, &TimeRange::LastMonth, now);
        let shas: Vec<_> = kept.iter().map(|c| c.sha.as_str()).collect();
        assert_eq!(shas, vec!["a1", "a2"]);
    }

    #[test]
    fn last_month_jan_rolls_back_to_dec_prev_year() {
        // 跨年边界:now = 2026-01-15 → 上月 = [2025-12-01, 2025-12-31 23:59:59.999]
        let now = Local.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap();
        let commits = vec![
            brief("a1", &local_iso(2025, 12, 1, 0, 0, 0)),     // 在
            brief("a2", &local_iso(2025, 12, 31, 23, 59, 59)), // 在
            brief("a3", &local_iso(2026, 1, 1, 0, 0, 0)),      // 不在
            brief("a4", &local_iso(2025, 11, 30, 23, 59, 59)), // 不在
        ];
        let kept = filter_by_range(&commits, &TimeRange::LastMonth, now);
        let shas: Vec<_> = kept.iter().map(|c| c.sha.as_str()).collect();
        assert_eq!(shas, vec!["a1", "a2"]);
    }

    #[test]
    fn custom_range_uses_unix_ms_bounds() {
        // now 无关紧要(custom 用自带 bounds)
        let now = Local.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
        let start = Local.with_ymd_and_hms(2026, 4, 1, 0, 0, 0).unwrap();
        let end = Local.with_ymd_and_hms(2026, 4, 30, 23, 59, 59).unwrap();
        let range = TimeRange::Custom {
            start_unix_ms: start.timestamp_millis(),
            end_unix_ms: end.timestamp_millis(),
        };
        let commits = vec![
            brief("a1", &local_iso(2026, 4, 15, 10, 0, 0)), // 在
            brief("a2", &local_iso(2026, 4, 30, 23, 0, 0)), // 在
            brief("a3", &local_iso(2026, 5, 1, 0, 0, 0)),   // 不在(> end)
            brief("a4", &local_iso(2026, 3, 31, 23, 0, 0)), // 不在(< start)
        ];
        let kept = filter_by_range(&commits, &range, now);
        let shas: Vec<_> = kept.iter().map(|c| c.sha.as_str()).collect();
        assert_eq!(shas, vec!["a1", "a2"]);
    }

    #[test]
    fn this_week_when_today_is_monday() {
        // 周一边界:now = 2026-05-11 周一 09:00 → ThisWeek start 应是同一天 00:00,不退到上周
        let now = Local.with_ymd_and_hms(2026, 5, 11, 9, 0, 0).unwrap();
        let (start, end) = time_range_bounds(&TimeRange::ThisWeek, now);
        assert_eq!(
            start.weekday(),
            Local
                .with_ymd_and_hms(2026, 5, 11, 0, 0, 0)
                .unwrap()
                .weekday()
        );
        assert_eq!(start.hour(), 0);
        assert_eq!(start.day(), 11);
        assert_eq!(end, now);
    }

    #[test]
    fn bucket_by_local_date_aggregates_per_day() {
        // 同一天两个 commit + 另一天一个
        let cs = vec![
            per("sha1", &local_iso(2026, 5, 12, 9, 0, 0), 10, 0, 5),
            per("sha2", &local_iso(2026, 5, 12, 18, 0, 0), 20, 1, 3),
            per("sha3", &local_iso(2026, 5, 11, 22, 0, 0), 100, 0, 50),
        ];
        let buckets = bucket_by_local_date(&cs);
        assert_eq!(buckets.len(), 2);
        // 升序
        assert_eq!(buckets[0].date, "2026-05-11");
        assert_eq!(buckets[0].ai_additions, 50);
        assert_eq!(buckets[0].commit_count, 1);
        assert_eq!(buckets[1].date, "2026-05-12");
        assert_eq!(buckets[1].human_additions, 30);
        assert_eq!(buckets[1].ai_additions, 8);
        assert_eq!(buckets[1].commit_count, 2);
    }

    #[test]
    fn bucket_drops_unparseable_dates() {
        let cs = vec![
            per("sha1", "bad", 100, 0, 0),
            per("sha2", &local_iso(2026, 5, 12, 10, 0, 0), 10, 0, 5),
        ];
        let buckets = bucket_by_local_date(&cs);
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].date, "2026-05-12");
    }

    #[test]
    fn bucket_uses_machine_local_timezone_not_commit_tz() {
        // 口径锁定(评审 C #4):bucket_by_local_date 按**应用进程所在机器的 Local 时区**分桶,
        // 不按 commit 自己声明的 TZ。原因:用户看自己仓库的"今天"是机器 TZ 的今天。
        // 本测断言:同一 UTC 时刻 的 commit,不论原 TZ 标注,都会被转到 Local 后分到同一天。
        let cs = vec![
            // 两个 commit 实际同一 UTC 时刻(2026-05-13 02:00 UTC),声明 TZ 不同
            per("sha1", "2026-05-12T22:00:00-04:00", 5, 0, 1),
            per("sha2", "2026-05-13T02:00:00+00:00", 10, 0, 2),
        ];
        let buckets = bucket_by_local_date(&cs);
        // 同一 UTC 时刻 → 转 Local 后必定同一天 → 必落同一桶
        assert_eq!(buckets.len(), 1, "同 UTC 时刻应分到同一本地日期桶");
        assert_eq!(buckets[0].commit_count, 2);
        assert_eq!(buckets[0].human_additions, 15);
        assert_eq!(buckets[0].ai_additions, 3);
    }

    #[test]
    fn bucket_empty_input_yields_empty_vec() {
        let buckets = bucket_by_local_date(&[]);
        assert!(buckets.is_empty());
    }

    // ===== split_hits_and_misses — 失效模型核心(评审 A 必补) =====

    use crate::db::stats_cache::CachedStats;

    /// 测试 helper:旧测试默认 `ignore_hash=""`(同时刻 current_ignore_hash 也传 "" → 命中)。
    fn cached(oid: &str, ai: u64) -> CachedStats {
        cached_with_ignore(oid, "", ai)
    }

    fn cached_with_ignore(oid: &str, ignore_hash: &str, ai: u64) -> CachedStats {
        let s = git_ai::stats::AiStats {
            ai_additions: ai,
            ai_accepted: ai,
            ..Default::default()
        };
        CachedStats {
            stats: s,
            notes_oid: oid.to_string(),
            ignore_hash: ignore_hash.to_string(),
            fetched_at_unix_ms: 0,
        }
    }

    #[test]
    fn hit_when_oid_matches() {
        let shas = vec!["sha-1".to_string()];
        let mut oid_map = HashMap::new();
        oid_map.insert("sha-1".to_string(), "A".to_string());
        let mut cached_map = HashMap::new();
        cached_map.insert("sha-1".to_string(), cached("A", 50));
        let (hits, misses) = split_hits_and_misses(&shas, &oid_map, "", &cached_map);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits.get("sha-1").unwrap().ai_additions, 50);
        assert!(misses.is_empty());
    }

    #[test]
    fn miss_when_oid_mismatches() {
        // 缓存的 notes_oid 是 A,当前是 B → notes 被改写,必须重跑(评审 A 必补)
        let shas = vec!["sha-1".to_string()];
        let mut oid_map = HashMap::new();
        oid_map.insert("sha-1".to_string(), "B".to_string());
        let mut cached_map = HashMap::new();
        cached_map.insert("sha-1".to_string(), cached("A", 50));
        let (hits, misses) = split_hits_and_misses(&shas, &oid_map, "", &cached_map);
        assert!(hits.is_empty());
        assert_eq!(misses, vec!["sha-1".to_string()]);
    }

    #[test]
    fn miss_when_not_cached() {
        let shas = vec!["sha-1".to_string()];
        let mut oid_map = HashMap::new();
        oid_map.insert("sha-1".to_string(), "A".to_string());
        let cached_map = HashMap::new();
        let (hits, misses) = split_hits_and_misses(&shas, &oid_map, "", &cached_map);
        assert!(hits.is_empty());
        assert_eq!(misses, vec!["sha-1".to_string()]);
    }

    #[test]
    fn hit_when_both_have_no_notes() {
        // cached.notes_oid="" 且 oid_map 无该 sha → 都视作"无 notes"状态,命中
        let shas = vec!["sha-1".to_string()];
        let oid_map = HashMap::new();
        let mut cached_map = HashMap::new();
        cached_map.insert("sha-1".to_string(), cached("", 20));
        let (hits, misses) = split_hits_and_misses(&shas, &oid_map, "", &cached_map);
        assert_eq!(hits.len(), 1);
        assert!(misses.is_empty());
    }

    #[test]
    fn miss_when_notes_appeared_after_cache() {
        // 缓存时无 notes(oid=""),后来打了 checkpoint(oid="A")→ miss
        let shas = vec!["sha-1".to_string()];
        let mut oid_map = HashMap::new();
        oid_map.insert("sha-1".to_string(), "A".to_string());
        let mut cached_map = HashMap::new();
        cached_map.insert("sha-1".to_string(), cached("", 0));
        let (hits, misses) = split_hits_and_misses(&shas, &oid_map, "", &cached_map);
        assert!(hits.is_empty());
        assert_eq!(misses, vec!["sha-1".to_string()]);
    }

    #[test]
    fn mixed_partial_hit_partial_miss() {
        let shas = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let mut oid_map = HashMap::new();
        oid_map.insert("a".to_string(), "A".to_string());
        oid_map.insert("b".to_string(), "BB".to_string());
        oid_map.insert("c".to_string(), "C".to_string());
        let mut cached_map = HashMap::new();
        cached_map.insert("a".to_string(), cached("A", 10)); // hit
        cached_map.insert("b".to_string(), cached("B-old", 20)); // miss
                                                                 // c 没缓存 → miss
        let (hits, misses) = split_hits_and_misses(&shas, &oid_map, "", &cached_map);
        assert_eq!(hits.len(), 1);
        assert!(hits.contains_key("a"));
        assert_eq!(misses.len(), 2);
        assert!(misses.contains(&"b".to_string()));
        assert!(misses.contains(&"c".to_string()));
    }

    // ===== P10 #29:ignore_hash 维度 =====

    #[test]
    fn hit_when_notes_and_ignore_both_match() {
        let shas = vec!["sha-1".to_string()];
        let mut oid_map = HashMap::new();
        oid_map.insert("sha-1".to_string(), "A".to_string());
        let mut cached_map = HashMap::new();
        cached_map.insert("sha-1".to_string(), cached_with_ignore("A", "ih-1", 7));
        let (hits, misses) = split_hits_and_misses(&shas, &oid_map, "ih-1", &cached_map);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits.get("sha-1").unwrap().ai_additions, 7);
        assert!(misses.is_empty());
    }

    #[test]
    fn miss_when_ignore_hash_mismatches() {
        // notes_oid 一致,但 .git-ai-ignore 改动 → ignore_hash 不一致 → miss
        let shas = vec!["sha-1".to_string()];
        let mut oid_map = HashMap::new();
        oid_map.insert("sha-1".to_string(), "A".to_string());
        let mut cached_map = HashMap::new();
        cached_map.insert(
            "sha-1".to_string(),
            cached_with_ignore("A", "old-ignore-hash", 7),
        );
        let (hits, misses) = split_hits_and_misses(&shas, &oid_map, "new-ignore-hash", &cached_map);
        assert!(hits.is_empty());
        assert_eq!(misses, vec!["sha-1".to_string()]);
    }

    #[test]
    fn miss_when_user_first_adds_gitaiignore() {
        // 旧 cache(v1 migration 后 ignore_hash="") 在用户首次添加 .git-ai-ignore 后失效。
        let shas = vec!["sha-1".to_string()];
        let mut oid_map = HashMap::new();
        oid_map.insert("sha-1".to_string(), "A".to_string());
        let mut cached_map = HashMap::new();
        // 历史行:ignore_hash 来自 DEFAULT '' 列(migration v2)
        cached_map.insert("sha-1".to_string(), cached_with_ignore("A", "", 7));
        // 用户写了 .git-ai-ignore → current_ignore_hash 不再为 ""
        let (hits, misses) = split_hits_and_misses(&shas, &oid_map, "h-now", &cached_map);
        assert!(hits.is_empty());
        assert_eq!(misses, vec!["sha-1".to_string()]);
    }
}
