import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  ArrowRight,
  Check,
  Clock,
  Crosshair,
  FolderGit2,
  FolderOpen,
  GitBranch,
  Layers,
  Loader2,
  Plus,
  RefreshCw,
  Search,
  Trash2,
  X,
} from "lucide-react";
import { useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";

import { Badge } from "../components/Badge";
import { Tooltip } from "../components/ui/TooltipBubble";
import {
  currentRepo as currentRepoApi,
  discoverRepos,
  getAggregateRepos,
  listRecentRepos,
  listScanRoots,
  openInExplorer,
  selectRepo,
  setAggregateRepos,
  setScanRoots,
} from "../lib/api";
import { cn } from "../lib/cn";
import { pickDirectory } from "../lib/pickDirectory";
import type { AggregateRepoEntry, RepoEntry } from "../lib/types";
import { useRepoChanged } from "../lib/useRepoChanged";
import { useRouter } from "../router";

export default function RepoPage() {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const router = useRouter();
  const handleRepoChanged = useRepoChanged();
  const [filter, setFilter] = useState("");
  const [newRoot, setNewRoot] = useState("");
  const [openingPath, setOpeningPath] = useState<string | null>(null);

  // 当前选中仓库,用于列表里高亮"当前"行。staleTime 与 TopBar 一致,共享缓存。
  const currentRepoQ = useQuery({
    queryKey: ["current_repo"],
    queryFn: currentRepoApi,
    staleTime: 5_000,
  });
  const currentPath = currentRepoQ.data?.path ?? null;
  const rootsQ = useQuery({ queryKey: ["scan_roots"], queryFn: listScanRoots, staleTime: 60_000 });
  const recentQ = useQuery({
    queryKey: ["recent_repos"],
    queryFn: listRecentRepos,
    staleTime: 30_000,
  });
  const reposQ = useQuery({
    queryKey: ["repos", rootsQ.data],
    queryFn: () => discoverRepos(rootsQ.data ?? [], 4),
    staleTime: 30_000,
    enabled: !!rootsQ.data,
  });

  const setRootsM = useMutation({
    mutationFn: setScanRoots,
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["scan_roots"] });
      qc.invalidateQueries({ queryKey: ["repos"] });
    },
    onError: (e) =>
      toast.error(t("repo.toast.saveRootsFailed"), { description: (e as Error).message }),
  });
  // 「查看」某个仓:selectRepo(顺带设为当前仓)→ 切仓全局副作用 → 直接进单仓视图(Stats)。
  // "看一个仓"是动词:用户无需理解/维护"当前仓"这个内部状态,点了就进去看(群贤决策)。
  const pickM = useMutation({
    mutationFn: selectRepo,
    onSuccess: () => {
      // 切仓 = 全局副作用,统一走 hook(invalidate + URL reset 一次到位)。
      handleRepoChanged();
      router.navigate("stats");
    },
    onError: (e) =>
      toast.error(t("repo.toast.switchFailed"), { description: (e as Error).message }),
  });

  // 跨仓聚合集合(M3):哪些仓纳入 Dashboard 跨仓视图。与"当前仓"(selectRepo)正交。
  const aggregateQ = useQuery({
    queryKey: ["aggregate_repos"],
    queryFn: getAggregateRepos,
    staleTime: 30_000,
  });
  const aggregateSet = useMemo(
    () => new Set((aggregateQ.data ?? []).map((e) => e.path.toLowerCase())),
    [aggregateQ.data],
  );
  const aggregateM = useMutation({
    mutationFn: setAggregateRepos,
    onSuccess: (_result, paths) => {
      const savedByPath = new Map(
        (aggregateQ.data ?? []).map((repo) => [repo.path.toLowerCase(), repo]),
      );
      const discoveredByPath = new Map(
        (reposQ.data ?? []).map((repo) => [repo.path.toLowerCase(), repo]),
      );
      const next: AggregateRepoEntry[] = paths.map((path) => {
        const saved = savedByPath.get(path.toLowerCase());
        if (saved) return saved;
        const entry = discoveredByPath.get(path.toLowerCase()) ?? null;
        return { path, valid: entry !== null, entry };
      });
      qc.setQueryData(["aggregate_repos"], next);
      // 聚合集合变了 → 旧集合维度的跨仓缓存立即移除，避免短暂展示旧数据。
      qc.removeQueries({ queryKey: ["history_agg"] });
      qc.removeQueries({ queryKey: ["working_agg"] });
    },
    onError: (e) =>
      toast.error(t("repo.aggregate.saveFailed"), { description: (e as Error).message }),
  });
  const clearAggregateM = useMutation({
    mutationFn: async () => {
      const repos = aggregateSet.size;
      await setAggregateRepos([]);
      return { repos };
    },
    onSuccess: ({ repos }) => {
      qc.setQueryData<AggregateRepoEntry[]>(["aggregate_repos"], []);
      qc.removeQueries({ queryKey: ["history_agg"] });
      qc.removeQueries({ queryKey: ["working_agg"] });
      toast.success(t("repo.aggregate.clearSuccess", { repos }));
    },
    onError: (e) => {
      void qc.invalidateQueries({ queryKey: ["aggregate_repos"] });
      toast.error(t("repo.aggregate.clearFailed"), { description: (e as Error).message });
    },
  });
  function toggleAggregate(path: string) {
    const cur = new Set((aggregateQ.data ?? []).map((e) => e.path));
    // 命中比较用小写;增删用原始(后端规整)路径。
    const existing = [...cur].find((p) => p.toLowerCase() === path.toLowerCase());
    if (existing) cur.delete(existing);
    else cur.add(path);
    aggregateM.mutate([...cur]);
  }
  function aggregateAll() {
    // 并集:保留已勾选(可能在搜索框外)+ 加入当前可见仓,绝不因过滤而覆盖丢失已选(红队 M3#5)。
    const next = new Set([
      ...(aggregateQ.data ?? []).map((e) => e.path),
      ...filtered.map((r) => r.path),
    ]);
    aggregateM.mutate([...next]);
  }
  function aggregateClear() {
    clearAggregateM.mutate();
  }

  const filtered = useMemo(() => {
    const all = reposQ.data ?? [];
    if (!filter.trim()) return all;
    const q = filter.toLowerCase();
    return all.filter((r) => r.name.toLowerCase().includes(q) || r.path.toLowerCase().includes(q));
  }, [reposQ.data, filter]);

  function addRoot() {
    const r = newRoot.trim();
    if (!r) return;
    const next = Array.from(new Set([...(rootsQ.data ?? []), r]));
    setRootsM.mutate(next);
    setNewRoot("");
  }
  function removeRoot(r: string) {
    setRootsM.mutate((rootsQ.data ?? []).filter((x) => x !== r));
  }
  async function openRepoInExplorer(path: string) {
    setOpeningPath(path);
    try {
      await openInExplorer(path);
    } catch (e) {
      toast.error(t("repo.toast.openFolderFailed"), { description: (e as Error).message });
    } finally {
      setOpeningPath((current) => (current === path ? null : current));
    }
  }

  const currentName = currentRepoQ.data?.name ?? null;

  return (
    <div className="space-y-4 p-6">
      <div>
        <h1 className="text-xl font-semibold">{t("repo.title")}</h1>
        <p className="mt-0.5 text-xs text-muted-foreground">{t("repo.subtitle")}</p>
      </div>

      {/* 顶部汇总:两个正交概念一眼分清 —— 聚合集(喂 Dashboard 跨仓)vs 当前仓(Stats/People/Blame 显示的那个)。 */}
      <section className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        <div className="rounded-lg border border-border bg-card p-4">
          <div className="flex items-center gap-2 text-xs font-medium text-muted-foreground">
            <Layers className="h-3.5 w-3.5" /> {t("repo.summary.aggregateTitle")}
          </div>
          <div className="mt-1 text-2xl font-semibold tabular-nums">
            {t("repo.summary.aggregateCount", { n: aggregateSet.size })}
          </div>
          <p className="mt-0.5 text-[11px] text-muted-foreground">
            {t("repo.summary.aggregateHint")}
          </p>
        </div>
        <div className="rounded-lg border border-border bg-card p-4">
          <div className="flex items-center gap-2 text-xs font-medium text-muted-foreground">
            <Crosshair className="h-3.5 w-3.5" /> {t("repo.summary.currentTitle")}
          </div>
          <div className="mt-1 truncate text-2xl font-semibold">
            {currentName ?? (
              <span className="text-base font-normal text-muted-foreground">
                {t("repo.summary.currentNone")}
              </span>
            )}
          </div>
          <p className="mt-0.5 text-[11px] text-muted-foreground">
            {t("repo.summary.currentHint")}
          </p>
        </div>
      </section>

      {/* 主区:全部仓库。每行两个显式动作 —— 「加入/已加入」(纳入跨仓聚合)、「查看」(进该仓单仓视图)。 */}
      <section className="rounded-lg border border-border bg-card p-4">
        <div className="mb-3 flex items-center justify-between gap-2">
          <h2 className="flex items-center gap-2 text-sm font-medium">
            {t("repo.all.title")}
            {reposQ.isFetching ? (
              <Loader2 className="h-3.5 w-3.5 animate-spin text-muted-foreground" />
            ) : (
              <Badge tone="neutral">{filtered.length}</Badge>
            )}
          </h2>
          <div className="flex items-center gap-2">
            <button
              onClick={() => qc.invalidateQueries({ queryKey: ["repos"] })}
              disabled={reposQ.isFetching}
              className="inline-flex items-center gap-1 text-xs text-muted-foreground hover:text-foreground disabled:opacity-50"
            >
              <RefreshCw className={cn("h-3 w-3", reposQ.isFetching && "animate-spin")} />{" "}
              {t("repo.scanRoots.rescan")}
            </button>
            <div className="relative w-56">
              <Search className="absolute left-2 top-1.5 h-3.5 w-3.5 text-muted-foreground" />
              <input
                value={filter}
                onChange={(e) => setFilter(e.target.value)}
                placeholder={t("repo.all.filterPlaceholder")}
                className="w-full rounded-sm border border-border bg-card py-1 pl-7 pr-2 text-xs dark:border-border dark:bg-card"
              />
            </div>
          </div>
        </div>

        {/* 聚合集批量操作:并集语义,「全选」只加可见仓、不覆盖搜索框外的已选(红队 M3#5)。 */}
        <div className="mb-2 flex items-center gap-2 text-xs text-muted-foreground">
          <span>{t("repo.aggregate.selectedCount", { n: aggregateSet.size })}</span>
          <button
            type="button"
            onClick={aggregateAll}
            disabled={filtered.length === 0 || aggregateM.isPending || clearAggregateM.isPending}
            title={t("repo.aggregate.selectAllHint")}
            className="inline-flex items-center rounded-md border border-border bg-secondary px-2.5 py-1 font-medium text-secondary-foreground hover:bg-secondary/80 disabled:cursor-not-allowed disabled:opacity-50"
          >
            {t("repo.aggregate.selectAll")}
          </button>
          <Tooltip content={t("repo.aggregate.clearHint")}>
            <button
              type="button"
              onClick={aggregateClear}
              disabled={
                aggregateSet.size === 0 || aggregateM.isPending || clearAggregateM.isPending
              }
              className="inline-flex items-center gap-1 rounded-md border border-danger bg-transparent px-2.5 py-1 font-medium text-danger hover:bg-danger-muted disabled:cursor-not-allowed disabled:opacity-50"
            >
              <Trash2 className="h-3 w-3" />
              {t("repo.aggregate.clear")}
            </button>
          </Tooltip>
        </div>
        {filtered.length === 0 && !reposQ.isFetching && (
          <div className="rounded-sm border border-dashed border-border px-4 py-8 text-center text-xs text-muted-foreground dark:border-border">
            {(rootsQ.data ?? []).length === 0 ? t("repo.all.emptyNoRoots") : t("repo.all.empty")}
          </div>
        )}
        <ul className="divide-y divide-border">
          {filtered.map((r) => (
            <RepoRow
              key={r.path}
              repo={r}
              isCurrent={!!currentPath && currentPath.toLowerCase() === r.path.toLowerCase()}
              inAggregate={aggregateSet.has(r.path.toLowerCase())}
              aggregateUpdating={aggregateM.isPending || clearAggregateM.isPending}
              onToggleAggregate={() => toggleAggregate(r.path)}
              viewing={pickM.isPending && pickM.variables === r.path}
              onView={() => pickM.mutate(r.path)}
              opening={openingPath === r.path}
              onOpen={() => openRepoInExplorer(r.path)}
            />
          ))}
        </ul>
      </section>

      {/* 次要区(折叠):扫描目录管理 + 最近打开。默认收起,让仓库列表占据主视觉。 */}
      <details className="group rounded-lg border border-border bg-card">
        <summary className="flex cursor-pointer list-none items-center justify-between px-4 py-3 text-sm font-medium">
          <span className="flex items-center gap-2">
            <FolderOpen className="h-4 w-4 text-muted-foreground" />
            {t("repo.manage.title")}
          </span>
          <span className="text-[11px] font-normal text-muted-foreground">
            {t("repo.manage.rootCount", { n: (rootsQ.data ?? []).length })}
          </span>
        </summary>
        <div className="space-y-4 border-t border-border p-4">
          {/* 扫描根目录 */}
          <div>
            <h3 className="mb-2 text-xs font-medium text-muted-foreground">
              {t("repo.scanRoots.title")}
            </h3>
            {(rootsQ.data ?? []).length === 0 && (
              <p className="mb-2 text-xs text-muted-foreground">
                {t("repo.scanRoots.emptyHint")} <span className="font-mono">D:\script</span>。
              </p>
            )}
            <ul className="mb-3 space-y-1">
              {(rootsQ.data ?? []).map((r) => (
                <li
                  key={r}
                  className="flex items-center justify-between rounded-sm border border-border px-2 py-1 text-xs dark:border-border"
                >
                  <span className="truncate font-mono">{r}</span>
                  <button
                    onClick={() => removeRoot(r)}
                    className="ml-2 inline-flex items-center gap-1 rounded-sm p-1 text-muted-foreground hover:bg-muted hover:text-rose-500 dark:hover:bg-muted"
                    title={t("repo.scanRoots.remove")}
                  >
                    <X className="h-3.5 w-3.5" />
                  </button>
                </li>
              ))}
            </ul>
            <div className="space-y-2">
              <div className="flex gap-2">
                <button
                  onClick={async () => {
                    try {
                      const picked = await pickDirectory(t("repo.scanRoots.pickDialogTitle"));
                      if (picked) setNewRoot(picked);
                    } catch (e) {
                      toast.error(t("repo.toast.openPickerFailed"), {
                        description: (e as Error).message,
                      });
                    }
                  }}
                  className="inline-flex items-center gap-1 rounded-md border border-border px-2.5 py-1 text-xs hover:bg-muted dark:border-border dark:hover:bg-muted"
                >
                  <FolderOpen className="h-3 w-3" /> {t("repo.scanRoots.pickDir")}
                </button>
                <div className="flex-1 truncate rounded-sm border border-dashed border-border bg-card px-2 py-1 font-mono text-xs text-muted-foreground dark:border-border dark:bg-card dark:text-neutral-300">
                  {newRoot.trim() ? (
                    newRoot
                  ) : (
                    <span className="text-muted-foreground">{t("repo.scanRoots.noDirPicked")}</span>
                  )}
                </div>
                <button
                  onClick={addRoot}
                  disabled={!newRoot.trim() || setRootsM.isPending}
                  className="inline-flex items-center gap-1 rounded-md bg-primary px-2.5 py-1 text-xs font-medium text-primary-foreground hover:bg-primary/90 disabled:opacity-50"
                >
                  <Plus className="h-3 w-3" /> {t("repo.scanRoots.add")}
                </button>
              </div>
              <details className="text-[11px] text-muted-foreground">
                <summary className="cursor-pointer">{t("repo.scanRoots.pasteAdvanced")}</summary>
                <input
                  value={newRoot}
                  onChange={(e) => setNewRoot(e.target.value)}
                  onKeyDown={(e) => e.key === "Enter" && addRoot()}
                  placeholder={t("repo.scanRoots.pastePlaceholder")}
                  className="mt-1 w-full rounded-sm border border-border bg-card px-2 py-1 font-mono text-xs dark:border-border dark:bg-card"
                />
              </details>
            </div>
          </div>

          {/* 最近打开(快速重设下钻焦点) */}
          {(recentQ.data?.length ?? 0) > 0 && (
            <div>
              <h3 className="mb-2 flex items-center gap-2 text-xs font-medium text-muted-foreground">
                <Clock className="h-3.5 w-3.5" /> {t("repo.recent.title")}
              </h3>
              <ul className="space-y-1">
                {(recentQ.data ?? []).slice(0, 5).map((p) => {
                  const isCurrent = !!currentPath && currentPath.toLowerCase() === p.toLowerCase();
                  return (
                    <li key={p}>
                      <button
                        onClick={() => pickM.mutate(p)}
                        disabled={isCurrent || (pickM.isPending && pickM.variables === p)}
                        className={cn(
                          "flex w-full items-center justify-between rounded-sm border px-2 py-1.5 text-left text-xs",
                          isCurrent
                            ? "border-primary bg-primary/10 dark:border-primary dark:bg-primary/10"
                            : "border-border hover:bg-muted dark:border-border dark:hover:bg-muted",
                        )}
                      >
                        <span className="truncate">{p}</span>
                        {isCurrent ? (
                          <Badge tone="info">{t("repo.current")}</Badge>
                        ) : (
                          <span className="text-muted-foreground">{t("repo.recent.open")}</span>
                        )}
                      </button>
                    </li>
                  );
                })}
              </ul>
            </div>
          )}
        </div>
      </details>
    </div>
  );
}

