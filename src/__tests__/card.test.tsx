// Card 组件渲染契约(任务 #5 报表 UI 美化)。
//
// # 测什么
// 1. 默认渲染:rounded-xl + bg-card + ring,无 shadow-xs
// 2. title/icon/actions:三 prop 都生效,渲染顺序正确
// 3. interactive:加 hover border 过渡类
// 4. padding 档:none/sm/md/lg 各档拼接到 body 上
//
// # 测试环境
// vitest environment=node + react-dom/server renderToStaticMarkup —— 项目无 jsdom,
// 静态 markup 已足够断言 className 拼接 / 结构层级。

import { describe, expect, it } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";

import { MetricCard } from "../components/MetricCard";
import { Card, CardBody, CardFooter, CardHeader } from "../components/ui/CardPanel";

describe("Card / 默认样式", () => {
  it("根节点是 rounded-xl + bg-card + ring,不带 shadow-xs", () => {
    const html = renderToStaticMarkup(<Card>hello</Card>);
    expect(html).toContain("rounded-xl");
    expect(html).toContain("bg-card");
    expect(html).toContain("ring-1");
    expect(html).toContain("ring-border/40");
    expect(html).not.toContain("shadow-xs");
  });

  it("无 title/icon/actions 时不渲染 header 行,children 直接放在带 padding 的 body", () => {
    const html = renderToStaticMarkup(<Card padding="sm">body-only</Card>);
    expect(html).toContain("body-only");
    expect(html).toContain("p-3");
    // 没有 header 的 px-4 py-2.5 行
    expect(html).not.toContain("px-4");
  });
});

describe("Card / title + icon + actions", () => {
  it("三 prop 都给时渲染 header + body 两段,顺序正确", () => {
    const html = renderToStaticMarkup(
      <Card
        title="工具 / 模型分布"
        icon={<span data-testid="icon">I</span>}
        actions={<button>act</button>}
      >
        <p>content</p>
      </Card>,
    );
    // header 行
    expect(html).toContain("工具 / 模型分布");
    expect(html).toContain('data-testid="icon"');
    expect(html).toContain("<button>act</button>");
    // 分割线类
    expect(html).toContain("border-b");
    // body 内容
    expect(html).toContain("<p>content</p>");
    // 顺序:icon/title 出现在 actions 之前(header 内左右布局)
    expect(html.indexOf("工具 / 模型分布")).toBeLessThan(html.indexOf("act"));
    // body 出现在 header 之后
    expect(html.indexOf("act")).toBeLessThan(html.indexOf("content"));
  });

  it("仅给 title 也能渲染(icon/actions 可选)", () => {
    const html = renderToStaticMarkup(<Card title="标题">x</Card>);
    expect(html).toContain("标题");
    expect(html).toContain("x");
  });
});

describe("Card / interactive hover", () => {
  it("interactive=true 时拼上 hover:border-primary/40 与 transition-colors", () => {
    const html = renderToStaticMarkup(<Card interactive>m</Card>);
    expect(html).toContain("hover:border-primary/40");
    expect(html).toContain("transition-colors");
  });

  it("interactive 默认 false → 不拼 hover 类", () => {
    const html = renderToStaticMarkup(<Card>m</Card>);
    expect(html).not.toContain("hover:border-primary/40");
  });
});

describe("Card / tone 左色条 + elevated 层级", () => {
  it("默认 tone=neutral 不加任何 border-l(防现有卡左移 2px)", () => {
    const html = renderToStaticMarkup(<Card>x</Card>);
    expect(html).not.toContain("border-l-");
  });

  it("tone=ai 加 border-l-2 border-l-ai", () => {
    const html = renderToStaticMarkup(<Card tone="ai">x</Card>);
    expect(html).toContain("border-l-2");
    expect(html).toContain("border-l-ai");
  });

  it("tone=human 加 border-l-2 border-l-human", () => {
    const html = renderToStaticMarkup(<Card tone="human">x</Card>);
    expect(html).toContain("border-l-human");
  });

  it("elevated=true 用 bg-card-elevated;默认用 bg-card", () => {
    expect(renderToStaticMarkup(<Card elevated>x</Card>)).toContain("bg-card-elevated");
    expect(renderToStaticMarkup(<Card>x</Card>)).not.toContain("bg-card-elevated");
  });
});

describe("Card / padding 档", () => {
  it.each([
    ["sm", "p-3"],
    ["md", "p-4"],
    ["lg", "p-6"],
  ] as const)("padding=%s → 拼 %s", (pad, cls) => {
    const html = renderToStaticMarkup(<Card padding={pad}>x</Card>);
    expect(html).toContain(cls);
  });

  it("padding=none → 不拼 p-* 类(由调用方自行包内边距)", () => {
    const html = renderToStaticMarkup(<Card padding="none">x</Card>);
    expect(html).not.toMatch(/\sp-\d+/);
  });
});

describe("Card / 子组件直接 import", () => {
  it("CardHeader / CardBody / CardFooter 各自带预期 className", () => {
    const html = renderToStaticMarkup(
      <Card padding="none">
        <CardHeader>h</CardHeader>
        <CardBody padding="md">b</CardBody>
        <CardFooter>f</CardFooter>
      </Card>,
    );
    // CardHeader / CardFooter 都有 border-b/border-t 分割线
    expect(html).toContain("border-b");
    expect(html).toContain("border-t");
    // body p-4
    expect(html).toContain("p-4");
    // 三段内容都在
    expect(html).toContain(">h<");
    expect(html).toContain(">b<");
    expect(html).toContain(">f<");
  });
});

describe("MetricCard / 指标说明入口", () => {
  it("help 与标题并排渲染,供指标公式按钮注入", () => {
    const html = renderToStaticMarkup(
      <MetricCard
        title="AI 占比"
        display="80.0%"
        help={<button aria-label="查看 AI 占比统计口径">i</button>}
      />,
    );
    expect(html).toContain("AI 占比");
    expect(html).toContain("80.0%");
    expect(html).toContain('aria-label="查看 AI 占比统计口径"');
    expect(html.indexOf("AI 占比")).toBeLessThan(html.indexOf("80.0%"));
  });
});
