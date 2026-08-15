// 单 commit 改动文件面板(数据容器 + 纯展示)。Stats 详情区与 Blame 左栏共用。
//
// # 数据口径
// - 改动文件与三桶来自 `list_changed_files_in_commit`；后端先把 note 范围与 diff 新增行求交
// - 文件 AI 占比只看本 commit 新增行；删除行单列，不进入分母
//
// # 抽象边界
// onOpenFile 由调用方注入(Stats 开弹窗 / Blame 在主区渲染整文件逐行 blame);
// selectedFile 仅用于高亮当前文件(Blame 传当前文件,Stats 不传)——纯展示态,非行为分支。

import { useQuery } from "@tanstack/react-query";
import { Loader2 } from "lucide-react";
import { useTranslation } from "react-i18next";

import { listChangedFilesInCommit } from "../lib/api";
import { cn } from "../lib/cn";
import { deriveFileAttributionDisplay } from "../lib/fileAttribution";
import { formatInt, formatPercent } from "../lib/formulas";
import type { ChangedFile, ChangedFilesResult, FileLineStats } from "../lib/types";
import { Tooltip } from "./ui/TooltipBubble";

export function ChangedFilesPanel({
  sha,
  onOpenFile,
  selectedFile,
}: {
  sha: string;
  onOpenFile: (file: string) => void;
  /** 高亮当前打开的文件(Blame 左栏用);Stats 弹窗模式不传。 */
  selectedFile?: string;
}) {
  const { t } = useTranslation();
  const changedQ = useQuery<ChangedFilesResult>({
    queryKey: ["changed_files", sha],
    queryFn: () => listChangedFilesInCommit(sha),
    staleTime: 60_000,
  });
  const data = changedQ.data;
  return (
    <div>
      <div className="mb-1 flex items-center gap-2 text-[10px] font-medium uppercase tracking-wider text-muted-foreground">
        {t("changedFiles.title")}
        {data?.status === "ok" && (
          <span className="rounded-sm bg-muted px-1.5 py-0.5 text-[10px] text-muted-foreground">
            {data.files.length}
          </span>
        )}
      </div>
      <div className="mb-2 text-[10px] leading-relaxed text-muted-foreground">
        {t("changedFiles.aiShareFormula")}
      </div>
      {changedQ.isLoading ? (
        <div className="flex items-center gap-2 text-xs text-muted-foreground">
          <Loader2 className="h-3.5 w-3.5 animate-spin" />
          {t("changedFiles.loading")}
        </div>
      ) : changedQ.isError ? (
        <div className="text-xs text-danger">
          {t("changedFiles.failedPrefix")}:{(changedQ.error as Error).message}
        </div>
      ) : !data ? null : data.status === "degraded" ? (
        <div className="text-xs text-muted-foreground">
          {data.reason.kind === "invalid_sha"
            ? t("changedFiles.invalidSha")
            : t("stats.degraded.repoMissing.title")}
        </div>
      ) : data.files.length === 0 ? (
        <div className="text-xs text-muted-foreground">{t("changedFiles.empty")}</div>
      ) : (
        <ChangedFilesList
          files={data.files}
          isMerge={data.is_merge}
          onOpenFile={onOpenFile}
          selectedFile={selectedFile}
        />
      )}
    </div>
  );
}

function ChangedFilesList({
  files,
  isMerge,
  onOpenFile,
  selectedFile,
}: {
  files: ChangedFile[];
  isMerge: boolean;
  onOpenFile: (file: string) => void;
  selectedFile?: string;
}) {
  const { t } = useTranslation();
  const statusLabelMap = t("changedFiles.status", { returnObjects: true }) as Record<
    string,
    string
  >;
  return (
    <ul className="space-y-0.5 text-xs">
      {files.map((f) => {
        const attribution = deriveFileAttributionDisplay(f, isMerge);
        const statusLabel = statusLabelMap[f.status] ?? f.status;
        const disabled = f.status === "D";
        const active = selectedFile === f.path;
        const detailedStats =
          attribution.kind === "measured" || attribution.kind === "unattributed"
            ? attribution.stats
            : null;
        const row = (
          <button
            type="button"
            onClick={() => !disabled && onOpenFile(f.path)}
            disabled={disabled}
            title={
              detailedStats
                ? undefined
                : disabled
                  ? t("changedFiles.deletedFileTitle")
                  : t("changedFiles.viewBlameTitle")
            }
            className={cn(
              "group flex w-full items-start gap-2 rounded-sm px-1.5 py-1 text-left transition-colors disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-transparent",
              active ? "bg-primary/10" : "hover:bg-muted",
            )}
          >
            <StatusBadge status={f.status} label={statusLabel} />
            <div className="min-w-0 flex-1">
              <div className="flex items-center gap-2">
                <code
                  className={cn(
                    "min-w-0 flex-1 truncate font-mono text-[11px] group-hover:text-foreground",
                    active ? "text-primary" : "text-foreground/90",
                  )}
                >
                  {f.path}
                </code>
                <FileAttributionMetric display={attribution} />
              </div>
              {detailedStats && <FileAttributionBar stats={detailedStats} />}
              {detailedStats && (
                <span className="sr-only">
                  {t("changedFiles.bucketDetailsTemplate", {
                    human: formatInt(detailedStats.human_additions),
                    unknown: formatInt(detailedStats.unknown_additions),
                    ai: formatInt(detailedStats.ai_additions),
                  })}
                  .{" "}
                  {t("changedFiles.diffTemplate", {
                    added: formatInt(detailedStats.additions),
                    deleted: formatInt(detailedStats.deletions),
                  })}
                </span>
              )}
            </div>
          </button>
        );
        return (
          <li key={`${f.status}:${f.path}`}>
            {detailedStats ? (
              <Tooltip side="left" content={<FileAttributionDetails stats={detailedStats} />}>
                {row}
              </Tooltip>
            ) : (
              row
            )}
          </li>
        );
      })}
    </ul>
  );
}

