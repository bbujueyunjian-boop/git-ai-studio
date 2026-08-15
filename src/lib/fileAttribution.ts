import type { ChangedFile, FileLineStats } from "./types";

export type FileAttributionDisplay =
  | { kind: "merge" }
  | { kind: "binary" }
  | { kind: "deletions_only"; deletions: number }
  | { kind: "no_additions" }
  | { kind: "unattributed"; stats: FileLineStats }
  | { kind: "measured"; stats: FileLineStats; aiRatio: number };

/** 把后端事实映射为互斥 UI 状态；不在前端补算或猜测缺失归因。 */
export function deriveFileAttributionDisplay(
  file: ChangedFile,
  isMerge: boolean,
): FileAttributionDisplay {
  if (isMerge) return { kind: "merge" };
  if (file.line_stats === null) return { kind: "binary" };

  const stats = file.line_stats;
  if (stats.additions === 0) {
    return stats.deletions > 0
      ? { kind: "deletions_only", deletions: stats.deletions }
      : { kind: "no_additions" };
  }
  if (
    stats.ai_additions === 0 &&
    stats.human_additions === 0 &&
    stats.unknown_additions === stats.additions
  ) {
    return { kind: "unattributed", stats };
  }
  return {
    kind: "measured",
    stats,
    aiRatio: stats.ai_additions / stats.additions,
  };
}
