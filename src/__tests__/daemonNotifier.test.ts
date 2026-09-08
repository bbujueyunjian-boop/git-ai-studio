import { describe, expect, it } from "vitest";

import {
  daemonDismissedUntilKey,
  daemonIssueKey,
  decideDaemonNotification,
} from "../lib/daemonNotifier";
import type { DaemonHealth } from "../lib/types";

const blocked: DaemonHealth = {
  kind: "blocked_lock_unknown_pid",
  lock_path: "C:/Users/me/.git-ai/internal/daemon/daemon.lock",
  pid_meta_path: "C:/Users/me/.git-ai/internal/daemon/daemon.pid.json",
  last_pid: null,
};

describe("daemonNotifier", () => {
  it("健康态不触发", () => {
    expect(
      decideDaemonNotification({
        enabled: true,
        health: { kind: "idle" },
        seenThisSession: false,
        dismissedUntilMs: null,
        nowMs: 1000,
      }),
    ).toEqual({ trigger: false, issueKey: null, reason: "healthy" });
  });

  it("异常首触发返回 issue key", () => {
    const d = decideDaemonNotification({
      enabled: true,
      health: blocked,
      seenThisSession: false,
      dismissedUntilMs: null,
      nowMs: 1000,
    });
    expect(d.trigger).toBe(true);
    expect(d.issueKey).toBe(daemonIssueKey(blocked));
  });

  it("会话内已提醒和 24h 冷却均不重复触发", () => {
    expect(
      decideDaemonNotification({
        enabled: true,
        health: blocked,
        seenThisSession: true,
        dismissedUntilMs: null,
        nowMs: 1000,
      }).reason,
    ).toBe("already_seen_this_session");
    expect(
      decideDaemonNotification({
        enabled: true,
        health: blocked,
        seenThisSession: false,
        dismissedUntilMs: 2000,
        nowMs: 1000,
      }).reason,
    ).toBe("user_dismissed");
  });

  it("localStorage key 按 issue 隔离", () => {
    const key = daemonDismissedUntilKey(daemonIssueKey(blocked)!);
    expect(key).toContain("git-ai-studio.notifications.daemon.dismissedUntil.");
    expect(key).not.toContain("C:/Users");
  });
});
