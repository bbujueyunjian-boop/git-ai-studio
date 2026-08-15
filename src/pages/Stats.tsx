// 提交归因(Stats 页):单仓 commit 浏览器。
//
// # 形态(借鉴 IDEA Git Log,但每栏锚在 AI 归因)
// 顶部指标看板(范围聚合大数字)+ 过滤条(搜索 + 只看我)+ 两栏(commit 列表 | 选中 commit 详情)。
// 点详情里的改动文件 → 弹窗看该文件逐行归因(哪个模型 / 还是人写)。
//
// # 权威 schema 来源
// - 字段定义:`git-ai/src/authorship/stats.rs:9-33`;公式 total = human + unknown + ai(stats.rs:114)
// - per-commit 数据来自 `list_recent_commits_with_stats`(复用 get_history 的 SQLite 缓存)
// - 文件 AI 占比由 note 行范围与本 commit diff 新增行求交后派生，删除行不进入分母
// - commit 级 AI% 用真实三桶派生;merge 行为见 specs §2.2(ai_accepted 恒 0)

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Activity,
  Copy,
  FileText,
  FolderOpen,
  GitBranch,
  GitMerge,
  Info,
  Loader2,
  Maximize2,
  Minimize2,
  RefreshCw,
  Search,
  Sparkles,
  X,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";

import { BlameCodeView, type BlameLineClickEvent } from "../components/BlameCodeView";
import { BlamePromptDetails } from "../components/BlamePromptDetails";
import { ChangedFilesPanel } from "../components/ChangedFilesList";
import { CommitAttributionList } from "../components/CommitAttributionList";
import { EmptyState } from "../components/EmptyState";
import { FileDegradedCard } from "../components/FileDegradedCard";
import { FormulaPopover } from "../components/FormulaPopover";
import { MetricCard } from "../components/MetricCard";
import { ScopeToggle } from "../components/ScopeToggle";
import { SplitPane } from "../components/Layout/SplitPane";
import { StatsBar } from "../components/StatsBar";
import { WorkingDirSummary, WORKING_DIR_SHA_TOKEN } from "../components/WorkingDirSummary";
import { Card } from "../components/ui/CardPanel";
import { Dialog } from "../components/ui/DialogShell";
import { Popover, PopoverContent, PopoverTrigger } from "../components/ui/PopoverPanel";
import {
  currentGitUserEmail,
  currentRepo,
  getBlameAtRef,
  getShowRaw,
  listRecentCommitsWithStats,
  readFileAtRef,
} from "../lib/api";
import { deriveBlameLines, parseLRange } from "../lib/blameLines";
import { detectTheme } from "../lib/chartColors";
import { cn } from "../lib/cn";
import { commitTotal, deriveRates, formatInt, formatPercent } from "../lib/formulas";
import { useNotesUpdated } from "../lib/useNotesUpdated";
import type {
  BlameResult,
  CommitWithStats,
  NoteKind,
  ReadFileResult,
  RecentCommitsResult,
  ShowRawResult,
  ToolModelStats,
} from "../lib/types";
import { useRouter } from "../router";

/** 一次拉的最近 commit 数。复用 get_history 的 per-commit 缓存,冷启首次才需子进程。 */
const COMMIT_LIST_LIMIT = 100;
const STATS_STALE_TIME_SECONDS = 30;
const STALE_TIME_MS = STATS_STALE_TIME_SECONDS * 1000;

/** 三桶求和 → 范围聚合(指标看板用)。失败 commit 以 0 桶占位,不污染(其 total=0)。 */
function aggregate(commits: CommitWithStats[]): {
  human: number;
  unknown: number;
  ai: number;
  total: number;
  aiPct: number | null;
  authors: number;
} {
  let human = 0;
  let unknown = 0;
  let ai = 0;
  const authors = new Set<string>();
  for (const c of commits) {
    human += c.stats.human_additions;
    unknown += c.stats.unknown_additions;
    ai += c.stats.ai_additions;
    authors.add(c.author_email.toLowerCase());
  }
  const total = human + unknown + ai;
  return { human, unknown, ai, total, aiPct: total > 0 ? ai / total : null, authors: authors.size };
}

