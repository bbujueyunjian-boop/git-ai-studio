import { invoke } from "@tauri-apps/api/core";
import type {
  AgentCli,
  AgentHookStatus,
  AgentKind,
  AiLinesResult,
  ApplyResult,
  AppSettings,
  AppSettingsPatch,
  ChangedFilesResult,
  ClaudeSettingsView,
  BlameResult,
  CacheScope,
  CommitBrief,
  DaemonRepairResult,
  DiagnosticOverview,
  FilesListPayload,
  HistoryResult,
  PeopleBreakdownResult,
  RangeSummaryResult,
  RecentCommitsResult,
  TimeRange,
  ReadFileResult,
  GitAiConfig,
  GitAiConfigPatch,
  HooksMode,
  HooksStatus,
  InstallHistoryEntry,
  InstalledVersion,
  CheckoutResult,
  CheckpointsResult,
  DirtyFilesPayload,
  EffectiveIgnorePatternsResult,
  ListBranchesResult,
  LogFilePayload,
  LogKind,
  MockPreset,
  NotesListResult,
  NpmStatus,
  AggregateRepoEntry,
  AggregateHistoryResult,
  AggregateWorkingStatusResult,
  ReleasesPayload,
  RepoEntry,
  SettingsBackup,
  ShowNoteResult,
  ShowRawResult,
  StatsResult,
  WhoamiResult,
} from "./types";

/**
 * 统一的 invoke 封装。
 * - 错误统一抛 Error,前端用 try/catch 或 React Query 的 isError 处理。
 * - 字符串错误透传;对象错误 JSON 化以便 toast 展示。
 */
export async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await invoke<T>(cmd, args);
  } catch (e) {
    throw new Error(typeof e === "string" ? e : JSON.stringify(e), { cause: e });
  }
}

// P0 联调
export const ping = () => call<string>("ping");
export const resolveGitAiPath = () => call<[boolean, string]>("resolve_git_ai_path");

// 诊断
export const diagnoseEnvironment = (force = false) =>
  call<DiagnosticOverview>("diagnose_environment", { force });
export const invalidateDiagnosticCache = () => call<void>("invalidate_diagnostic_cache");
export const checkAgentHooks = (agent: AgentKind) =>
  call<AgentHookStatus>("check_agent_hooks", { agent });

// 仓库
export const discoverRepos = (roots: string[], maxDepth?: number) =>
  call<RepoEntry[]>("discover_repos", { roots, maxDepth });
export const selectRepo = (path: string) => call<RepoEntry>("select_repo", { path });
export const currentRepo = () => call<RepoEntry | null>("current_repo");
export const currentGitUserEmail = () => call<string | null>("current_git_user_email");
export const detectDirty = (path: string) => call<boolean | null>("detect_dirty", { path });
export const listRecentRepos = () => call<string[]>("list_recent_repos");
export const clearRecentRepos = () => call<number>("clear_recent_repos");
export const listScanRoots = () => call<string[]>("list_scan_roots");
export const setScanRoots = (roots: string[]) => call<void>("set_scan_roots", { roots });
/** 跨仓聚合的显式仓库集合(M1)。get 返回带有效性标注;set 做 normalize + 去重持久化。 */
export const getAggregateRepos = () => call<AggregateRepoEntry[]>("get_aggregate_repos");
export const setAggregateRepos = (repos: string[]) => call<void>("set_aggregate_repos", { repos });
export const restoreLastRepo = () => call<RepoEntry | null>("restore_last_repo");
export const openInExplorer = (path: string) => call<void>("open_in_explorer", { path });

// Install
export const listReleases = (force = false) => call<ReleasesPayload>("list_releases", { force });
export const getInstalledVersion = () => call<InstalledVersion>("get_installed_version");
export const isInstallRunning = () => call<string | null>("is_install_running");
export const installGitAi = (jobId: string, version?: string) =>
  call<number>("install_git_ai", { jobId, version: version ?? null });
export const uninstallGitAi = (jobId: string, confirmToken: string) =>
  call<void>("uninstall_git_ai", { jobId, confirmToken });
export const getGitAiConfig = () => call<GitAiConfig>("get_git_ai_config");
export const setGitAiConfig = (patch: GitAiConfigPatch) =>
  call<GitAiConfig>("set_git_ai_config", { patch });
