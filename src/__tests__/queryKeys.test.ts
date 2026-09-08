import { QueryClient, QueryObserver } from "@tanstack/react-query";
import { describe, expect, it, vi } from "vitest";

import { invalidateRepoScopedQueries } from "../lib/queryKeys";

describe("仓库切换时的 Checkpoints 缓存", () => {
  it("页面保持挂载且缓存仍新鲜时，重新查询新仓库记录", async () => {
    // 1. 通过真实 QueryObserver 模拟已挂载的 Checkpoints 页面。
    const client = new QueryClient();
    let currentRepo = "repo-a";
    const observer = new QueryObserver(client, {
      queryKey: ["list_checkpoints"],
      queryFn: async () => ({ repo: currentRepo, entries: [`${currentRepo}-checkpoint`] }),
      staleTime: 30_000,
    });
    const unsubscribe = observer.subscribe(() => {});
    try {
      await vi.waitFor(() => expect(observer.getCurrentResult().data?.repo).toBe("repo-a"));

      // 2. 使用生产切仓刷新入口，确认不会继续保留旧仓记录。
      currentRepo = "repo-b";
      invalidateRepoScopedQueries(client);
      await vi.waitFor(() =>
        expect(observer.getCurrentResult().data).toEqual({
          repo: "repo-b",
          entries: ["repo-b-checkpoint"],
        }),
      );
    } finally {
      unsubscribe();
      client.clear();
    }
  });
});
