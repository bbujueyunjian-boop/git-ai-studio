// 与 src-tauri/src/state.rs / commands/diagnostic.rs / commands/repo.rs 一一对齐。
// 任何 Rust struct 字段变更后,同步改这里。

export interface RepoEntry {
  path: string;
  name: string;
  head_branch: string | null;
  head_sha: string | null;
  /** null 表示尚未探测(扫描列表态);true/false 表示已知有/无未提交改动。 */
  dirty: boolean | null;
  has_git_ai_dir: boolean;
  /** `.git/ai/working_logs/<head_sha>/*.jsonl` 数量;0 表示当前 HEAD 还没有 checkpoint。 */
  working_logs_count: number;
}

export interface DebugReportSection {
  name: string;
  raw: string;
  entries: [string, string][];
}

export interface DebugReport {
  ok: boolean;
  git_ai_version?: string;
  generated_at?: string;
  sections: DebugReportSection[];
  raw: string;
}

export type AgentKind = "Claude" | "Cursor" | "Codex" | "OpenCode" | "Gemini" | "Pi";

/** git-ai daemon 健康态。后端 [`DaemonHealth`] 用 `#[serde(tag="kind", rename_all="snake_case")]`,
 *  这里的 kind 取值与之严格对齐;`blocked_lock_unknown_pid` 表示需要核实持锁者。 */
export type DaemonHealth =
  | { kind: "idle" }
  | { kind: "running"; pid: number }
  | {
      kind: "blocked_lock_unknown_pid";
      lock_path: string;
      pid_meta_path: string;
      last_pid: number | null;
    };

export interface DaemonRepairResult {
  before: DaemonHealth;
  after: DaemonHealth;
}

export type HookType = "command" | "http" | "unknown";

export interface AgentHookStatus {
  agent: AgentKind;
  detected: boolean;
  configured: boolean;
  config_path: string | null;
  hook_type: HookType | null;
  raw_excerpt: string | null;
  issues: string[];
}

export type DegradeReason =
  | { kind: "git_ai_not_found"; hint: string }
  | { kind: "command_failed"; code: number; stderr: string };

export interface DiagnosticOverview {
  generated_at_unix_ms: number;
  took_ms: number;
  repo: RepoEntry | null;
  report: DebugReport;
  agents: AgentHookStatus[];
  degraded: DegradeReason | null;
}

/** 红/黄/绿/灰 四态(灰=未检测/不适用)。 */
export type StatusLevel = "ok" | "warn" | "err" | "muted";

/** Diagnostic 页一项检查的统一展示模型。 */
export interface CheckItem {
  id: string;
  label: string;
  level: StatusLevel;
  /** 这一项不通过会影响哪个指标(给用户的人话,而不是只有色块)。 */
  impact?: string;
  /** 详情(实际值、错误文本)。 */
  detail?: string;
  /** 修复跳转目标页。 */
  fix?: { to: string; label: string };
}

// ===== Install / Releases =====

export interface ReleaseAsset {
  name: string;
  size: number;
  browser_download_url: string;
}

export interface ReleaseSummary {
  tag: string;
  name: string;
  published_at: string;
  is_prerelease: boolean;
  body: string;
  assets: ReleaseAsset[];
  is_latest: boolean;
}

export interface RateLimitInfo {
  remaining: number;
  reset_unix: number;
}

export interface ReleasesPayload {
  releases: ReleaseSummary[];
  fetched_at_unix_ms: number;
  /** true = 这次响应是 304 命中本地 ETag,数据复用缓存;不代表失败。 */
  from_etag_cache: boolean;
  rate_limit: RateLimitInfo | null;
}

export interface InstalledVersion {
  installed: boolean;
  version: string | null;
  binary_path: string | null;
}

/** 可被本应用代装的外部 AI 编码 CLI(npm 包)。安装目标集合 != 被探测 hook 的 AgentKind 集合。 */
export type AgentCli = "ClaudeCode" | "Codex";

/** npm 探测结果。available=false 是预期空态(未装 Node),前端据此禁用装/卸并提示,不弹错。 */
export interface NpmStatus {
  available: boolean;
  version: string | null;
  path: string | null;
}

export interface InstallHistoryEntry {
  at_unix_ms: number;
  action: "install" | "upgrade" | "uninstall";
  version_previous: string | null;
  version_current: string | null;
  outcome: "success" | "failed";
  exit_code: number | null;
}

export interface GitAiConfig {
  disable_auto_updates: boolean;
  update_channel: string;
  [key: string]: unknown;
}

export interface GitAiConfigPatch {
  disable_auto_updates?: boolean;
  update_channel?: string;
}

// 流式日志 event payload(订阅 install://<job_id>/log)
export interface InstallLogEvent {
  stream: "stdout" | "stderr" | "exit";
  line?: string;
  code?: number;
  timeout?: boolean;
  ts: number;
}

// ===== App Settings =====

export type CloseBehavior = "exit" | "tray";

