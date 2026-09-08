import type { QueryClient } from "@tanstack/react-query";

import type { TimeRange } from "./types";

/**
 * 把 TimeRange 序列化为稳定 queryKey 片段。
 *
 * # 为什么集中
 * Dashboard 用 `["history", repoPath, rangeKey(range)]` 作为缓存 key;LowAiShareWatcher 也
 * 走同一缓存。两边必须用同一个函数,否则 queryKey 不一致 → 数据不共享 → 同一窗口跑两份子进程。
 */
export function rangeKey(r: TimeRange): string {
  return JSON.stringify(r);
}

/**
 * 把聚合仓库集合序列化为稳定 queryKey 片段:排序后 stringify ⇒ 顺序无关(勾选顺序不同、
 * 集合相同 → 同一 key)。与 rangeKey 同模式。Dashboard(M4)用
 * `["history_agg", reposKey(repos), rangeKey(range)]`。入参应是后端已 canonicalize 的路径。
 */
export function reposKey(paths: string[]): string {
  return JSON.stringify([...paths].sort());
}

/**
 * 切换"当前仓库"或"当前 HEAD"后,所有依赖 HEAD 的页面/数据都要刷新。
 *
 * # 设计为什么是逃生通道
 * 历史上 TopBar 的 BranchSwitcher 在 onSuccess 里手动列了 10 个 invalidate;
 * Repo 页 selectRepo 只列了 3 个;App 启动 restoreLastRepo 只列了 2 个 ——
 * 三处漂移导致切仓库后右上分支下拉 / Dashboard / Blame 都是上一个仓库的数据。
 *
 * 抽出共用入口让三个入口走它,挡住"漏 invalidate"这一类 bug。但这是**逃生通道**,
 * 不是常规手段:任何新增 query 的人都得记得来这里加一行,**没有编译期保护**。
 * 长期方案是改用 `queryKey: ["repo", repoPath, ...]` 前缀化,然后一行
 * `qc.invalidateQueries({ queryKey: ["repo"] })` 全部失效。该重构机械但触及 ~40 处,
 * 见 review #C 的建议;暂未排入。
 *
 * # 谁应该被 invalidate
 * - **repo 元信息**:current_repo / recent_repos / list_branches
 * - **HEAD 派生数据**:blame_at_commit / read_file_at_commit(Stats 逐行弹窗)/ commit_stats /
 *   commit_status / recent_commits_with_stats / list_ai_notes / history
 * - **环境/诊断**:diagnose_environment / effective_ignore_patterns
 *
 * # 谁不应该被 invalidate
 * - `hooks_status`:app-scoped(读全局 Claude settings.json),与 repo 无关。
 *   且 TopBar.hooksQ 已有 refetchInterval=15s,误 invalidate 会触发 schtasks 子进程探测,纯浪费。
 * - `app_settings / scan_roots / repos / git_ai_config / whoami / resolve_git_ai_path`:
 *   全部 app-scoped,与 repo 选择无关。
 */
export function invalidateRepoScopedQueries(qc: QueryClient): void {
  // 1. 刷新当前仓库与分支信息
  qc.invalidateQueries({ queryKey: ["current_repo"] });
  qc.invalidateQueries({ queryKey: ["current_git_user_email"] });
  // People 页用的独立 key,与 ["current_repo"] 名义重复但是两个 query 实例。
  // 不刷它会让该页 repoPath 卡在旧值,所有依赖 repoPath 的子 query 都走错路径。
  qc.invalidateQueries({ queryKey: ["current_repo_path"] });
  qc.invalidateQueries({ queryKey: ["recent_repos"] });
  qc.invalidateQueries({ queryKey: ["list_branches"] });
  // 2. 刷新 HEAD 与工作区派生数据
  qc.invalidateQueries({ queryKey: ["blame_at_commit"] });
  qc.invalidateQueries({ queryKey: ["read_file_at_commit"] });
  qc.invalidateQueries({ queryKey: ["commit_stats"] });
  // 未提交工作树摘要(WorkingDirSummary)走 commit_status;漏了它会导致切仓/分支后
  // "未提交 xx 行" 仍显示上一个仓库/分支的旧值(A1)。
  qc.invalidateQueries({ queryKey: ["commit_status"] });
  qc.invalidateQueries({ queryKey: ["recent_commits_with_stats"] });
  qc.invalidateQueries({ queryKey: ["list_ai_notes"] });
  qc.invalidateQueries({ queryKey: ["list_checkpoints"] });
  qc.invalidateQueries({ queryKey: ["history"] });
  qc.invalidateQueries({ queryKey: ["people"] });
  // 3. 刷新仓库环境诊断
  qc.invalidateQueries({ queryKey: ["diagnose_environment"] });
  qc.invalidateQueries({ queryKey: ["effective_ignore_patterns"] });
}