export default function StatsPage() {
  const { t } = useTranslation();
  const router = useRouter();
  const qc = useQueryClient();
  const [onlyMine, setOnlyMine] = useState(true); // 默认只看我(ADR-012:单开发者本机工具本分)
  const [query, setQuery] = useState("");
  const [showSha, setShowSha] = useState<string | null>(null);

  const selectedSha = router.params || undefined;
  // 逐行归因弹窗由 URL 驱动(单一真相、可深链、刷新保留 + Notes/Checkpoints 跳转入口):
  // #/stats/<sha>?file=<路径>&L=<起>-<止>。点改动文件写入「当前展示 commit」的 sha + file(见 onOpenFile),
  // 关弹窗清掉 query。注意写 sha 必须用展示中的 selected.sha,而非 URL 锚定的 selectedSha —— 无参进入(Rail)时
  // params 为空,若不把 selected.sha 落到 params,渲染守卫 {openFile && selectedSha} 永远 false,点文件无反应。
  const openFile = router.query.get("file");
  const openRange = parseLRange(router.query.get("L"));
  const closeOpenFile = () => router.navigate("stats", selectedSha);
  const isWorking = selectedSha === WORKING_DIR_SHA_TOKEN;

  const repoQ = useQuery({
    queryKey: ["current_repo"],
    queryFn: () => currentRepo(),
    staleTime: STALE_TIME_MS,
  });
  const repoPath = repoQ.data?.path ?? null;
  const headSha = repoQ.data?.head_sha ?? null;

  const userEmailQ = useQuery({
    queryKey: ["current_git_user_email", repoPath],
    queryFn: currentGitUserEmail,
    staleTime: STALE_TIME_MS,
  });
  const userEmail = userEmailQ.data?.toLowerCase() ?? null;

  const commitsQ = useQuery<RecentCommitsResult>({
    queryKey: ["recent_commits_with_stats", repoPath, COMMIT_LIST_LIMIT],
    queryFn: () => listRecentCommitsWithStats(COMMIT_LIST_LIMIT),
    staleTime: STALE_TIME_MS,
  });

  // 提交后(refs/notes/ai 变化)立即失效列表缓存,不等用户手动刷新。
  useNotesUpdated(
    repoPath,
    useCallback(() => {
      void qc.invalidateQueries({ queryKey: ["recent_commits_with_stats", repoPath] });
    }, [qc, repoPath]),
  );

  const refresh = () => {
    void qc.invalidateQueries({ queryKey: ["recent_commits_with_stats", repoPath] });
  };

  // ===== degraded 空态 =====
  if (commitsQ.data?.status === "degraded") {
    const kind = commitsQ.data.reason.kind;
    const keyPrefix =
      kind === "repo_missing" ? "stats.degraded.repoMissing" : "stats.degraded.gitAiMissing";
    return (
      <EmptyState
        Icon={kind === "repo_missing" ? FolderOpen : Activity}
        title={t(`${keyPrefix}.title` as never)}
        description={t(`${keyPrefix}.description` as never)}
        ctaLabel={t(`${keyPrefix}.cta` as never)}
        onCta={() => router.navigate(kind === "repo_missing" ? "repo" : "install")}
      />
    );
  }
  if (commitsQ.isLoading) {
    return (
      <div className="flex h-full items-center justify-center text-sm text-muted-foreground">
        <Loader2 className="mr-2 h-4 w-4 animate-spin" />
        {t("stats.loading")}
      </div>
    );
  }
  if (commitsQ.isError) {
    return (
      <div className="p-6">
        <div className="rounded-md border border-danger bg-danger-muted p-4 text-sm text-danger">
          {t("stats.error", { message: (commitsQ.error as Error).message })}
        </div>
      </div>
    );
  }

  const payload = commitsQ.data?.status === "ok" ? commitsQ.data.payload : null;
  const allCommits = payload?.commits ?? [];
  const failedShas = new Set(payload?.failed_shas ?? []);

  const q = query.trim().toLowerCase();
  const filtered = allCommits.filter((c) => {
    if (onlyMine && userEmail && c.author_email.toLowerCase() !== userEmail) return false;
    if (q && !c.subject.toLowerCase().includes(q) && !c.sha.toLowerCase().includes(q)) return false;
    return true;
  });
  const filteredFailedShas = filtered
    .filter((commit) => failedShas.has(commit.sha))
    .map((commit) => commit.sha);

  const agg = aggregate(filtered);

  // 选中 commit:URL 锚定;不在过滤结果里(被过滤掉 / 旧 sha)则回退到列表首项(HEAD),不强改 URL。
  const selected = isWorking
    ? null
    : (filtered.find((c) => c.sha === selectedSha) ??
      (selectedSha ? allCommits.find((c) => c.sha === selectedSha) : undefined) ??
      filtered[0] ??
      null);

  return (
    <div className="absolute inset-0 flex flex-col overflow-hidden bg-background">
      <MetricsBar
        agg={agg}
        count={filtered.length}
        repoName={repoQ.data?.name ?? "—"}
        branch={repoQ.data?.head_branch ?? null}
        loadedCount={allCommits.length}
        limit={COMMIT_LIST_LIMIT}
        truncated={payload?.truncated ?? false}
        onlyMine={onlyMine && userEmail !== null}
        hasSearch={q.length > 0}
        failedCount={filteredFailedShas.length}
      />

      {/* 过滤条:搜索 + 只看我 + 右侧统计 / 刷新 */}
      <header className="flex h-12 shrink-0 flex-wrap items-center gap-2 border-b border-border px-3">
        <div className="relative">
          <Search className="pointer-events-none absolute left-2 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
          <input
            type="search"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder={t("stats.filter.searchPlaceholder")}
            className="w-56 rounded-md border border-border bg-card py-1 pl-7 pr-2 text-xs focus:border-primary focus:outline-hidden focus:ring-1 focus:ring-ring"
          />
        </div>
        <ScopeToggle onlyMine={onlyMine} onChange={setOnlyMine} />
        {onlyMine && !userEmail && (
          <span className="text-[11px] text-warning-foreground dark:text-warning">
            {t("stats.filter.noUserEmail")}
          </span>
        )}
        <div className="ml-auto flex items-center gap-3 text-xs text-muted-foreground">
          <span>
            {t("stats.summary.commitCount", { count: filtered.length })}
            {payload?.truncated && (
              <span
                className="ml-1 text-warning-foreground dark:text-warning"
                title={t("stats.summary.truncatedTitle")}
              >
                {t("stats.summary.truncatedBadge", { limit: COMMIT_LIST_LIMIT })}
              </span>
            )}
          </span>
          {filteredFailedShas.length > 0 && (
            <span className="text-danger" title={filteredFailedShas.join("\n")}>
              {t("stats.summary.failedCount", { count: filteredFailedShas.length })}
            </span>
          )}
          <span className="font-medium text-primary">AI {formatPercent(agg.aiPct)}</span>
          <button
            type="button"
            onClick={refresh}
            disabled={commitsQ.isFetching}
            className="inline-flex items-center gap-1 rounded-md border border-border bg-card px-2 py-1 text-muted-foreground hover:bg-muted hover:text-foreground disabled:opacity-50"
          >
            <RefreshCw className={cn("h-3 w-3", commitsQ.isFetching && "animate-spin")} />
            {commitsQ.isFetching
              ? t("stats.cacheHint.refreshing")
              : t("stats.cacheHint.refreshNow")}
          </button>
        </div>
      </header>

      {/* 两栏:commit 列表(左,可拖拽改宽)| 选中详情(右,占满剩余 → 更宽,放得下改动文件/逐行)。
          列表是固定可拖宽,详情 flex-1 吃掉剩余空间,解决"左大右小"。宽度持久化。 */}
      <SplitPane
        className="min-h-0 flex-1"
        storageKey="stats.commitList.width"
        defaultLeftWidth={440}
        minLeftWidth={320}
        maxLeftWidth={680}
        left={
          <div className="h-full overflow-y-auto">
            <CommitList
              commits={filtered}
              failedShas={failedShas}
              selectedSha={selected?.sha}
              isWorking={isWorking}
              onSelect={(sha) => router.navigate("stats", sha)}
              onSelectWorking={() => router.navigate("stats", WORKING_DIR_SHA_TOKEN)}
            />
          </div>
        }
        right={
          <div className="h-full overflow-y-auto">
            {isWorking ? (
              <div className="space-y-2 p-4">
                <h2 className="text-sm font-semibold text-foreground">
                  {t("stats.detail.workingTitle")}
                </h2>
                <p className="text-[11px] text-muted-foreground">{t("stats.workingScopeHint")}</p>
                <WorkingDirSummary
                  repoPath={repoPath}
                  headSha={headSha}
                  jumpTo="stats"
                  refetchMs={30_000}
                />
              </div>
            ) : selected ? (
              <CommitDetail
                commit={selected}
                failed={failedShas.has(selected.sha)}
                onOpenFile={(file) => router.navigate("stats", selected.sha, { file })}
                onViewNotes={() => router.navigate("notes", selected.sha)}
                onViewShow={() => setShowSha(selected.sha)}
              />
            ) : (
              <div className="flex h-full items-center justify-center p-6 text-xs text-muted-foreground">
                {t("stats.detail.emptyHint")}
              </div>
            )}
          </div>
        }
      />

      <footer className="shrink-0 border-t border-border px-4 py-1.5 text-[10px] text-muted-foreground">
        {t("stats.footer", { seconds: STATS_STALE_TIME_SECONDS })}
      </footer>

      {openFile && selectedSha && (
        <BlameDialog sha={selectedSha} file={openFile} range={openRange} onClose={closeOpenFile} />
      )}
      <ShowRawDialog sha={showSha} onClose={() => setShowSha(null)} />
    </div>
  );
}

