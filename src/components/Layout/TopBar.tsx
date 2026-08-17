import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Check,
  ChevronDown,
  Copy,
  FolderGit2,
  GitBranch,
  Loader2,
  Settings,
  Trash2,
} from "lucide-react";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import {
  checkoutBranch,
  clearRecentRepos,
  currentRepo as currentRepoApi,
  listBranches,
  listRecentRepos,
  selectRepo as selectRepoApi,
} from "../../lib/api";
import type { ListBranchesResult } from "../../lib/types";
import { cn } from "../../lib/cn";
import { invalidateRepoScopedQueries } from "../../lib/queryKeys";
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "../ui/Command";
import { Popover, PopoverContent, PopoverTrigger } from "../ui/PopoverPanel";
import { Tooltip } from "../ui/TooltipBubble";
import { UpdateBadge } from "../UpdateBadge";
import type { RouteId } from "../../router";

/**
 * 选择器(仓库/分支)在条目数超过此阈值后才显示搜索输入框。
 *
 * 低于阈值时直接列表点选效率更高,避免"才 3 个仓库还要搜索"的视觉噪声;高于阈值时
 * cmdk 自带 fuzzy filter 提供按名/路径搜索能力(用户原话:"默认 10 条 其他的搜索即可")。
 */
const SELECTOR_SEARCH_THRESHOLD = 10;

function formatBranchHoverText(name: string, sha: string | null): string {
  return sha ? `${name}\n${sha}` : name;
}

function branchSearchValue(name: string, sha: string): string {
  return `${name} ${sha} ${sha.slice(0, 7)}`;
}

interface Props {
  onNavigate: (r: RouteId) => void;
  onRepoChanged?: () => void;
}

/**
 * 顶部全局栏:
 * - 左:always-visible 仓库切换器(下拉:最近仓库 + 管理仓库)+ 当前分支 / HEAD sha
 * - 右:设置齿轮(深链 settings)
 *
 * 环境配置(诊断 / 安装 / Hooks)作为侧栏「配置」组的常驻菜单项呈现,不在顶栏占指示灯。
 */