export interface LowAiShareConfig {
  enabled: boolean;
  /** 阈值百分比 [1, 100];null = 走前端默认 80。 */
  threshold_percent: number | null;
  /** 手动关注邮箱;空数组 = 自动使用当前仓库 git config user.email。 */
  target_emails: string[];
  /** 重复提醒间隔(分钟);null = 前端默认 360。 */
  remind_interval_minutes: number | null;
  /** 点 X / 静默按钮后的静默时长(分钟);null = 前端默认 1440。 */
  dismiss_minutes: number | null;
  /**
   * 实时触发开关:开启后 fsnotify 监听 refs/notes/ai,commit 完成 1-3s 内推送;
   * 关闭走 15 分钟轮询。null = 走前端默认 true(向后兼容老配置)。
   */
  realtime_enabled: boolean | null;
}

export interface NotificationsConfig {
  cc_switch_auto_repair: boolean;
  low_ai_share: LowAiShareConfig;
  /** git-ai daemon 异常推送 OS 通知的独立总开关。默认 false。 */
  daemon_unhealthy_alert: boolean;
}

export interface AppSettings {
  scan_roots: string[];
  recent_repos: string[];
  last_repo: string | null;
  theme: string | null;
  /** 主窗口关闭时的行为;null / 缺失 视为 "exit"(与历史行为一致)。 */
  close_behavior: CloseBehavior | null;
  /** 通知与守护类配置子结构。 */
  notifications: NotificationsConfig;
  repo_setup_seen: boolean;
  /** 桌面宠物配置(opt-in,默认关)。详见 ADR-011。 */
  pet: PetConfig;
  /** 显式勾选纳入跨仓聚合的仓库路径集合(M1)。与 recent_repos / current_repo 正交。 */
  aggregate_repos: string[];
  /** **已废弃**:历史顶层位置,后端 load 时迁移到 notifications.cc_switch_auto_repair。 */
  cc_switch_auto_repair?: boolean | null;
}

/** get_aggregate_repos 的返回项:聚合集合里一个仓库 + 其当前有效性。
 *  对齐 src-tauri/src/commands/repo.rs::AggregateRepoEntry。失效仓 valid=false / entry=null。 */
export interface AggregateRepoEntry {
  path: string;
  valid: boolean;
  entry: RepoEntry | null;
}

/** 桌面宠物配置。对齐 src-tauri/src/state.rs::PetConfig。 */
export interface PetConfig {
  enabled: boolean;
  /** 形象主题 id(robot3d / robotflat / inkbeast);null = 前端默认 "robot3d"。 */
  theme_id: string | null;
  /** 拖拽记忆的窗口位置 [physical x, y];null = 用默认位置。 */
  position: [number, number] | null;
  /** 尺寸档位;null = 前端默认 "medium"。 */
  size: "small" | "medium" | "large" | null;
  /** 整体不透明度 [0.2, 1];null = 前端默认 1。 */
  opacity: number | null;
  /** 醒目提醒重复间隔(秒);0 = 不重复;null = 前端默认 30。 */
  alert_interval_sec: number | null;
}

/**
 * 增量 patch:只覆盖前端显式给出的字段。
 *
 * # 扁平化
 * 后端持久化是嵌套(`notifications.low_ai_share.enabled`),patch 这里保持扁平,
 * 前端调用更直接:`setAppSettings({ low_ai_share_enabled: true })`。
 */
export interface AppSettingsPatch {
  scan_roots?: string[];
  theme?: string;
  close_behavior?: CloseBehavior;
  cc_switch_auto_repair?: boolean;
  low_ai_share_enabled?: boolean;
  /** 后端 clamp 到 [1, 100]。 */
  low_ai_share_threshold_percent?: number;
  low_ai_share_target_emails?: string[];
  /** 后端 clamp 到 [5, 1440]。 */
  low_ai_share_remind_interval_minutes?: number;
  /** 后端 clamp 到 [5, 10080]。 */
  low_ai_share_dismiss_minutes?: number;
  /** 低 AI 占比"实时"开关。开启后 fsnotify 监听 refs/notes/ai,关闭走 15 分钟轮询。 */
  low_ai_share_realtime_enabled?: boolean;
  /** git-ai daemon 异常推送 OS 通知的独立总开关。 */
  daemon_unhealthy_alert?: boolean;
  repo_setup_seen?: boolean;
  /** 桌面宠物:总开关(翻转即时显隐 pet 窗口)。 */
  pet_enabled?: boolean;
  /** 桌面宠物:形象主题 id(robot3d / robotflat / inkbeast)。 */
  pet_theme_id?: string;
  /** 桌面宠物:拖拽后记忆的窗口位置 [physical x, y]。 */
  pet_position?: [number, number];
  /** 桌面宠物:尺寸档位(small / medium / large)。 */
  pet_size?: "small" | "medium" | "large";
  /** 桌面宠物:整体不透明度 [0.2, 1];后端 clamp。 */
  pet_opacity?: number;
  /** 桌面宠物:醒目提醒重复间隔(秒);0 = 不重复;后端 clamp 到 [0, 600]。 */
  pet_alert_interval_sec?: number;
}