// ============ 指标看板 ============

// 指标看板:与作者归因(People)同款卡片布局。AI 行 / 总行拆成两张独立卡,
// 不再挤成 "X / Y" 单行(大数字会换行),每卡一个数字。
function MetricsBar({
  agg,
  count,
  repoName,
  branch,
  loadedCount,
  limit,
  truncated,
  onlyMine,
  hasSearch,
  failedCount,
}: {
  agg: ReturnType<typeof aggregate>;
  count: number;
  repoName: string;
  branch: string | null;
  loadedCount: number;
  limit: number;
  truncated: boolean;
  onlyMine: boolean;
  hasSearch: boolean;
  failedCount: number;
}) {
  const { t } = useTranslation();
  return (
    <div className="shrink-0 border-b border-border p-3">
      <div className="mb-2 flex flex-wrap items-center gap-x-2 gap-y-1 text-[10px] text-muted-foreground">
        <span className="inline-flex items-center gap-1 font-medium text-foreground">
          <GitBranch className="h-3 w-3 text-primary" />
          {t("stats.scope.label")}
          <StatsScopeHelp
            repoName={repoName}
            branch={branch}
            loadedCount={loadedCount}
            includedCount={count}
            limit={limit}
            truncated={truncated}
            failedCount={failedCount}
          />
        </span>
        <span className="max-w-48 truncate" title={repoName}>
          {repoName}
        </span>
        <code className="rounded-sm bg-muted px-1 py-0.5 font-mono text-[9px] text-foreground">
          {branch ?? t("stats.scope.detachedHead")}
        </code>
        <span>· {t("stats.scope.historyWindow", { loaded: loadedCount, limit })}</span>
        <span>· {t("stats.scope.included", { count })}</span>
        <span className="rounded-full bg-muted px-1.5 py-0.5">
          {onlyMine ? t("stats.scope.onlyMine") : t("stats.scope.allAuthors")}
        </span>
        {hasSearch && (
          <span className="rounded-full bg-warning-muted px-1.5 py-0.5 text-warning-foreground dark:text-warning">
            {t("stats.scope.searchApplied")}
          </span>
        )}
        {truncated && (
          <span className="text-warning-foreground dark:text-warning">
            {t("stats.scope.reachedLimit", { limit })}
          </span>
        )}
        <span>· {t("stats.scope.excludesWorking")}</span>
      </div>
      <div className="grid grid-cols-2 gap-2 sm:grid-cols-3 lg:grid-cols-5">
        <MetricCard
          title={t("stats.metric.aiShare")}
          display={formatPercent(agg.aiPct)}
          tone="ai"
          help={<StatsMetricHelp metric="aiShare" />}
        />
        <MetricCard
          title={t("stats.metric.aiLines")}
          display={formatInt(agg.ai)}
          tone="ai"
          help={<StatsMetricHelp metric="aiLines" />}
        />
        <MetricCard
          title={t("stats.metric.totalLines")}
          display={formatInt(agg.total)}
          help={<StatsMetricHelp metric="totalLines" />}
        />
        <MetricCard
          title={t("stats.metric.commits")}
          display={formatInt(count)}
          help={<StatsMetricHelp metric="commits" />}
        />
        <MetricCard
          title={t("stats.metric.authors")}
          display={formatInt(agg.authors)}
          help={<StatsMetricHelp metric="authors" />}
        />
      </div>
    </div>
  );
}

