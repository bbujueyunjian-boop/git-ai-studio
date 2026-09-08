import type { DaemonHealth } from "./types";

export const DAEMON_PROBE_INTERVAL_MS = 30 * 1000;
export const DAEMON_DISMISS_HOURS = 24;
const DAEMON_DISMISSED_KEY_PREFIX = "git-ai-studio.notifications.daemon.dismissedUntil.";

export interface DaemonNotifyInput {
  enabled: boolean;
  health: DaemonHealth | undefined;
  seenThisSession: boolean;
  dismissedUntilMs: number | null;
  nowMs: number;
}

export interface DaemonNotifyDecision {
  trigger: boolean;
  issueKey: string | null;
  reason:
    | null
    | "disabled"
    | "no_data"
    | "healthy"
    | "already_seen_this_session"
    | "user_dismissed";
}

/** 仅为持锁者未知的异常生成告警键，空闲与正常运行不告警。 */
export function daemonIssueKey(health: DaemonHealth): string | null {
  // 1. 只对需要排查的运行状态分配稳定键。
  if (health.kind === "blocked_lock_unknown_pid") {
    return `${health.kind}:${health.lock_path}`;
  }
  return null;
}

export function decideDaemonNotification(input: DaemonNotifyInput): DaemonNotifyDecision {
  if (!input.enabled) return { trigger: false, issueKey: null, reason: "disabled" };
  if (!input.health) return { trigger: false, issueKey: null, reason: "no_data" };
  const issueKey = daemonIssueKey(input.health);
  if (!issueKey) return { trigger: false, issueKey: null, reason: "healthy" };
  if (input.seenThisSession) {
    return { trigger: false, issueKey, reason: "already_seen_this_session" };
  }
  if (input.dismissedUntilMs !== null && input.nowMs < input.dismissedUntilMs) {
    return { trigger: false, issueKey, reason: "user_dismissed" };
  }
  return { trigger: true, issueKey, reason: null };
}

export function daemonDismissedUntilKey(issueKey: string): string {
  return `${DAEMON_DISMISSED_KEY_PREFIX}${encodeURIComponent(issueKey)}`;
}

/**
 * 重置事件。`DaemonWatcher` 监听后清掉 `seenIssueKeys` 与连续观察计数,
 * 重新允许告警推送。Settings 页"重置静默"按钮 dispatch 这条事件。
 */
export const DAEMON_RESET_EVENT = "git-ai-studio:daemon-reset";

/**
 * 清除所有 daemon dismiss 记录。daemon 的 dismiss key 是 per-issueKey 形式
 * (`...dismissedUntil.blocked_lock_unknown_pid:<path>`),
 * 用户不会记得当时具体是哪个 issue,所以**全清** —— 扫描 localStorage 删所有
 * 带前缀的 key。
 */
export function clearDaemonSilence(): void {
  try {
    const ls = window.localStorage;
    const toRemove: string[] = [];
    for (let i = 0; i < ls.length; i++) {
      const k = ls.key(i);
      if (k && k.startsWith(DAEMON_DISMISSED_KEY_PREFIX)) toRemove.push(k);
    }
    for (const k of toRemove) ls.removeItem(k);
  } catch {
    /* localStorage 不可用时无需操作,内存兜底由下方 Map 管理;
       Watcher 收到 reset 事件会清内存计数 */
  }
  memoryFallback.clear();
}

/**
 * localStorage 不可用时(Safari Private 模式等)的内存兜底。Daemon dismiss
 * 与 cross-session 冷却 key 都通过本对走 ms 序列化。
 */
const memoryFallback = new Map<string, number>();
let storageBlocklisted = false;

export function readMsFromStorage(key: string): number | null {
  if (!storageBlocklisted) {
    try {
      const v = window.localStorage.getItem(key);
      if (v !== null) {
        const n = Number(v);
        return Number.isFinite(n) ? n : null;
      }
    } catch {
      storageBlocklisted = true;
    }
  }
  return memoryFallback.get(key) ?? null;
}

export function writeMsToStorage(key: string, ms: number): void {
  memoryFallback.set(key, ms);
  if (storageBlocklisted) return;
  try {
    window.localStorage.setItem(key, String(ms));
  } catch {
    storageBlocklisted = true;
  }
}