// ===== Hooks =====

export type HooksMode = "official" | "none";

export interface HooksStatus {
  mode: HooksMode;
}

export interface SettingsBackup {
  path: string;
  at_unix_ms: number;
  size: number;
}

export interface ClaudeSettingsView {
  path: string;
  exists: boolean;
  raw_size: number;
  raw: string | null;
  mode: HooksMode;
}

export interface ApplyResult {
  mode_after: HooksMode;
  changed: boolean;
  added: string[];
  updated: string[];
  removed: string[];
}

// ===== Stats(P4)=====
// 字段对齐 git-ai 上游 src/authorship/stats.rs::CommitStats(7 个字段)。
// 上游公式(stats.rs:114):total_additions = human + unknown + ai(3 桶并列)。

export interface ToolModelStats {
  ai_additions: number;
  ai_accepted: number;
}

export interface AiStats {
  human_additions: number;
  /** 上游定义(stats.rs:22):lines with no attestation at all。 */
  unknown_additions: number;
  ai_additions: number;
  /** 上游 stats.rs:116 注释:ai_additions == ai_accepted 恒成立,二者数值相同。 */
  ai_accepted: number;
  git_diff_deleted_lines: number;
  git_diff_added_lines: number;
  /**
   * key 形如 "claude_code::claude-sonnet-4-5-20250929"。
   * 上游 runtime 真源 `git-ai/src/authorship/stats.rs:470,477` +
   * `diff_ai_accepted.rs:62` 用 `format!("{}::{}", tool, model)`(双冒号);
   * 上游 README 的单斜杠示例是过期文档,以代码为准。
   */
  tool_model_breakdown: Record<string, ToolModelStats>;
}

export type StatsKind = "commit" | "working";

export type NoteKind = "merge" | "empty_additions" | "working_logs_missing";

export interface StatsView {
  kind: StatsKind;
  commit_sha: string | null;
  is_merge: boolean;
  stats: AiStats;
  /** ai_additions + human_additions + mixed_additions + unknown_additions(后端聚合一次,前端不要重算)。 */
  total_additions: number;
  note_kind: NoteKind | null;
}

export type StatsDegradedKind = "repo_missing" | "git_ai_missing" | "no_head";

export interface StatsDegraded {
  kind: StatsDegradedKind;
}

export type StatsResult =
  | { status: "ok"; view: StatsView }
  | { status: "degraded"; reason: StatsDegraded };

export interface CommitBrief {
  sha: string;
  short: string;
  /** ISO-8601 with TZ(`%cI`)。 */
  authored_at: string;
  /** `git log %an`:作者显示名,不经 mailmap。 */
  author_name: string;
  /** `git log %ae`:作者邮箱,People 视图按 lowercase 做身份聚合 key。 */
  author_email: string;
  subject: string;
  parents: string[];
  is_merge: boolean;
}

// ===== History / Dashboard(P5)=====
// 字段对齐 src-tauri/src/commands/history.rs 与 git_ai/stats.rs::RangeAuthorshipStats。

export interface PerCommitStat {
  sha: string;
  short: string;
  authored_at: string;
  is_merge: boolean;
  stats: AiStats;
}

export interface DailyBucket {
  /** 本地时区 YYYY-MM-DD。 */
  date: string;
  human_additions: number;
  unknown_additions: number;
  ai_additions: number;
  commit_count: number;
}

export interface RangeAuthorshipStatsData {
  total_commits: number;
  commits_with_authorship: number;
  authors_committing_authorship: string[];
  authors_not_committing_authorship: string[];
  commits_without_authorship: string[];
  /** Vec<(sha, git_author)>。 */
  commits_without_authorship_with_authors: Array<[string, string]>;
}

export interface RangeAuthorshipStats {
  authorship_stats: RangeAuthorshipStatsData;
  range_stats: AiStats;
}

/**
 * 时间范围筛选维度。镜像 src-tauri/src/commands/history.rs::TimeRange(serde tagged enum)。
 *
 * - 周第一天 = 周一(ISO / 国内习惯)
 * - `today / this_week / this_month` 的 end = 当前时刻
 * - `yesterday / last_week / last_month` 的 end = 当日 23:59:59.999
 * - `last_n_days` 维持滑动窗口语义(兼容旧 windowDays 7/30/90)
 * - `custom` 接受任意 unix_ms 范围
 */
export type TimeRange =
  | { kind: "today" }
  | { kind: "yesterday" }
  | { kind: "this_week" }
  | { kind: "last_week" }
  | { kind: "this_month" }
  | { kind: "last_month" }
  | { kind: "last_n_days"; days: number }
  | { kind: "custom"; start_unix_ms: number; end_unix_ms: number };

