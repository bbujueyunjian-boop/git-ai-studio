import { describe, expect, it } from "vitest";

import { deriveFileAttributionDisplay } from "../lib/fileAttribution";
import type { ChangedFile, FileLineStats } from "../lib/types";

function file(stats: FileLineStats | null): ChangedFile {
  return { path: "src/x.ts", status: "M", line_stats: stats };
}

describe("逐文件 AI 占比展示状态", () => {
  it("按 AI 新增行 / 总新增行计算占比", () => {
    const display = deriveFileAttributionDisplay(
      file({
        additions: 50,
        deletions: 8,
        ai_additions: 30,
        human_additions: 5,
        unknown_additions: 15,
      }),
      false,
    );
    expect(display.kind).toBe("measured");
    if (display.kind === "measured") {
      expect(display.aiRatio).toBe(0.6);
      expect(display.stats.deletions).toBe(8);
    }
  });

  it("全部 unknown 显示未归因，而不是 AI 0%", () => {
    const display = deriveFileAttributionDisplay(
      file({
        additions: 50,
        deletions: 0,
        ai_additions: 0,
        human_additions: 0,
        unknown_additions: 50,
      }),
      false,
    );
    expect(display.kind).toBe("unattributed");
  });

  it("human-only 是可测量的 AI 0%，不误判为未归因", () => {
    const display = deriveFileAttributionDisplay(
      file({
        additions: 5,
        deletions: 0,
        ai_additions: 0,
        human_additions: 5,
        unknown_additions: 0,
      }),
      false,
    );
    expect(display.kind).toBe("measured");
    if (display.kind === "measured") expect(display.aiRatio).toBe(0);
  });

  it("纯删除不计算百分比", () => {
    const display = deriveFileAttributionDisplay(
      file({
        additions: 0,
        deletions: 51,
        ai_additions: 0,
        human_additions: 0,
        unknown_additions: 0,
      }),
      false,
    );
    expect(display).toEqual({ kind: "deletions_only", deletions: 51 });
  });

  it("纯重命名显示无新增行", () => {
    const display = deriveFileAttributionDisplay(
      file({
        additions: 0,
        deletions: 0,
        ai_additions: 0,
        human_additions: 0,
        unknown_additions: 0,
      }),
      false,
    );
    expect(display).toEqual({ kind: "no_additions" });
  });

  it("merge 与 binary 都明确不可计算", () => {
    expect(deriveFileAttributionDisplay(file(null), true)).toEqual({ kind: "merge" });
    expect(deriveFileAttributionDisplay(file(null), false)).toEqual({ kind: "binary" });
  });
});
