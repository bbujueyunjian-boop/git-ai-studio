import { useQuery } from "@tanstack/react-query";
import { useEffect, useRef } from "react";

import i18n from "../i18n";
import { diagnoseGitAiDaemon } from "../lib/api";
import {
  DAEMON_DISMISS_HOURS,
  DAEMON_PROBE_INTERVAL_MS,
  DAEMON_RESET_EVENT,
  daemonDismissedUntilKey,
  daemonIssueKey,
  decideDaemonNotification,
} from "../lib/daemonNotifier";
import { readMsFromStorage, writeMsToStorage } from "../lib/daemonNotifier";
import { notify } from "../lib/osNotify";
import type { AppSettings, DaemonHealth } from "../lib/types";

interface Props {
  settings: AppSettings | undefined;
}

export function DaemonWatcher({ settings }: Props) {
  // 单一总开关:与 LowAiShareWatcher 一致 — 关闭即停止轮询与告警。
  const enabled = settings?.notifications?.daemon_unhealthy_alert ?? false;
  const seenIssueKeysRef = useRef(new Set<string>());
  // 连续观察同一 issueKey 才告警 — 重启电脑后 daemon 启动竞态期会瞬时出现 blocked_lock_unknown_pid,
  // 几秒后 daemon 写入新 pid.json 又恢复 running,这里要求"至少连续 2 次同一 issue"才推送 OS 通知。
  const consecutiveIssueRef = useRef<{ issueKey: string; count: number } | null>(null);

  // 监听"重置静默"事件:Settings 页点按钮后清掉本会话已弹过 / 连续观察计数。
  useEffect(() => {
    const onReset = () => {
      seenIssueKeysRef.current = new Set<string>();
      consecutiveIssueRef.current = null;
    };
    window.addEventListener(DAEMON_RESET_EVENT, onReset);
    return () => window.removeEventListener(DAEMON_RESET_EVENT, onReset);
  }, []);

  const q = useQuery({
    queryKey: ["diagnose_git_ai_daemon"],
    queryFn: diagnoseGitAiDaemon,
    enabled,
    refetchInterval: enabled ? DAEMON_PROBE_INTERVAL_MS : false,
    // 即便窗口最小化 / 隐藏到托盘也继续轮询 — Studio 的典型用法是"开机自启 +
    // 关闭=最小化到托盘"做后台监控,daemon 是 host 级关键告警(挂了所有 hook
    // 命令被阻塞),不能因为窗口隐藏就停推送 OS 通知。30s 一次 tasklist 子进程开销
    // 可忽略。
    refetchIntervalInBackground: true,
  });

  useEffect(() => {
    if (!enabled) return;
    const health = q.data;
    const issueKey = health ? daemonIssueKey(health) : null;

    // 步骤 1:维护连续观察计数。issueKey 一致 → count++;切换或恢复 healthy → 清零。
    if (issueKey) {
      const prev = consecutiveIssueRef.current;
      consecutiveIssueRef.current =
        prev && prev.issueKey === issueKey
          ? { issueKey, count: prev.count + 1 }
          : { issueKey, count: 1 };
    } else {
      consecutiveIssueRef.current = null;
    }

    const decision = decideDaemonNotification({
      enabled,
      health,
      seenThisSession: issueKey ? seenIssueKeysRef.current.has(issueKey) : false,
      dismissedUntilMs: issueKey ? readMsFromStorage(daemonDismissedUntilKey(issueKey)) : null,
      nowMs: Date.now(),
    });
    if (
      !decision.trigger ||
      !decision.issueKey ||
      !health ||
      health.kind !== "blocked_lock_unknown_pid"
    ) {
      return;
    }
    // 步骤 2:必须连续 2 次以上同一 issue 才告警(防抖)。
    if ((consecutiveIssueRef.current?.count ?? 0) < 2) return;

    seenIssueKeysRef.current.add(decision.issueKey);
    writeMsToStorage(
      daemonDismissedUntilKey(decision.issueKey),
      Date.now() + DAEMON_DISMISS_HOURS * 60 * 60 * 1000,
    );
    const { title, body } = buildDaemonAlertPayload(health);
    void notify(title, body);
  }, [enabled, q.data, q.dataUpdatedAt]);

  return null;
}

/** 为未知持锁者生成告警，仅展示已观测的文件和 PID 元信息。 */
function buildDaemonAlertPayload(
  health: Extract<DaemonHealth, { kind: "blocked_lock_unknown_pid" }>,
): { title: string; body: string } {
  // 1. 提供排查位置，不把同名进程当作持锁者。
  const title = i18n.t("daemon.blockedLock.title");
  const body = `lock: ${health.lock_path}\npid metadata: ${health.pid_meta_path}\nlast pid: ${health.last_pid ?? "unknown"}`;
  return { title, body };
}