function RepoRow({
  repo,
  isCurrent,
  inAggregate,
  aggregateUpdating,
  onToggleAggregate,
  viewing,
  opening,
  onView,
  onOpen,
}: {
  repo: RepoEntry;
  isCurrent: boolean;
  inAggregate: boolean;
  aggregateUpdating: boolean;
  onToggleAggregate: () => void;
  viewing: boolean;
  opening: boolean;
  onView: () => void;
  onOpen: () => void;
}) {
  const { t } = useTranslation();
  return (
    <li
      className={cn(
        "group -mx-2 flex items-center gap-3 rounded-sm px-2 py-2.5",
        isCurrent
          ? "bg-primary/5 ring-1 ring-inset ring-ring dark:bg-primary/10 dark:ring-ring"
          : "hover:bg-muted/40",
        viewing && "pointer-events-none opacity-70",
      )}
    >
      <FolderGit2 className={cn("h-4 w-4", isCurrent ? "text-primary" : "text-muted-foreground")} />
      {/* 仓库信息块:纯展示,不再承担"设为当前"动作 —— 动作集中在右侧两个显式按钮。 */}
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2 text-sm">
          <span className="font-medium">{repo.name}</span>
          {isCurrent && <Badge tone="info">{t("repo.current")}</Badge>}
          {repo.head_branch && (
            <span className="inline-flex items-center gap-1 text-xs text-muted-foreground">
              <GitBranch className="h-3 w-3" /> {repo.head_branch}
            </span>
          )}
          {repo.head_sha && (
            <span className="font-mono text-[11px] text-muted-foreground">
              {repo.head_sha.slice(0, 7)}
            </span>
          )}
          {repo.dirty === true && <Badge tone="warn">{t("repo.row.dirty")}</Badge>}
          {repo.has_git_ai_dir && repo.working_logs_count > 0 && (
            <Badge tone="info">
              {t("repo.row.checkpointCount", { n: repo.working_logs_count })}
            </Badge>
          )}
          {repo.has_git_ai_dir && repo.working_logs_count === 0 && (
            <Badge tone="neutral">{t("repo.row.gitAiEmpty")}</Badge>
          )}
        </div>
        <div className="mt-0.5 truncate text-[11px] text-muted-foreground">{repo.path}</div>
      </div>
      <Tooltip content={t("repo.row.openInExplorer")}>
        <button
          type="button"
          onClick={onOpen}
          disabled={opening}
          className="inline-flex rounded-sm p-1 text-muted-foreground hover:bg-muted hover:text-foreground disabled:cursor-wait disabled:opacity-60 dark:hover:bg-muted"
        >
          {opening ? (
            <Loader2 className="h-3.5 w-3.5 animate-spin" />
          ) : (
            <FolderOpen className="h-3.5 w-3.5" />
          )}
        </button>
      </Tooltip>
      {/* 加入聚合:显式按钮(替代裸 checkbox)。已加入态可点击移出,语义更直白。 */}
      <Tooltip
        content={inAggregate ? t("repo.aggregate.removeTooltip") : t("repo.aggregate.addTooltip")}
      >
        <button
          type="button"
          onClick={onToggleAggregate}
          disabled={aggregateUpdating}
          aria-pressed={inAggregate}
          aria-label={t("repo.aggregate.includeLabel")}
          className={cn(
            "inline-flex items-center gap-1 rounded-md px-2.5 py-1 text-xs font-medium disabled:cursor-not-allowed disabled:opacity-50",
            inAggregate
              ? "bg-primary text-primary-foreground hover:bg-primary/90"
              : "border border-border text-foreground hover:bg-muted dark:hover:bg-muted",
          )}
        >
          {inAggregate ? <Check className="h-3 w-3" /> : <Plus className="h-3 w-3" />}
          {inAggregate ? t("repo.aggregate.added") : t("repo.aggregate.add")}
        </button>
      </Tooltip>
      {/* 查看:进入该仓的单仓视图(Stats),并顺带把它设为当前仓 —— "看一个仓"是动词,
          不再让用户去理解/维护"下钻焦点"这个内部状态(详见群贤决策)。 */}
      <Tooltip content={t("repo.row.viewTooltip")}>
        <button
          type="button"
          onClick={onView}
          disabled={viewing}
          className="inline-flex items-center gap-1 rounded-md bg-primary/10 px-2.5 py-1 text-xs font-medium text-primary hover:bg-primary/15 disabled:opacity-60 dark:bg-primary/10 dark:text-primary dark:hover:bg-primary/20"
        >
          {viewing ? <Loader2 className="h-3 w-3 animate-spin" /> : null}
          {t("repo.row.view")}
          {!viewing && <ArrowRight className="h-3 w-3" />}
        </button>
      </Tooltip>
    </li>
  );
}