export interface HistoryPayload {
  /** 本次查询的时间范围(echo 回前端)。 */
  range: TimeRange;
  /** range start 的本地时刻 unix_ms(给前端 chart X 轴 domain 用)。 */
  range_start_unix_ms: number;
  /** range end 的本地时刻 unix_ms。 */
  range_end_unix_ms: number;
  total_commits_in_window: number;
  per_commit: PerCommitStat[];
  daily_buckets: DailyBucket[];
  cache_hits: number;
  cached_repo_total: number;
  /** git-ai stats 子进程失败的 commit sha 列表。UI 显式提示,不被 0 桶兜底。 */
  failed_shas: string[];
  /** `list_recent` 拉到 hard cap(500)条 commit 时为 true,提示窗口可能漏算更老 commit。 */
  truncated: boolean;
  took_ms: number;
}

export type HistoryDegradedKind = "repo_missing" | "git_ai_missing";

export type HistoryResult =
  | { status: "ok"; payload: HistoryPayload }
  | { status: "degraded"; reason: { kind: HistoryDegradedKind } };

// ===== 提交归因 commit 浏览器(Stats 页)。镜像 history.rs 的 CommitWithStats / RecentCommits*。=====
// 与 HistoryPayload 正交:这是"最近 N 个 commit 各自的元信息 + AI 三桶"的扁平列表,供 Stats 页
// commit 列表 + 详情渲染;复用与 get_history 同一份缓存。

export interface CommitWithStats {
  sha: string;
  short: string;
  /** ISO-8601 with TZ。 */
  authored_at: string;
  author_name: string;
  author_email: string;
  subject: string;
  is_merge: boolean;
  stats: AiStats;
  /** 提示性 note(merge / 空新增 / 缺 hook),后端复用 derive_note_kind 单一口径。 */
  note_kind: NoteKind | null;
}

export interface RecentCommitsPayload {
  commits: CommitWithStats[];
  /** git-ai stats 子进程失败的 commit:在 commits 里以 0 桶占位,UI 显式提示,绝不当真实数据。 */
  failed_shas: string[];
  /** list_recent 取到刚好 max_count 条 ⇒ 可能有更老 commit 未纳入,UI 显式提示。 */
  truncated: boolean;
  cache_hits: number;
  took_ms: number;
}

export type RecentCommitsResult =
  | { status: "ok"; payload: RecentCommitsPayload }
  | { status: "degraded"; reason: { kind: HistoryDegradedKind } };

// ===== 跨仓聚合历史(M2)。镜像 src-tauri/src/commands/history.rs 的 Aggregate* 类型。 =====
// 与单仓 HistoryPayload 正交:Dashboard 默认跨仓走这套,LowAiShare 等单仓消费者仍用 HistoryPayload。

/** 跨仓 per-commit:带 repo_path 以支持跨仓识别与下钻。 */
export interface AggregatePerCommit {
  repo_path: string;
  sha: string;
  short: string;
  authored_at: string;
  is_merge: boolean;
  stats: AiStats;
}

/** 整仓采集失败:其数据未并入聚合,UI 必须显式列出"未纳入统计",绝不当 0。 */
export interface FailedRepo {
  repo_path: string;
  reason: string;
}

/** 单 commit 采集失败(带 repo_path 限定,跨仓 sha 不全局唯一)。 */
export interface FailedCommit {
  repo_path: string;
  sha: string;
}

export interface AggregateHistoryPayload {
  range: TimeRange;
  range_start_unix_ms: number;
  range_end_unix_ms: number;
  total_commits_in_window: number;
  per_commit: AggregatePerCommit[];
  daily_buckets: DailyBucket[];
  cache_hits: number;
  /** 采集失败的仓(数据未并入聚合,UI 显式提示)。 */
  failed_repos: FailedRepo[];
  /** 单 commit 采集失败(0 桶占位,UI 显式提示)。 */
  failed_shas: FailedCommit[];
  /** 命中 500 cap 的仓(可能有更老 commit 未计入)。 */
  truncated_repos: string[];
  took_ms: number;
}

export type AggregateHistoryDegradedKind = "no_repos_selected" | "git_ai_missing";

export type AggregateHistoryResult =
  | { status: "ok"; payload: AggregateHistoryPayload }
  | { status: "degraded"; reason: { kind: AggregateHistoryDegradedKind } };

/** 单仓未提交切片(三桶);仅含有改动的仓。 */
export interface WorkingRepoSlice {
  repo_path: string;
  human_additions: number;
  unknown_additions: number;
  ai_additions: number;
}

/** 跨仓「本地未提交」聚合(`git ai status` 求和)。与时间窗口正交、不缓存、天然只看我。 */
export interface AggregateWorkingStatusPayload {
  repos_with_changes: number;
  human_additions: number;
  unknown_additions: number;
  ai_additions: number;
  per_repo: WorkingRepoSlice[];
  failed_repos: FailedRepo[];
  took_ms: number;
}

export type AggregateWorkingStatusResult =
  | { status: "ok"; payload: AggregateWorkingStatusPayload }
  | { status: "degraded"; reason: { kind: AggregateHistoryDegradedKind } };