type StatsMetricHelpId = "aiShare" | "aiLines" | "totalLines" | "commits" | "authors";

function StatsMetricHelp({ metric }: { metric: StatsMetricHelpId }) {
  const { t } = useTranslation();
  const title = t(`stats.metric.${metric}`);
  const definition = t(`stats.metricHelp.${metric}.definition`);
  const formula = t(`stats.metricHelp.${metric}.formula`);

  return (
    <HelpPopover label={t("stats.metricHelp.ariaLabel", { metric: title })} title={title}>
      <HelpSection label={t("formula.definition")}>{definition}</HelpSection>
      <HelpSection label={t("formula.formula")} mono>
        {formula}
      </HelpSection>
    </HelpPopover>
  );
}

function StatsScopeHelp({
  repoName,
  branch,
  loadedCount,
  includedCount,
  limit,
  truncated,
  failedCount,
}: {
  repoName: string;
  branch: string | null;
  loadedCount: number;
  includedCount: number;
  limit: number;
  truncated: boolean;
  failedCount: number;
}) {
  const { t } = useTranslation();
  return (
    <HelpPopover label={t("stats.scope.helpAriaLabel")} title={t("stats.scope.helpTitle")}>
      <ul className="list-disc space-y-1.5 pl-4 text-[11px] leading-relaxed text-muted-foreground">
        <li>
          {t("stats.scope.helpSource", {
            repo: repoName,
            branch: branch ?? t("stats.scope.detachedHead"),
          })}
        </li>
        <li>{t("stats.scope.helpLimit", { loaded: loadedCount, limit })}</li>
        <li>{t("stats.scope.helpFilters", { included: includedCount })}</li>
        <li>{t("stats.scope.helpWorking")}</li>
        <li>{t("stats.scope.helpFailures", { count: failedCount })}</li>
        {truncated && (
          <li className="text-warning-foreground dark:text-warning">
            {t("stats.scope.helpTruncated", { limit })}
          </li>
        )}
      </ul>
    </HelpPopover>
  );
}