export function TopBar({ onNavigate, onRepoChanged }: Props) {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const repoQ = useQuery({ queryKey: ["current_repo"], queryFn: currentRepoApi, staleTime: 5_000 });
  const recentQ = useQuery({
    queryKey: ["recent_repos"],
    queryFn: listRecentRepos,
    staleTime: 30_000,
  });
  const [repoPickerOpen, setRepoPickerOpen] = useState(false);
  const [clearRecentConfirmOpen, setClearRecentConfirmOpen] = useState(false);

  const repo = repoQ.data ?? null;
  const recents = recentQ.data ?? [];

  const clearRecentM = useMutation({
    mutationFn: clearRecentRepos,
    onSuccess: (cleared) => {
      qc.setQueryData<string[]>(["recent_repos"], []);
      setClearRecentConfirmOpen(false);
      toast.success(t("topBar.clearRecentSuccess", { count: cleared }));
    },
    onError: (e) => {
      void qc.invalidateQueries({ queryKey: ["recent_repos"] });
      toast.error(t("topBar.clearRecentFailed"), { description: (e as Error).message });
    },
  });

  async function copyShaToClipboard() {
    if (!repo?.head_sha) return;
    await navigator.clipboard.writeText(repo.head_sha);
    toast.success(t("topBar.shaCopied"));
  }

  async function pickRecent(path: string) {
    try {
      await selectRepoApi(path);
      toast.success(t("topBar.repoSwitched"), { description: path });
      onRepoChanged?.();
    } catch (e) {
      toast.error(t("topBar.repoSwitchFailed"), { description: (e as Error).message });
    }
  }

  return (
    <header className="flex h-12 items-center gap-3 border-b border-border bg-background px-4">
      {/* 仓库切换器:always-visible。cmdk 驱动的可搜索下拉。 */}
      <Popover
        open={repoPickerOpen}
        onOpenChange={(open) => {
          setRepoPickerOpen(open);
          if (!open) setClearRecentConfirmOpen(false);
        }}
      >
        <PopoverTrigger asChild>
          <button
            className={cn(
              "inline-flex items-center gap-1.5 rounded-md border border-border px-2 py-1 text-sm",
              "text-foreground hover:bg-muted",
            )}
          >
            <FolderGit2 className="h-4 w-4 text-primary" />
            <span className="max-w-[180px] truncate">{repo?.name ?? t("topBar.noRepo")}</span>
            <ChevronDown className="h-3.5 w-3.5 text-muted-foreground" />
          </button>
        </PopoverTrigger>
        <PopoverContent align="start" className="w-[320px] max-w-none p-0">
          <Command>
            {recents.length > SELECTOR_SEARCH_THRESHOLD && (
              <CommandInput placeholder={t("topBar.searchRepoPlaceholder")} />
            )}
            <CommandList>
              <div className="flex items-center justify-between px-3 py-1.5 text-[10px] font-medium text-slate-500">
                <span>{t("topBar.recentRepos")}</span>
                <button
                  type="button"
                  onClick={() => setClearRecentConfirmOpen(true)}
                  disabled={recents.length === 0 || clearRecentM.isPending}
                  className="inline-flex items-center gap-1 rounded-sm px-1.5 py-0.5 text-danger hover:bg-danger-muted disabled:cursor-not-allowed disabled:opacity-40"
                >
                  <Trash2 className="h-3 w-3" />
                  {t("topBar.clearRecent")}
                </button>
              </div>
              <CommandEmpty>{t("topBar.noRecentRepos")}</CommandEmpty>
              <CommandGroup>
                {recents.map((p) => (
                  <CommandItem
                    key={p}
                    value={p}
                    onSelect={() => {
                      setRepoPickerOpen(false);
                      pickRecent(p);
                    }}
                  >
                    <span className="truncate">{p}</span>
                  </CommandItem>
                ))}
              </CommandGroup>
            </CommandList>
          </Command>
          {clearRecentConfirmOpen && (
            <div className="border-t border-border bg-danger-muted px-3 py-2 text-xs">
              <p className="text-foreground">{t("topBar.clearRecentConfirm")}</p>
              <div className="mt-2 flex justify-end gap-2">
                <button
                  type="button"
                  onClick={() => setClearRecentConfirmOpen(false)}
                  disabled={clearRecentM.isPending}
                  className="rounded-md px-2 py-1 text-muted-foreground hover:bg-muted disabled:opacity-50"
                >
                  {t("topBar.cancel")}
                </button>
                <button
                  type="button"
                  onClick={() => clearRecentM.mutate()}
                  disabled={clearRecentM.isPending}
                  className="inline-flex items-center gap-1 rounded-md border border-danger px-2 py-1 font-medium text-danger hover:bg-danger-muted disabled:opacity-50"
                >
                  {clearRecentM.isPending && <Loader2 className="h-3 w-3 animate-spin" />}
                  {t("topBar.confirmClearRecent")}
                </button>
              </div>
            </div>
          )}
          {/* 「管理仓库…」放在 Command 容器之外:cmdk 1.x 的 CommandItem 即便 forceMount,
              当 value 与当前 filter 词不匹配时,鼠标点击的 onSelect 会被静默吞掉。改成
              Popover 内的独立 button,绕开 cmdk 的 filter / selectable 机制,直接 onClick。 */}
          <div className="border-t border-border">
            <button
              type="button"
              onClick={() => {
                setRepoPickerOpen(false);
                onNavigate("repo");
              }}
              className="flex w-full items-center gap-2 px-3 py-2 text-left text-sm hover:bg-muted"
            >
              <FolderGit2 className="h-4 w-4 text-muted-foreground" />
              <span>{t("topBar.manageRepos")}</span>
            </button>
          </div>
        </PopoverContent>
      </Popover>

      {/* HEAD chip:分支切换 + 可选 sha + 复制 */}
      {repo && (
        <div
          role="group"
          aria-label="HEAD"
          className="inline-flex items-center gap-1 rounded-md border border-border text-xs"
        >
          <BranchSwitcher repo={repo} qc={qc} onSwitched={onRepoChanged} />
          {repo.head_sha && (
            <span className="px-0.5 font-mono text-muted-foreground" aria-hidden="true">
              ·
            </span>
          )}
          {repo.head_sha && (
            <Tooltip content={repo.head_sha} side="bottom">
              <span className="cursor-text select-text font-mono">{repo.head_sha.slice(0, 7)}</span>
            </Tooltip>
          )}
          <button
            type="button"
            onClick={copyShaToClipboard}
            disabled={!repo.head_sha}
            aria-label={t("topBar.copyShaAria")}
            title={t("topBar.copyShaTitle")}
            className="ml-0.5 inline-flex items-center rounded-sm p-1 text-muted-foreground hover:bg-muted hover:text-foreground disabled:opacity-40"
          >
            <Copy className="h-3 w-3" />
          </button>
        </div>
      )}

      <div className="flex-1" />

      {/* 更新徽章:仅在有可用更新时显示,点击深链 settings 查看 / 安装 */}
      <UpdateBadge onClick={() => onNavigate("settings")} />

      {/* 设置齿轮:深链 settings。ghost icon 按钮(参考 cc-switch),中性 token hover,
          不抢色(原硬编码蓝 hover 已移除);方形定宽 + focus-visible ring 兼顾键盘可达。 */}
      <Tooltip content={t("topBar.settingsTooltip")} side="bottom">
        <button
          type="button"
          onClick={() => onNavigate("settings")}
          aria-label={t("topBar.settingsTooltip")}
          className="inline-flex h-8 w-8 items-center justify-center rounded-md text-muted-foreground transition-colors duration-150 hover:bg-muted hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
        >
          <Settings className="h-4 w-4" />
        </button>
      </Tooltip>
    </header>
  );
}