/**
 * `get_range_summary` 的返回。镜像 src-tauri/src/commands/history.rs::RangeSummaryResult。
 *
 * range 聚合(hook 覆盖率)已从 `get_history` 解耦为独立命令:固有耗时长且有自己的缓存,
 * 由 Dashboard 独立 query 驱动,不连累主体渲染。空态走 degraded;真失败走 Err → 红 toast。
 *
 * `empty_window`:选中时间范围内无 commit,无从推导 range 边界(get_history 同窗口也为空)。
 */
export type RangeSummaryDegradedKind = "repo_missing" | "git_ai_missing" | "empty_window";

export type RangeSummaryResult =
  | { status: "ok"; range_summary: RangeAuthorshipStats }
  | { status: "degraded"; reason: { kind: RangeSummaryDegradedKind } };

export type CacheScope = "all" | "current_repo";

// ===== Ignore patterns(P11-C)=====

export type IgnoreDegradedKind = "repo_missing" | "git_ai_missing";

export interface EffectiveIgnorePatternsPayload {
  repo_path: string;
  patterns: string[];
}

export type EffectiveIgnorePatternsResult =
  | { status: "ok"; payload: EffectiveIgnorePatternsPayload }
  | { status: "degraded"; reason: { kind: IgnoreDegradedKind } };

// ===== Auth (whoami / logout)(P11-D)=====
// 镜像 src-tauri/src/git_ai/auth.rs::WhoamiPayload + AuthState。

export type AuthState =
  | { kind: "logged_out" }
  | { kind: "logged_in" }
  | { kind: "refresh_expired" }
  | { kind: "error"; message: string };

export interface OrgEntry {
  org_id: string | null;
  org_slug: string | null;
  org_name: string | null;
  role: string | null;
}

export interface WhoamiPayload {
  api_base_url: string;
  backend: string;
  /** 仅当用户用 GIT_AI_API_KEY env 而非 OAuth 时存在(已脱敏)。 */
  api_key_masked: string | null;
  state: AuthState;
  /** 上游人话格式(如 "2026-05-13 14:00:00 UTC"),原样透传。 */
  access_token_expires_at: string | null;
  refresh_token_expires_at: string | null;
  user_id: string | null;
  email: string | null;
  name: string | null;
  personal_org_id: string | null;
  orgs: OrgEntry[];
}

export type WhoamiResult =
  | { status: "ok"; payload: WhoamiPayload }
  | { status: "degraded"; reason: { kind: "git_ai_missing" } };

// ===== Show raw(P11-D)=====

export interface ShowRawPayload {
  commit_sha: string;
  /** git-ai show 上游原文(JSON metadata + `---` + attestations)。UI 用 <pre> 渲染。 */
  raw: string;
}

export type ShowRawResult =
  | { status: "ok"; payload: ShowRawPayload }
  | { status: "degraded"; reason: { kind: "repo_missing" | "git_ai_missing" } };

// ===== Blame(P6)=====
// 字段对齐 git-ai 上游 src/commands/blame.rs:1265-1286 + authorship_log.rs:198-213 + working_log.rs:42-46。
// 关键事实:`lines` BTreeMap 只含 AI 行;非 AI 行不在 map 里 → UI 只能 2 态(AI / 非 AI)。

export interface BlameAgentId {
  tool: string;
  id: string;
  model: string;
}

/** 仓库级累计字段(accepted_lines / overriden_lines)— UI 必标"(整个仓库)"。 */
export interface BlamePromptRecord {
  agent_id: BlameAgentId;
  human_author: string | null;
  messages_url?: string;
  total_additions: number;
  total_deletions: number;
  accepted_lines: number;
  overriden_lines: number;
  custom_attributes?: Record<string, string>;
  other_files: string[];
  commits: string[];
}

export interface BlameMetadata {
  is_logged_in: boolean;
  current_user: string | null;
}

/**
 * 上游 BlameHunk 精简版,字段对齐 git-ai/src/commands/blame.rs:27-57。
 * 每个 hunk 含一段连续行号区间(在当前文件视角)+ commit 元信息。
 * 前端用它做"IDE-style 每行作者列":AI 行优先用 prompt 的 tool/human_author,
 * 非 AI 行 fallback 到这里的 original_author / ai_human_author。
 */
export interface BlameHunk {
  range: [number, number];
  commit_sha: string;
  abbrev_sha: string;
  original_author: string;
  /** Unix 秒(对齐上游;前端按需格式化)。 */
  author_time: number;
  author_tz: string;
  /** 当该 hunk 含 AI 行时,git-ai 解出的人类触发者;非 AI 行通常为 null。 */
  ai_human_author: string | null;
}

export interface BlamePayload {
  /** key 形如 "13" 或 "15-25"(end inclusive);value 是 prompts 的 key。**只含 AI 行**。 */
  lines: Record<string, string>;
  prompts: Record<string, BlamePromptRecord>;
  metadata: BlameMetadata;
  /** 上游 blame_hunks 解析后的全行作者归因(AI 行 + 非 AI 行都覆盖)。 */
  hunks: BlameHunk[];
}