function HelpPopover({
  label,
  title,
  children,
}: {
  label: string;
  title: string;
  children: React.ReactNode;
}) {
  return (
    <Popover>
      <PopoverTrigger asChild>
        <button
          type="button"
          aria-label={label}
          aria-haspopup="dialog"
          className="inline-flex h-4 w-4 items-center justify-center rounded-sm text-muted-foreground hover:text-foreground focus:outline-hidden focus:ring-2 focus:ring-ring"
        >
          <Info className="h-3.5 w-3.5" />
        </button>
      </PopoverTrigger>
      <PopoverContent className="w-80" align="start">
        <div className="space-y-2.5">
          <div className="text-sm font-semibold text-foreground">{title}</div>
          {children}
        </div>
      </PopoverContent>
    </Popover>
  );
}

function HelpSection({
  label,
  children,
  mono = false,
}: {
  label: string;
  children: React.ReactNode;
  mono?: boolean;
}) {
  return (
    <div>
      <div className="text-[10px] font-semibold uppercase tracking-wide text-muted-foreground">
        {label}
      </div>
      <div
        className={cn("mt-0.5 text-[11px] leading-relaxed text-foreground", mono && "font-mono")}
      >
        {children}
      </div>
    </div>
  );
}

// ============ commit 列表 ============

function CommitList({
  commits,
  failedShas,
  selectedSha,
  isWorking,
  onSelect,
  onSelectWorking,
}: {
  commits: CommitWithStats[];
  failedShas: Set<string>;
  selectedSha: string | undefined;
  isWorking: boolean;
  onSelect: (sha: string) => void;
  onSelectWorking: () => void;
}) {
  const { t } = useTranslation();
  return (
    <div>
      {/* 置顶:本地未提交改动(实时,尚未进入提交历史)—— 沿用 git 客户端"历史顶部放未提交"惯例。
          它不是 commit;下方分隔线后才是已提交历史。 */}
      <button
        type="button"
        onClick={onSelectWorking}
        className={cn(
          "flex w-full items-center gap-3 px-3 py-2 text-left text-xs",
          isWorking ? "bg-primary/10" : "hover:bg-muted/40",
        )}
      >
        <Sparkles className="h-3.5 w-3.5 shrink-0 text-primary" />
        <span className="min-w-0 flex-1 truncate">
          <span className="font-medium text-foreground">{t("stats.commitList.workingLabel")}</span>
          <span className="ml-1.5 text-muted-foreground">{t("stats.commitList.workingHint")}</span>
        </span>
        <span className="shrink-0 rounded-sm bg-primary/10 px-1.5 py-0.5 text-[10px] font-medium text-primary">
          {t("stats.commitList.liveBadge")}
        </span>
      </button>

      {/* 分隔:以下为已提交历史(区分上面的"未提交"实时态) */}
      <div className="flex items-center gap-2 border-y border-border bg-muted/30 px-3 py-1 text-[10px] font-medium uppercase tracking-wider text-muted-foreground">
        {t("stats.commitList.committedHistory")}
        <span className="font-normal normal-case">
          {t("stats.commitList.commitCount", { count: commits.length })}
        </span>
      </div>

      <CommitAttributionList
        commits={commits}
        failedShas={failedShas}
        selectedSha={isWorking ? undefined : selectedSha}
        onSelect={onSelect}
      />
    </div>
  );
}

// ============ 选中 commit 详情 ============

function CommitDetail({
  commit,
  failed,
  onOpenFile,
  onViewNotes,
  onViewShow,
}: {
  commit: CommitWithStats;
  failed: boolean;
  onOpenFile: (file: string) => void;
  onViewNotes: () => void;
  onViewShow: () => void;
}) {
  const { t } = useTranslation();
  const total = commitTotal(commit.stats);
  const rates = deriveRates(commit.stats, total);
  return (
    <div className="space-y-4 p-4">
      <div>
        <div className="text-sm font-semibold text-foreground">{commit.subject}</div>
        <div className="mt-1 flex flex-wrap items-center gap-2 text-[11px] text-muted-foreground">
          <code className="rounded-sm bg-muted px-1.5 py-0.5 font-mono">{commit.short}</code>
          {commit.is_merge && (
            <span className="inline-flex items-center gap-1 text-info">
              <GitMerge className="h-3 w-3" />
              merge
            </span>
          )}
          <span className="font-medium text-primary">AI {formatPercent(rates.ai_share)}</span>
        </div>
        <div className="mt-1 text-[11px] text-muted-foreground">
          <span className="font-medium text-foreground">{commit.author_name}</span> &lt;
          {commit.author_email}&gt; · {commit.authored_at.slice(0, 16).replace("T", " ")}
        </div>
      </div>

      {failed && (
        <div className="rounded-md border border-danger bg-danger-muted p-2 text-[11px] text-danger">
          {t("stats.detail.failedNotice")}
        </div>
      )}

      <Card padding="sm">
        <StatsBar stats={commit.stats} total={total} />
        <div className="mt-2 flex items-center justify-between text-[11px] text-muted-foreground">
          <span>
            {t("stats.detail.bucketBreakdown", {
              human: formatInt(commit.stats.human_additions),
              unknown: formatInt(commit.stats.unknown_additions),
              ai: formatInt(commit.stats.ai_additions),
            })}
          </span>
          <FormulaPopover metricId="ai_share" />
        </div>
      </Card>

      <NoteBanners noteKind={commit.note_kind} />

      <ToolModelTable breakdown={commit.stats.tool_model_breakdown} />

      <ChangedFilesPanel sha={commit.sha} onOpenFile={onOpenFile} />

      <RawDataLinks onViewNotes={onViewNotes} onViewShow={onViewShow} />
    </div>
  );
}