/**
 * 分支切换器:DropdownMenuTrigger 是 button(承担点击),展开列出本地分支供切换。
 *
 * # 全局副作用
 * 切换成功后 invalidate 所有"基于 HEAD"的 query —— Blame / Stats / Notes / History / Dashboard 同步刷新。
 * 这是显式表达"切分支影响全局",而不是让各页自己猜要不要重拉。
 *
 * # Degraded 处理
 * - dirty_worktree:列前 5 个脏文件 + CTA "去 Checkpoints" 引导用户暂存
 * - not_found:分支不存在(理论上 list 拿到的分支应一直存在,出现说明并发改动)
 * - conflict:stderr 透传(checkout 自己拒绝,常见是 .gitignore 没盖住的未跟踪文件冲突)
 */
function BranchSwitcher({
  repo,
  qc,
  onSwitched,
}: {
  repo: { head_branch: string | null; head_sha: string | null };
  qc: ReturnType<typeof useQueryClient>;
  onSwitched?: () => void;
}) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const branchesQ = useQuery<ListBranchesResult>({
    queryKey: ["list_branches"],
    queryFn: listBranches,
    staleTime: 10_000,
  });
  const checkoutM = useMutation({
    mutationFn: (name: string) => checkoutBranch(name),
    onSuccess: (result, name) => {
      if (result.status === "degraded") {
        const r = result.reason;
        if (r.kind === "dirty_worktree") {
          const preview = r.files.slice(0, 5).join("\n");
          const more =
            r.files.length > 5 ? `\n${t("topBar.branch.moreFiles", { n: r.files.length })}` : "";
          toast.error(t("topBar.branch.dirtyTitle", { n: r.files.length, name }), {
            description: `${preview}${more}\n${t("topBar.branch.dirtyDescription")}`,
            duration: 8000,
          });
          return;
        }
        if (r.kind === "not_found") {
          toast.error(t("topBar.branch.notFound", { name: r.name }));
          return;
        }
        if (r.kind === "conflict") {
          toast.error(t("topBar.branch.switchFailed"), { description: r.stderr, duration: 8000 });
          return;
        }
        if (r.kind === "repo_missing") {
          toast.error(t("topBar.noRepoSelected"));
          return;
        }
      } else {
        toast.success(t("topBar.branch.switched", { branch: result.payload.branch }));
        // 切分支 = 全局副作用:所有基于 HEAD 的页面要刷新
        invalidateRepoScopedQueries(qc);
        onSwitched?.();
      }
    },
    onError: (e, name) =>
      toast.error(t("topBar.branch.switchToFailed", { name }), {
        description: (e as Error).message,
      }),
  });

  const branches = branchesQ.data?.status === "ok" ? branchesQ.data.branches : [];
  const currentLabel = repo.head_branch ?? "detached";
  const currentHoverText = formatBranchHoverText(currentLabel, repo.head_sha);
  const busy = checkoutM.isPending;

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <button
          type="button"
          disabled={busy}
          className="inline-flex items-center gap-1 rounded-md px-2 py-1 hover:bg-muted disabled:opacity-60"
          title={currentHoverText}
          aria-label={t("topBar.branch.triggerTitle")}
        >
          {busy ? (
            <Loader2 className="h-3.5 w-3.5 shrink-0 animate-spin text-muted-foreground" />
          ) : (
            <GitBranch className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
          )}
          <span className="max-w-[160px] truncate font-mono">{currentLabel}</span>
          <ChevronDown className="h-3 w-3 text-muted-foreground" />
        </button>
      </PopoverTrigger>
      <PopoverContent align="start" className="w-[420px] max-w-none p-0">
        <Command>
          <CommandInput placeholder={t("topBar.branch.searchPlaceholder")} />
          <CommandList>
            <CommandEmpty>{t("topBar.branch.noMatch")}</CommandEmpty>
            <CommandGroup heading={t("topBar.branch.heading")}>
              {branchesQ.isLoading && (
                <div className="px-2 py-1 text-xs text-muted-foreground">
                  {t("topBar.branch.loading")}
                </div>
              )}
              {branchesQ.isError && (
                <div className="px-2 py-1 text-xs text-danger">
                  {(branchesQ.error as Error).message}
                </div>
              )}
              {branches.length === 0 && !branchesQ.isLoading && !branchesQ.isError && (
                <div className="px-2 py-1 text-xs text-muted-foreground">
                  {t("topBar.branch.noLocalBranches")}
                </div>
              )}
              {branches.map((b) => (
                <CommandItem
                  key={b.name}
                  value={branchSearchValue(b.name, b.sha)}
                  disabled={busy}
                  title={formatBranchHoverText(b.name, b.sha)}
                  onSelect={() => {
                    if (!b.is_current) {
                      setOpen(false);
                      checkoutM.mutate(b.name);
                    }
                  }}
                  className="flex items-center justify-between gap-3"
                >
                  <span className="flex min-w-0 items-center gap-1.5">
                    {b.is_current ? (
                      <Check className="h-3 w-3 shrink-0 text-success" />
                    ) : (
                      <span className="h-3 w-3 shrink-0" aria-hidden="true" />
                    )}
                    {/* 不在内部 span 上加 title:行级 CommandItem 的 title(全名+SHA)才是完整
                        hover 文本,内部 title={b.name} 会把它覆盖成"只剩分支名"。 */}
                    <span className="truncate font-mono">{b.name}</span>
                  </span>
                  <span className="shrink-0 font-mono text-[10px] text-muted-foreground">
                    {b.sha.slice(0, 7)}
                  </span>
                </CommandItem>
              ))}
            </CommandGroup>
          </CommandList>
        </Command>
      </PopoverContent>
    </Popover>
  );
}