function FileAttributionMetric({
  display,
}: {
  display: ReturnType<typeof deriveFileAttributionDisplay>;
}) {
  const { t } = useTranslation();
  if (display.kind === "merge") {
    return (
      <MetricText title={t("changedFiles.mergeNotApplicable")}>
        {t("changedFiles.mergeNotApplicableShort")}
      </MetricText>
    );
  }
  if (display.kind === "binary") {
    return (
      <MetricText title={t("changedFiles.binaryNotApplicable")}>
        {t("changedFiles.binaryNotApplicableShort")}
      </MetricText>
    );
  }
  if (display.kind === "deletions_only") {
    const label = t("changedFiles.deletionsOnlyTemplate", {
      n: formatInt(display.deletions),
    });
    return <MetricText title={label}>{label}</MetricText>;
  }
  if (display.kind === "no_additions") {
    return <MetricText>{t("changedFiles.noAdditions")}</MetricText>;
  }

  const stats = display.stats;
  const label =
    display.kind === "unattributed"
      ? t("changedFiles.unattributedTemplate", { n: formatInt(stats.unknown_additions) })
      : t("changedFiles.aiShareTemplate", {
          percent: formatPercent(display.aiRatio),
          ai: formatInt(stats.ai_additions),
          total: formatInt(stats.additions),
        });
  return (
    <span
      title={label}
      className={cn(
        "max-w-[140px] shrink-0 truncate rounded-sm px-1.5 py-0.5 text-[10px] font-medium ring-1 ring-inset",
        display.kind === "unattributed"
          ? "bg-muted text-muted-foreground ring-border"
          : "bg-ai/10 text-ai ring-ai/30",
      )}
    >
      {label}
    </span>
  );
}

function FileAttributionDetails({ stats }: { stats: FileLineStats }) {
  const { t } = useTranslation();
  return (
    <div className="space-y-0.5 whitespace-normal break-words">
      <div>
        {t("changedFiles.bucketDetailsTemplate", {
          human: formatInt(stats.human_additions),
          unknown: formatInt(stats.unknown_additions),
          ai: formatInt(stats.ai_additions),
        })}
      </div>
      <div className="opacity-80">
        {t("changedFiles.diffTemplate", {
          added: formatInt(stats.additions),
          deleted: formatInt(stats.deletions),
        })}
      </div>
    </div>
  );
}

function MetricText({ children, title }: { children: React.ReactNode; title?: string }) {
  return (
    <span
      className="max-w-[96px] shrink-0 truncate text-[10px] text-muted-foreground"
      title={title}
    >
      {children}
    </span>
  );
}

function FileAttributionBar({ stats }: { stats: FileLineStats }) {
  const segments = [
    { key: "human", value: stats.human_additions, className: "bg-human" },
    { key: "unknown", value: stats.unknown_additions, className: "bg-unknown" },
    { key: "ai", value: stats.ai_additions, className: "bg-ai" },
  ];
  return (
    <div
      aria-hidden="true"
      className="mt-1 flex h-1 w-full overflow-hidden rounded-full bg-secondary"
    >
      {segments.map((segment) =>
        segment.value > 0 ? (
          <span
            key={segment.key}
            className={cn("h-full", segment.className)}
            style={{ width: `${(segment.value / stats.additions) * 100}%` }}
          />
        ) : null,
      )}
    </div>
  );
}

// git diff status 字符 → 语义 tone(与 AI / 人工数据色解耦,表达文件级 git 操作)。
const STATUS_TONE: Record<string, string> = {
  A: "bg-success-muted text-success",
  M: "bg-warning-muted text-warning-foreground dark:text-warning",
  D: "bg-danger-muted text-danger",
  C: "bg-info-muted text-info",
};
const STATUS_TONE_NEUTRAL = "bg-muted text-muted-foreground";

function StatusBadge({ status, label }: { status: string; label: string }) {
  const cls = STATUS_TONE[status] ?? STATUS_TONE_NEUTRAL;
  return (
    <span
      className={`inline-flex w-[42px] shrink-0 items-center justify-center rounded-sm px-1 py-0.5 text-[10px] font-medium ${cls}`}
      title={`${status} · ${label}`}
    >
      {label}
    </span>
  );
}