// ===== Diff(任务 #2:单 commit 改动文件 + AI 行)=====
// 字段对齐 src-tauri/src/commands/diff.rs。

/**
 * 单条改动文件。status 透传 `git diff-tree --name-status` 第一列字符:
 * - A=Added / M=Modified / D=Deleted / T=Type changed
 * - R=Renamed / C=Copied(已归到新路径)
 * - U=Unmerged / X=Unknown / B=pairing Broken
 * 前端按字符渲染色块,不做语义抽象。
 */
export interface ChangedFile {
  path: string;
  status: string;
  /** merge / binary 为 null；文本文件即使没有新增行也有统计对象。 */
  line_stats: FileLineStats | null;
}

/** 单文件本 commit 的新增行三桶；删除行不进入 AI 占比分母。 */
export interface FileLineStats {
  additions: number;
  deletions: number;
  ai_additions: number;
  human_additions: number;
  unknown_additions: number;
}

/**
 * 单条 AI 行段(闭区间)。由 git-ai notes 的 attestation `line_ranges`
 * 字符串展开而来 —— 一个 entry 的 "1-10,15,20-25" 会被拆成 3 段。
 * Stats 页据此显示"本 commit 改了 N 行 AI"。
 */
export interface AiLineRef {
  file: string;
  line_start: number;
  line_end: number;
}

export type DiffDegradedReason = { kind: "repo_missing" } | { kind: "invalid_sha"; sha: string };

export type ChangedFilesResult =
  | { status: "ok"; files: ChangedFile[]; is_merge: boolean }
  | { status: "degraded"; reason: DiffDegradedReason };

export type AiLinesResult =
  | { status: "ok"; lines: AiLineRef[] }
  | { status: "degraded"; reason: DiffDegradedReason };

// ===== Branches(E4/E5)=====
// 镜像 src-tauri/src/commands/branches.rs

export interface BranchEntry {
  name: string;
  sha: string;
  is_current: boolean;
}

export type ListBranchesResult =
  | { status: "ok"; current: string | null; branches: BranchEntry[] }
  | { status: "degraded"; reason: { kind: "repo_missing" } };

export type CheckoutDegradedReason =
  | { kind: "repo_missing" }
  | { kind: "dirty_worktree"; files: string[] }
  | { kind: "conflict"; stderr: string }
  | { kind: "not_found"; name: string };

export type CheckoutResult =
  | { status: "ok"; payload: { branch: string; sha: string } }
  | { status: "degraded"; reason: CheckoutDegradedReason };

export type BlameDegradedReason =
  | { kind: "repo_missing" }
  | { kind: "git_ai_missing" }
  | { kind: "no_head" }
  | { kind: "commit_not_found"; sha: string }
  | { kind: "file_not_in_head"; file: string }
  | { kind: "file_too_large"; size: number; limit: number }
  | { kind: "file_binary" }
  /** ref 维度过滤(分支名 / sha / tag)失败:git rev-parse --verify <ref>^{commit} 返非 0。
   *  后端字段名 `ref`(Rust 关键字,serde rename),前端原样匹配。 */
  | { kind: "ref_not_found"; ref: string };
// 删除 no_ai_authorship:它是 payload.lines 空集合,不是 degraded(后端 commands/blame.rs 同步删)

export type BlameResult =
  | { status: "ok"; payload: BlamePayload }
  | { status: "degraded"; reason: BlameDegradedReason };

export type ReadFileResult =
  | { status: "ok"; text: string; size: number }
  | { status: "degraded"; reason: BlameDegradedReason };

export interface FilesListPayload {
  files: string[];
  truncated: boolean;
  total: number;
}

// ===== Notes(P7)=====
// 字段对齐 src-tauri/src/git_ai/notes_ai.rs(镜像上游 authorship/3.0.0 schema)。
// 真源:git-ai/specs/git_ai_standard_v3.0.0.md §1.2
//      git-ai/src/authorship/authorship_log_serialization.rs:28-37
//      git-ai/src/authorship/authorship_log.rs:190-237
// 关键:overriden_lines 是上游 v3.0.0 拼写 errata E-001,字段名原样照搬;v4.x 才会改名。
//      hash 三类:无前缀=prompts;h_=humans;s_ 或 s_::t_=sessions(split("::").next() 取 key)。

export interface NotesAgentId {
  tool: string;
  id: string;
  model: string;
}

export interface NotesPromptRecord {
  agent_id: NotesAgentId;
  human_author: string | null;
  /**
   * 外链:某些 agent 把完整 transcript 存远端。UI 只显示 + 复制,不打开。
   * 上游 PromptRecord 仅此一项与对话相关 —— 内联 `messages` 数组自 v1.3.4 起从 spec 移除
   * (spec E-002),不再镜像/展示。
   */
  messages_url?: string;
  total_additions: number;
  total_deletions: number;
  accepted_lines: number;
  /** 上游 v3.0.0 spec E-001 拼写 errata,字段名原样。 */
  overriden_lines: number;
  custom_attributes?: Record<string, string>;
}