// ============ Note 警示条(基于客观字段:note_kind 来自后端 derive_note_kind 单一口径) ============

function NoteBanners({ noteKind }: { noteKind: NoteKind | null }) {
  const { t } = useTranslation();
  const router = useRouter();
  if (!noteKind) return null;
  if (noteKind === "merge") {
    return <Banner tone="info" text={t("stats.noteText.merge")} />;
  }
  if (noteKind === "empty_additions") {
    return <Banner tone="info" text={t("stats.noteText.emptyAdditions")} />;
  }
  return (
    <Banner
      tone="warn"
      text={t("stats.noteText.workingLogsMissing")}
      cta={{ label: t("stats.banner.gotoHooks"), onClick: () => router.navigate("hooks") }}
    />
  );
}

function Banner({
  tone,
  text,
  cta,
}: {
  tone: "warn" | "info";
  text: string;
  cta?: { label: string; onClick: () => void };
}) {
  return (
    <div
      className={cn(
        "flex items-start gap-2 rounded-md border p-3 text-xs",
        tone === "warn"
          ? "border-warning bg-warning-muted text-warning-foreground dark:text-warning"
          : "border-info bg-info-muted text-info",
      )}
    >
      <GitMerge className="mt-0.5 h-3.5 w-3.5 shrink-0" />
      <div className="flex-1">{text}</div>
      {cta && (
        <button
          type="button"
          onClick={cta.onClick}
          className="shrink-0 rounded-sm border border-warning bg-card px-2 py-0.5 text-[11px] font-medium text-warning-foreground hover:bg-warning-muted dark:text-warning"
        >
          {cta.label}
        </button>
      )}
    </div>
  );
}

// ============ Tool/Model breakdown 表 ============