export const setAutoUpdate = (enabled: boolean) =>
  call<GitAiConfig>("set_auto_update", { enabled });
export const getInstallHistory = () => call<InstallHistoryEntry[]>("install_history");

// AI 编码 CLI(Claude Code / Codex)的 npm 装卸:复用 install://<job>/log 流式日志协议 + install_lock。
/** 强制重读登录环境真实 PATH(供"重新检测"触发,使运行期才装的 Node 不重启即可识别) */
export const refreshPathEnv = () => call<void>("refresh_path_env");
export const detectNpm = () => call<NpmStatus>("detect_npm");
export const detectAgentCli = (agent: AgentCli) =>
  call<InstalledVersion>("detect_agent_cli", { agent });
export const installAgentCli = (jobId: string, agent: AgentCli, version?: string) =>
  call<number>("install_agent_cli", { jobId, agent, version: version ?? null });
export const uninstallAgentCli = (jobId: string, agent: AgentCli, confirmToken: string) =>
  call<void>("uninstall_agent_cli", { jobId, agent, confirmToken });

// Hooks
export const getHooksStatus = () => call<HooksStatus>("get_hooks_status");
export const readClaudeSettings = () => call<ClaudeSettingsView>("read_claude_settings");
export const listSettingsBackups = () => call<SettingsBackup[]>("list_settings_backups");
export const restoreClaudeSettings = (jobId: string, backupPath: string) =>
  call<void>("restore_claude_settings", { jobId, backupPath });
export const claudeSettingsMerge = (jobId: string, mode: HooksMode) =>
  call<ApplyResult>("claude_settings_merge", { jobId, mode });
export const diagnoseGitAiDaemon = () =>
  call<import("./types").DaemonHealth>("diagnose_git_ai_daemon");
export const repairGitAiDaemon = () => call<DaemonRepairResult>("repair_git_ai_daemon");
export const installHooksOfficial = (jobId: string) =>
  call<number>("install_hooks_official", { jobId });
/**
 * 为单个 AI agent 触发 hook 修复(P0b)。
 *
 * 现状(2026-05):上游 git-ai `install-hooks` 子命令未提供 `--agents` 过滤参数,
 * 后端实际仍调 `git-ai install`(全装,idempotent)。对已正确配置的其它 agent 是 no-op,
 * 只对该 agent + 任何其它缺失项做补全。长期方向:推上游加 `--agents <id>` 后切到精确单装。
 *
 * 与 `installHooksOfficial` 的区别:UI 语义是"为该 agent 修复",日志前缀也会标明
 * 目标 agent;两者底层调同一 CLI,锁串行。
 */
export const installHooksForAgent = (jobId: string, agent: AgentKind) =>
  call<number>("install_hooks_for_agent", { jobId, agent });

// Stats(P4)
export const getCommitStats = (sha?: string | null) =>
  call<StatsResult>("get_commit_stats", { sha: sha ?? null });
export const getCommitStatus = () => call<StatsResult>("get_commit_status");
export const listRecentCommits = (maxCount: number) =>
  call<CommitBrief[]>("list_recent_commits", { maxCount });

// History / Dashboard(P5 / P11-A 时间筛选)
export const getHistory = (range: TimeRange) => call<HistoryResult>("get_history", { range });
/** 提交归因 commit 浏览器:最近 N 个 commit + 各自 AI 三桶(单仓 current_repo,复用 get_history 缓存)。 */
export const listRecentCommitsWithStats = (maxCount: number) =>
  call<RecentCommitsResult>("list_recent_commits_with_stats", { maxCount });
/**
 * 跨仓聚合历史(M2):读 aggregate_repos,Dashboard 默认视图数据源。单仓 getHistory 不变。
 * `onlyMine`(默认 true):「只看我」口径,后端逐仓按生效的 git user.email 过滤 commit。
 */
export const getAggregateHistory = (range: TimeRange, onlyMine: boolean) =>
  call<AggregateHistoryResult>("get_aggregate_history", { range, onlyMine });
/** 跨仓「本地未提交」快照(`git ai status` 求和)。无参、不缓存、天然只看我。 */
export const getAggregateWorkingStatus = () =>
  call<AggregateWorkingStatusResult>("get_aggregate_working_status");
// range 聚合(hook 覆盖率)独立命令:固有耗时长 + 自带缓存,Dashboard 独立 query 驱动。
export const getRangeSummary = (range: TimeRange) =>
  call<RangeSummaryResult>("get_range_summary", { range });