export interface NotesHumanRecord {
  /** "Alice Smith <alice@example.com>" */
  author: string;
}

export interface NotesSessionRecord {
  agent_id: NotesAgentId;
  human_author: string | null;
  custom_attributes?: Record<string, string>;
}

export interface NotesAuthorshipMetadata {
  schema_version: string;
  git_ai_version: string | null;
  base_commit_sha: string;
  prompts: Record<string, NotesPromptRecord>;
  humans: Record<string, NotesHumanRecord>;
  sessions: Record<string, NotesSessionRecord>;
}

export interface NotesAttestationEntry {
  hash: string;
  /** 原样字符串(如 "1-10,15-20")— viewer 不擅自 re-sort。 */
  line_ranges: string;
}

export interface NotesFileAttestation {
  file_path: string;
  entries: NotesAttestationEntry[];
}

export interface NotesAuthorshipLog {
  attestations: NotesFileAttestation[];
  metadata: NotesAuthorshipMetadata;
}

export interface NoteListEntry {
  commit_sha: string;
  short_sha: string;
  note_oid: string;
  /** ISO-8601 带时区(%cI)。 */
  committed_at: string;
  subject: string;
}

export interface NotesListPayload {
  repo_path: string;
  /** HEAD commit sha(用于判定"当前选中 === HEAD"以启用 Blame 跳转)。 */
  head_sha: string | null;
  notes: NoteListEntry[];
  /**
   * notes ref 引用但本地仓库不存在的 commit sha。
   * git 设计上 notes 与 commits 是独立 namespace,常见原因:协作 push 了 notes 但 commit 未抵达本地、
   * shallow clone、rebase 留孤儿 sha。**不**降级显示这些条目(无 subject/date 占位会误导),
   * 由 Notes 页 banner 提示用户 fetch 或联系仓库维护者。
   */
  unreachable_shas: string[];
}

export type NotesDegradedReason = { kind: "repo_missing" } | { kind: "no_notes_in_repo" };

export type NotesListResult =
  | { status: "ok"; payload: NotesListPayload }
  | { status: "degraded"; reason: NotesDegradedReason };

export interface ShowNotePayload {
  commit_sha: string;
  log: NotesAuthorshipLog;
}

export type ShowNoteResult =
  | { status: "ok"; payload: ShowNotePayload }
  | { status: "degraded"; reason: NotesDegradedReason };

/** attestation hash 分类(前端 lookup 用)。 */
export type NotesHashKind = "prompt" | "human" | "session";

export function classifyNotesHash(hash: string): NotesHashKind {
  if (hash.startsWith("h_")) return "human";
  if (hash.startsWith("s_")) return "session";
  return "prompt";
}

/** 复合 hash `s_xxx::t_yyy` 取 session_key(spec 上游 :278)。 */
export function sessionKeyOf(hash: string): string {
  const idx = hash.indexOf("::");
  return idx < 0 ? hash : hash.slice(0, idx);
}

/**
 * 解析 `NotesAttestationEntry.line_ranges` 字符串(上游 `authorship_log_serialization.rs:576-598`
 * `format_line_ranges` 真源):逗号分隔,每段是 `<n>`(Single)或 `<start>-<end>`(Range);
 * 段之间已按 start 升序;Single(l) 视作 `[l, l]`。
 *
 * # 失败语义
 * 任何段不符合 `^\d+$` 或 `^\d+-\d+$`,或 start > end,整个返 `[]` (no-fallback fail-fast)。
 * 空字符串返 `[]`。
 *
 * # 例子
 * - `"5"`               → `[[5, 5]]`
 * - `"1-10"`            → `[[1, 10]]`
 * - `"5,10-15,20-25"`   → `[[5, 5], [10, 15], [20, 25]]`
 * - `"abc"`             → `[]`(fail)
 * - `"10-5"`            → `[]`(start > end fail)
 */
export function parseLineRanges(s: string): Array<[number, number]> {
  const trimmed = s.trim();
  if (!trimmed) return [];
  const out: Array<[number, number]> = [];
  for (const seg of trimmed.split(",")) {
    const part = seg.trim();
    if (!part) continue;
    const m = /^(\d+)(?:-(\d+))?$/.exec(part);
    if (!m) return [];
    const a = Number(m[1]);
    const b = m[2] !== undefined ? Number(m[2]) : a;
    if (!Number.isFinite(a) || !Number.isFinite(b) || a < 1 || b < a) return [];
    out.push([a, b]);
  }
  return out;
}

// ===== Checkpoints(P8)=====
// 字段对齐 src-tauri/src/repo/working_logs.rs(镜像上游 working_log.rs / attribution_tracker.rs)。
// 真源:git-ai/src/authorship/working_log.rs:8-167
//      git-ai/src/authorship/attribution_tracker.rs:25-65
//      git-ai/src/git/repo_storage.rs:33-145, 225-485
// 关键事实:CheckpointKind 在 jsonl 里是 PascalCase("AiAgent"/"Human"/"AiTab"/"KnownHuman"),
//          base_commit_sha = HEAD sha 本身(不是 parent)。