function ToolModelTable({ breakdown }: { breakdown: Record<string, ToolModelStats> }) {
  const { t } = useTranslation();
  const entries = useMemo(() => Object.entries(breakdown), [breakdown]);
  if (entries.length === 0) return null;
  return (
    <div>
      <div className="mb-1 flex items-center gap-2 text-[10px] font-medium uppercase tracking-wider text-muted-foreground">
        {t("stats.toolModel.heading")}
        <FormulaPopover metricId="tool_model_breakdown" />
      </div>
      <table className="w-full text-xs">
        <thead className="border-b border-border text-left text-[10px] uppercase tracking-wide text-muted-foreground">
          <tr>
            <th className="py-1 pr-4 font-medium">tool::model</th>
            <th className="py-1 font-medium">{t("stats.toolModel.aiLinesColumn")}</th>
          </tr>
        </thead>
        <tbody>
          {entries.map(([k, v]) => (
            <tr key={k} className="border-b border-border last:border-0">
              <td className="py-1 pr-4 font-mono">{k}</td>
              <td className="py-1 font-mono">{formatInt(v.ai_additions)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

// ============ 原始数据深链 ============

function RawDataLinks({
  onViewNotes,
  onViewShow,
}: {
  onViewNotes: () => void;
  onViewShow: () => void;
}) {
  const { t } = useTranslation();
  return (
    <div className="flex items-center gap-3 border-t border-border pt-3 text-[11px] text-muted-foreground">
      <button
        type="button"
        onClick={onViewNotes}
        title={t("stats.rawLinks.notesTitle")}
        className="inline-flex items-center gap-1 rounded-md px-2 py-1 transition-colors hover:bg-muted hover:text-foreground"
      >
        {t("stats.rawLinks.notesLabel")}
      </button>
      <button
        type="button"
        onClick={onViewShow}
        title={t("stats.rawLinks.showTitle")}
        className="inline-flex items-center gap-1 rounded-md px-2 py-1 transition-colors hover:bg-muted hover:text-foreground"
      >
        <FileText className="h-3 w-3" />
        {t("showRaw.trigger")}
      </button>
    </div>
  );
}

// ============ 文件 → 逐行归因弹窗(复用 Blame 行级内核 + 停靠详情) ============

function BlameDialog({
  sha,
  file,
  range,
  onClose,
}: {
  sha: string;
  file: string;
  range: [number, number] | null;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const [full, setFull] = useState(false);
  const [theme, setTheme] = useState<"light" | "dark">(() => detectTheme());
  const [activeClick, setActiveClick] = useState<BlameLineClickEvent | null>(null);

  useEffect(() => {
    const ob = new MutationObserver(() => setTheme(detectTheme()));
    ob.observe(document.documentElement, { attributes: true, attributeFilter: ["class"] });
    return () => ob.disconnect();
  }, []);

  // ref = commit sha(getBlameAtRef 的 ref 接受任意 commit-ish);range 来自深链 ?L=,
  // 与原 Blame 页同口径:行范围传后端做范围查询(只标命中行)。
  const blameQ = useQuery<BlameResult>({
    queryKey: ["blame_at_commit", sha, file, range],
    queryFn: () => getBlameAtRef(sha, file, range ? [[range[0], range[1]]] : null),
    staleTime: 30_000,
  });
  const fileQ = useQuery<ReadFileResult>({
    queryKey: ["read_file_at_commit", sha, file],
    queryFn: () => readFileAtRef(sha, file),
    staleTime: 30_000,
  });

  const blamePayload = blameQ.data?.status === "ok" ? blameQ.data.payload : null;
  // 逐行渲染数据(AI 行索引 + 每行作者/模型)走共享纯派生,与原 Blame 页同一实现。
  const { aiLines, lineAuthors } = useMemo(() => deriveBlameLines(blamePayload), [blamePayload]);
  const aiCount = aiLines.size;
  const record = activeClick ? (blamePayload?.prompts[activeClick.promptId] ?? null) : null;

  const fileText = fileQ.data?.status === "ok" ? fileQ.data.text : null;
  // 硬故障:文件读取 / blame 任一 degraded → 逐一专用空态(响亮失败,不塌缩成一句泛化文案)。
  const degradedReason =
    fileQ.data?.status === "degraded"
      ? fileQ.data.reason
      : blameQ.data?.status === "degraded"
        ? blameQ.data.reason
        : null;

  return (
    <Dialog
      open
      onOpenChange={(v) => !v && onClose()}
      size={full ? "full" : "xl"}
      title={
        <span className="flex items-center gap-2">
          <Sparkles className="h-4 w-4 shrink-0 text-primary" />
          <span className="truncate font-mono text-sm">{file}</span>
          <code className="shrink-0 rounded-sm bg-muted px-1.5 py-0.5 font-mono text-[11px] text-muted-foreground">
            {sha.slice(0, 7)}
          </code>
          {aiCount > 0 && (
            <span className="shrink-0 text-xs font-normal text-primary">
              {t("stats.blameDialog.aiLineCount", { count: aiCount })}
            </span>
          )}
          <button
            type="button"
            onClick={() => setFull((v) => !v)}
            title={full ? t("stats.blameDialog.restore") : t("stats.blameDialog.fullscreen")}
            className="ml-1 rounded-sm p-1 text-muted-foreground hover:bg-muted hover:text-foreground"
          >
            {full ? <Minimize2 className="h-4 w-4" /> : <Maximize2 className="h-4 w-4" />}
          </button>
        </span>
      }
    >
      {/* 固定宽高容器:钉死宽+高,避免加载态窄、加载完变宽的弹性抖动;并给 CM6 确定高度规避长文件被裁 */}
      <div
        className={cn("flex min-h-0", full ? "h-[82vh] w-full" : "h-[72vh] w-[58rem] max-w-full")}
      >
        {fileQ.isLoading || blameQ.isLoading ? (
          <div className="flex flex-1 items-center justify-center text-sm text-muted-foreground">
            <Loader2 className="mr-2 h-4 w-4 animate-spin" />
            {t("stats.blameDialog.loading")}
          </div>
        ) : degradedReason ? (
          <FileDegradedCard reason={degradedReason} />
        ) : fileText !== null ? (
          <>
            <div className="min-h-0 min-w-0 flex-1 overflow-hidden rounded-md border border-border">
              <BlameCodeView
                code={fileText}
                filePath={file}
                aiLines={aiLines}
                lineAuthors={lineAuthors}
                theme={theme}
                onLineClick={setActiveClick}
              />
            </div>
            {activeClick && record && (
              <aside className="relative ml-3 w-72 shrink-0 overflow-y-auto rounded-md border border-border bg-card p-3 pr-8">
                <button
                  type="button"
                  onClick={() => setActiveClick(null)}
                  aria-label={t("blame.lineDetail.close")}
                  className="absolute right-2 top-2 rounded-sm p-1 text-muted-foreground hover:bg-muted hover:text-foreground"
                >
                  <X className="h-3.5 w-3.5" />
                </button>
                <BlamePromptDetails
                  record={record}
                  lineNumber={activeClick.lineNumber}
                  metadata={blamePayload?.metadata ?? { is_logged_in: false, current_user: null }}
                />
              </aside>
            )}
          </>
        ) : null}
      </div>
    </Dialog>
  );
}

// ============ git-ai show <sha> 原文 Dialog ============

const NO_AUTHORSHIP_MARKER = "No authorship data found for this revision";

function splitShowRaw(raw: string): { json: string; attestations: string | null } {
  const idx = raw.indexOf("\n---\n");
  if (idx < 0) return { json: raw, attestations: null };
  return { json: raw.slice(0, idx), attestations: raw.slice(idx + 5) };
}

function ShowRawDialog({ sha, onClose }: { sha: string | null; onClose: () => void }) {
  const { t } = useTranslation();
  const open = sha !== null;
  const showQ = useQuery<ShowRawResult>({
    queryKey: ["show_raw", sha],
    queryFn: () => getShowRaw(sha as string),
    enabled: open,
    staleTime: 30_000,
  });

  useEffect(() => {
    if (!open) return;
    const data = showQ.data;
    if (data?.status === "degraded") {
      toast.error(
        data.reason.kind === "repo_missing"
          ? t("showRaw.degradedRepoMissing")
          : t("showRaw.degradedGitAiMissing"),
      );
      onClose();
    }
  }, [open, showQ.data, onClose, t]);

  const copyM = useMutation({
    mutationFn: async (text: string) => {
      await navigator.clipboard.writeText(text);
    },
    onSuccess: () => toast.success(t("showRaw.copiedToast")),
    onError: (e) => toast.error(t("stats.copyFailedToast"), { description: (e as Error).message }),
  });

  const payload = showQ.data?.status === "ok" ? showQ.data.payload : null;
  const raw = payload?.raw ?? "";
  const isEmpty = raw.trim() === NO_AUTHORSHIP_MARKER;
  const sections = useMemo(() => (raw && !isEmpty ? splitShowRaw(raw) : null), [raw, isEmpty]);

  return (
    <Dialog
      open={open}
      onOpenChange={(v) => !v && onClose()}
      title={sha ? t("showRaw.dialogTitleTemplate", { sha: sha.slice(0, 7) }) : ""}
      description={t("showRaw.dialogDescription")}
      size="xl"
      footer={
        <>
          <button
            type="button"
            onClick={() => copyM.mutate(raw)}
            disabled={!payload || isEmpty || copyM.isPending}
            className="inline-flex items-center gap-1 rounded-md border border-border px-3 py-1.5 text-sm hover:bg-muted disabled:opacity-50"
          >
            {copyM.isPending ? (
              <Loader2 className="h-3.5 w-3.5 animate-spin" />
            ) : (
              <Copy className="h-3.5 w-3.5" />
            )}
            {t("showRaw.copyButton")}
            {t("stats.blameDialog.copyFullSuffix")}
          </button>
          <button
            type="button"
            onClick={onClose}
            className="rounded-md bg-primary px-3 py-1.5 text-sm font-medium text-primary-foreground hover:bg-primary/90"
          >
            {t("stats.rawLinks.close")}
          </button>
        </>
      }
    >
      {showQ.isLoading && (
        <div className="flex items-center gap-2 text-xs text-muted-foreground">
          <Loader2 className="h-3.5 w-3.5 animate-spin" />
          {t("stats.blameDialog.showRawLoading")}
        </div>
      )}
      {showQ.isError && (
        <p className="text-xs text-danger">
          {t("showRaw.loadFailed")}:{(showQ.error as Error).message}
        </p>
      )}
      {payload && isEmpty && <p className="text-xs text-muted-foreground">{t("showRaw.empty")}</p>}
      {payload && !isEmpty && sections && (
        <div className="space-y-3">
          <RawSection
            label={t("stats.blameDialog.jsonSectionLabel")}
            body={sections.json}
            onCopy={(s) => copyM.mutate(s)}
            copyPending={copyM.isPending}
          />
          {sections.attestations !== null && (
            <RawSection
              label={t("stats.blameDialog.attestationsSectionLabel")}
              body={sections.attestations}
              onCopy={(s) => copyM.mutate(s)}
              copyPending={copyM.isPending}
            />
          )}
        </div>
      )}
    </Dialog>
  );
}

function RawSection({
  label,
  body,
  onCopy,
  copyPending,
}: {
  label: string;
  body: string;
  onCopy: (s: string) => void;
  copyPending: boolean;
}) {
  const { t } = useTranslation();
  return (
    <section className="rounded-md border border-border">
      <header className="flex items-center justify-between border-b border-border bg-muted px-3 py-1.5 text-xs font-medium text-foreground">
        <span>{label}</span>
        <button
          type="button"
          onClick={() => onCopy(body)}
          disabled={copyPending}
          className="inline-flex items-center gap-1 rounded-sm p-0.5 text-muted-foreground hover:bg-background hover:text-foreground disabled:opacity-50"
          title={t("stats.rawLinks.copySectionTitle", { label })}
        >
          <Copy className="h-3 w-3" />
        </button>
      </header>
      <pre className="max-h-[60vh] overflow-auto whitespace-pre rounded-b-md bg-card p-3 font-mono text-xs leading-relaxed text-foreground">
        {body}
      </pre>
    </section>
  );
}