export const clearStatsCache = (scope: CacheScope) => call<number>("clear_stats_cache", { scope });

// People breakdown(P12 按人 + 时间范围)
export const getPeopleBreakdown = (range: TimeRange) =>
  call<PeopleBreakdownResult>("get_people_breakdown", { range });

// Blame(P6/P10):后端使用上游 `git-ai blame-analysis --json '<payload>'` 机器命令。
export const getBlame = (file: string, ranges: Array<[number, number]> | null) =>
  call<BlameResult>("get_blame", { file, ranges });
export const listFilesAtHead = (sha: string | null) =>
  call<FilesListPayload>("list_files_at_head", { sha });
export const readFileAtHead = (sha: string | null, file: string) =>
  call<ReadFileResult>("read_file_at_head", { sha, file });

// Blame ref 维度(分支 / commit sha 过滤):ref=null 等价 HEAD,与上面三个旧入口同源。
// 后端参数名是 Rust 关键字 `ref`,Tauri JSON 协议接收时原样传入即可。
export const getBlameAtRef = (
  ref: string | null,
  file: string,
  ranges: Array<[number, number]> | null,
) => call<BlameResult>("get_blame_at_ref", { ref, file, ranges });
export const listFilesAtRef = (ref: string | null) =>
  call<FilesListPayload>("list_files_at_ref", { ref });
export const readFileAtRef = (ref: string | null, file: string) =>
  call<ReadFileResult>("read_file_at_ref", { ref, file });

// Notes(P7)
export const listAiNotes = () => call<NotesListResult>("list_ai_notes");
export const showAiNote = (sha: string) => call<ShowNoteResult>("show_ai_note", { sha });

// Diff(任务 #2:Dashboard/提交归因 跳转代码 — 改动文件 + AI 行)
export const listChangedFilesInCommit = (sha: string) =>
  call<ChangedFilesResult>("list_changed_files_in_commit", { sha });
export const listAiLinesInCommit = (sha: string) =>
  call<AiLinesResult>("list_ai_lines_in_commit", { sha });

// Checkpoints(P8)
export const listCheckpoints = (sha?: string | null) =>
  call<CheckpointsResult>("list_checkpoints", { sha: sha ?? null });
export const isMockRunning = () => call<string | null>("is_mock_running");
export const gitStatusPorcelain = () => call<DirtyFilesPayload>("git_status_porcelain");
export const mockCheckpoint = (
  jobId: string,
  preset: MockPreset,
  pathspecs: string[],
  confirmToken: string,
) =>
  call<number>("mock_checkpoint", {
    jobId,
    preset,
    pathspecs,
    confirmToken,
  });

// Logs(P9)
export const readLogFile = (kind: LogKind, maxBytes?: number) =>
  call<LogFilePayload>("read_log_file", { kind, maxBytes: maxBytes ?? null });
export const runGitAiDebugReport = (jobId: string) =>
  call<number>("run_git_ai_debug_report", { jobId });
export const openLogDir = (kind: LogKind) => call<void>("open_log_dir", { kind });

// Auth / Show raw(P11-D)
// get_whoami 仍由诊断页 quickFix「whoami-error」规则使用(登录态作为诊断项,非登录 UI)。
export const getWhoami = () => call<WhoamiResult>("get_whoami");
export const getShowRaw = (sha: string) => call<ShowRawResult>("get_show_raw", { sha });

// Branches(E4/E5)
export const listBranches = () => call<ListBranchesResult>("list_branches");
export const checkoutBranch = (name: string) => call<CheckoutResult>("checkout_branch", { name });

// Ignore patterns
export const listEffectiveIgnorePatterns = () =>
  call<EffectiveIgnorePatternsResult>("list_effective_ignore_patterns");

// Settings
export const getAppSettings = () => call<AppSettings>("get_app_settings");
export const setAppSettings = (patch: AppSettingsPatch) =>
  call<AppSettings>("set_app_settings", { patch });

// 应用「开机自启」:真源为操作系统登录项,非 app config(不落 config.json)。
export const getAutoLaunchStatus = () => call<boolean>("get_auto_launch_status");
export const setAutoLaunch = (enabled: boolean) => call<boolean>("set_auto_launch", { enabled });
