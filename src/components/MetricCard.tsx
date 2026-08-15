import type { ReactNode } from "react";

import { Card } from "./ui/CardPanel";

/**
 * 指标卡:标题 + 单个主数字。作者归因(People)与提交归因(Stats)共用,语义统一。
 *
 * 主数字 28px(font-bold),与 Stats/People 指标卡同档(Dashboard 用更大的 36px light 档,密度更低)。
 * 左色条 tone:人工=human 绿、AI/占比=ai 蓝、计数=neutral 无条。
 */
export function MetricCard({
  title,
  display,
  tone = "neutral",
  help,
}: {
  title: string;
  display: string;
  tone?: "ai" | "human" | "neutral";
  help?: ReactNode;
}) {
  return (
    <Card
      padding="sm"
      interactive
      tone={tone}
      className="flex min-h-[100px] flex-col justify-between"
    >
      <div className="flex items-center gap-1 text-[11px] font-medium text-muted-foreground">
        <span>{title}</span>
        {help}
      </div>
      <div className="mt-1 font-mono text-[28px] font-bold leading-tight tabular-nums text-foreground">
        {display}
      </div>
    </Card>
  );
}