export type CheckpointKind = "Human" | "AiAgent" | "AiTab" | "KnownHuman";

export interface CheckpointAgentId {
  tool: string;
  id: string;
  model: string;
}

export interface CheckpointKnownHumanMetadata {
  editor: string;
  editor_version: string;
  extension_version: string;
}

export interface CheckpointLineStats {
  additions: number;
  deletions: number;
  additions_sloc: number;
  deletions_sloc: number;
}

export interface CheckpointAttribution {
  start: number;
  end: number;
  author_id: string;
  /** ms-since-epoch,实际 ~41 bit 远小于 JS Number 53 bit 安全位。 */
  ts: number;
}

export interface CheckpointLineAttribution {
  start_line: number;
  end_line: number;
  author_id: string;
  overrode?: string;
}

export interface CheckpointEntry {
  file: string;
  blob_sha: string;
  attributions: CheckpointAttribution[];
  line_attributions: CheckpointLineAttribution[];
}

export interface Checkpoint {
  kind: CheckpointKind;
  diff: string;
  author: string;
  entries: CheckpointEntry[];
  /** unix seconds(working_log.rs:148-151 用 `as_secs()`)。 */
  timestamp: number;
  agent_id: CheckpointAgentId | null;
  agent_metadata: Record<string, string> | null;
  line_stats: CheckpointLineStats;
  api_version: string;
  git_ai_version?: string;
  known_human_metadata?: CheckpointKnownHumanMetadata;
  trace_id?: string;
}

export interface CheckpointsPayload {
  repo_path: string;
  head_sha: string;
  checkpoints: Checkpoint[];
}

export type CheckpointsDegradedReason =
  | { kind: "repo_missing" }
  | { kind: "no_head" }
  | { kind: "git_ai_missing" }
  | { kind: "working_logs_dir_missing" };

export type CheckpointsResult =
  | { status: "ok"; payload: CheckpointsPayload }
  | { status: "degraded"; reason: CheckpointsDegradedReason };

export interface GitStatusFile {
  path: string;
  status: string;
}

export interface DirtyFilesPayload {
  files: GitStatusFile[];
  total: number;
}

export type MockPreset = "human" | "mock_ai" | "mock_known_human";

// ===== Logs(P9)=====
// 字段对齐 src-tauri/src/commands/logs.rs::LogKind / LogFilePayload。
// LogKind 是 serde internally tagged + snake_case。

export type LogKind = { kind: "app" };

export interface LogFilePayload {
  path: string;
  exists: boolean;
  size: number;
  mtime_unix_ms: number | null;
  truncated_head: boolean;
  content: string;
}

/** 流式日志 event payload(订阅 logs://debug/<job_id>),复用 install/checkpoint 同形结构。 */
export interface LogStreamEvent {
  stream: "stdout" | "stderr" | "exit";
  line?: string;
  code?: number;
  timeout?: boolean;
  ts: number;
}

// ===== People breakdown(按人 + 时间范围,P12)=====
// 字段对齐 src-tauri/src/commands/people.rs。identity_key = author_email.toLowerCase()。

/** 单 commit 引用,前端在 PeopleRow 展开里点击跳转 Stats。 */
export interface PersonCommitRef {
  sha: string;
  short: string;
  authored_at: string;
  subject: string;
  is_merge: boolean;
  ai_additions: number;
  human_additions: number;
  unknown_additions: number;
}

export interface PersonRow {
  /** `author_email.toLowerCase()`;identity 主键(不引 mailmap)。 */
  identity_key: string;
  /** 显示名:取该 identity 下最近一次 commit 的 `%an`。 */
  author_name: string;
  /** 原样邮箱(未 lowercase),做大小写显示对齐。 */
  author_email: string;
  commits: number;
  human_additions: number;
  unknown_additions: number;
  ai_additions: number;
  /** `human + unknown + ai`(3 桶并列,与上游 stats.rs:114 一致)。 */
  total_additions: number;
  commit_refs: PersonCommitRef[];
}

export interface PeopleTotals {
  commits: number;
  human_additions: number;
  unknown_additions: number;
  ai_additions: number;
  total_additions: number;
}

export interface PeopleBreakdownPayload {
  range: TimeRange;
  range_start_unix_ms: number;
  range_end_unix_ms: number;
  /** 后端按 identity_key 字典序输出;前端可在此基础上做交互式排序。 */
  rows: PersonRow[];
  grand_total: PeopleTotals;
  failed_shas: string[];
  truncated: boolean;
  cache_hits: number;
  took_ms: number;
}

export type PeopleDegradedKind = "repo_missing" | "git_ai_missing";

export type PeopleBreakdownResult =
  | { status: "ok"; payload: PeopleBreakdownPayload }
  | { status: "degraded"; reason: { kind: PeopleDegradedKind } };
