/**
 * 桌面宠物的纯逻辑层 + 形象主题常量。详见 ADR-011。
 *
 * # 信息层 vs 审美层(ADR-011 子决策 1,不可破坏的护城河)
 * 角色图是「审美层」(可换形象皮肤);叠在角色身上的双色环是「信息层」:紫弧(INK_AI)=
 * AI 占比、蓝弧(INK_YOU)=你的占比。信息层的两个语义色是**全局常量**,任何形象主题都
 * 不能改写"哪个色代表谁" —— 一旦可改,宠物就不再编码数据,退化成普通换肤宠物。
 *
 * # 状态收敛(子决策 3:单向数据流)
 * 主窗口 InkPetController 复用现有 watcher 的 react-query 数据,跑 `decidePetState` 纯
 * 函数,把结果 emit 给 pet 窗口渲染。本模块不持有任何 state,易单测。
 */

import robot3dIdle from "../assets/pet/robot3d/idle.png";
import robot3dHappy from "../assets/pet/robot3d/happy.png";
import robot3dSleep from "../assets/pet/robot3d/sleep.png";
import robot3dAlert from "../assets/pet/robot3d/alert.png";
import robotFlatIdle from "../assets/pet/robotflat/idle.png";
import robotFlatHappy from "../assets/pet/robotflat/happy.png";
import robotFlatSleep from "../assets/pet/robotflat/sleep.png";
import robotFlatAlert from "../assets/pet/robotflat/alert.png";
import inkbeastIdle from "../assets/pet/inkbeast/idle.png";
import inkbeastHappy from "../assets/pet/inkbeast/happy.png";
import inkbeastSleep from "../assets/pet/inkbeast/sleep.png";
import inkbeastAlert from "../assets/pet/inkbeast/alert.png";
import type { SupportedLanguage } from "../i18n";

/** pet 窗口与主窗口之间的状态推送事件名(主窗 emit / pet listen)。 */
export const PET_STATE_EVENT = "git-ai-studio://pet-state";

/** pet 窗口挂载就绪后发出;主窗收到立刻补发一次当前状态(消除迟挂载的同步窗口期)。 */
export const PET_READY_EVENT = "git-ai-studio://pet-ready";

/** pet 窗口的交互(右键菜单 / 双击)回传给主窗执行的命令事件。 */
export const PET_COMMAND_EVENT = "git-ai-studio://pet-command";

/** 信息层语义色(ADR-011 锁死,主题不可改写):紫 = AI 占比,蓝 = 你的占比。 */
export const INK_AI = "#7C6BD6";
export const INK_YOU = "#3A8FB7";

/** 宠物的 7 个逻辑状态,声明顺序即显示优先级(高 → 低)。 */
export type PetStateKind =
  | "attribution_failed" // 打标失败:整体染红 + 抖动 + 醒目提醒
  | "daemon_unhealthy" // daemon 卡:整体染灰 + 抖动 + 醒目提醒
  | "hook_missing" // hook 未配置:整体染黄 + 醒目提醒
  | "attributing" // 正在打标:脉冲(瞬态)
  | "low_ai_share" // AI 率过低:轻蓝染氛围提示
  | "sleeping" // 长时间无 git 活动:睡眠态
  | "ok" // 一切正常
  | "unknown"; // 数据尚未加载:中性"加载中"占位,不粉饰为 ok。由 controller 在数据未就绪时直接下发,不经 decidePetState

/** 角色实拍的 4 种姿态图;7 个逻辑状态映射到这 4 张图 + 程序化效果。 */
export type PetVisual = "idle" | "happy" | "sleep" | "alert";

/** 主窗 emit 给 pet 窗的完整状态载荷(pet 窗纯渲染,所有渲染参数都由主窗下发)。 */
export interface PetStatePayload {
  kind: PetStateKind;
  /** 当周 AI 占比 [0,100];null = 无数据(样本不足 / 未选仓库)。驱动双色环配比。 */
  aiSharePercent: number | null;
  /** 形象主题 id(robot3d / robotflat / inkbeast);pet 窗据此取图。 */
  themeId: string;
  /** 整体不透明度 [0,1]。 */
  opacity: number;
  /** 醒目提醒重复间隔(秒);0 = 只在状态切入时提醒一次、不重复。 */
  alertIntervalSec: number;
  /** 窗口 / 画布边长(px),由尺寸档位映射而来。 */
  sizePx: number;
  /** 主窗当前生效语言。pet 是独立 webview(独立 i18n 实例),靠这条同步语言 —— 否则
   *  右键菜单 / 气泡会停在 pet 窗自己启动时检测到的语言,与主窗切换后不一致。 */
  lang: SupportedLanguage;
}

/** `decidePetState` 的输入。各字段由 InkPetController 从现有 query 派生。 */
export interface PetStateInput {
  /** failed_shas 非空(近窗口有 commit 打标失败)。 */
  attributionFailed: boolean;
  /** daemon 处于 blocked_lock_unknown_pid。 */
  daemonUnhealthy: boolean;
  /** 有被检测到但未配置 hook 的 agent。 */
  hookMissing: boolean;
  /** 刚收到 notes-updated 事件,短暂处于"正在打标"。 */
  attributing: boolean;
  /** AI 率低于阈值(且样本足够)。 */
  lowAiShare: boolean;
  /** 距上次 git 活动超过空闲阈值。 */
  idle: boolean;
}

/**
 * 把多源信号收敛成单一可渲染状态。**优先级仲裁**(高 → 低):
 * 打标失败 > daemon 卡 > hook 缺 > 打标中 > 低 AI 率 > 睡眠 > 正常。
 *
 * 异常态(前 3)压倒一切 —— 它们是"用户需要并能立刻处理的问题";瞬态打标动画次之;
 * 低 AI / 睡眠是最低优先的"氛围信号"。纯函数,无副作用。
 */
export function decidePetState(input: PetStateInput): PetStateKind {
  if (input.attributionFailed) return "attribution_failed";
  if (input.daemonUnhealthy) return "daemon_unhealthy";
  if (input.hookMissing) return "hook_missing";
  if (input.attributing) return "attributing";
  if (input.lowAiShare) return "low_ai_share";
  if (input.idle) return "sleeping";
  return "ok";
}

/**
 * 状态的视觉表现(渲染层据此选图 + 叠效果)。
 * - `visual`:用哪张姿态图
 * - `tint`:整体染色叠加(rgba 字符串,只染角色非透明像素),null = 不染
 * - `shake`:左右抖动(强调"出问题了")
 * - `pulse`:缩放脉冲(强调"正在工作")
 * - `urgent`:醒目提醒(常驻文字气泡 + 按设置间隔重复抖动提醒)
 */
export interface StateVisual {
  visual: PetVisual;
  tint: string | null;
  shake: boolean;
  pulse: boolean;
  urgent: boolean;
}

/** 把逻辑状态 + AI 率映射成视觉表现。纯函数。 */
export function visualForState(kind: PetStateKind, aiSharePercent: number | null): StateVisual {
  switch (kind) {
    // 打标失败:整只染红 —— 最强提醒(用户明确要求)
    case "attribution_failed":
      return {
        visual: "alert",
        tint: "rgba(220,38,38,0.55)",
        shake: true,
        pulse: false,
        urgent: true,
      };
    // daemon 卡:染灰(故障感)+ 抖动
    case "daemon_unhealthy":
      return {
        visual: "alert",
        tint: "rgba(108,112,122,0.50)",
        shake: true,
        pulse: false,
        urgent: true,
      };
    // hook 缺:染黄(警告)
    case "hook_missing":
      return {
        visual: "alert",
        tint: "rgba(234,179,8,0.42)",
        shake: false,
        pulse: false,
        urgent: true,
      };
    // 打标中:脉冲(工作动效),瞬态
    case "attributing":
      return { visual: "idle", tint: null, shake: false, pulse: true, urgent: false };
    // 低 AI 率:轻蓝染(氛围信号,不打扰)
    case "low_ai_share":
      return {
        visual: "idle",
        tint: "rgba(58,143,183,0.26)",
        shake: false,
        pulse: false,
        urgent: false,
      };
    case "sleeping":
      return { visual: "sleep", tint: null, shake: false, pulse: false, urgent: false };
    // 数据尚未加载:中性常态脸 + null 占位环(hover 气泡显示"加载中"),不发声、不粉饰为正常
    case "unknown":
      return { visual: "idle", tint: null, shake: false, pulse: false, urgent: false };
    // 正常:AI 活跃时露出开心表情,否则常态
    case "ok":
    default:
      return {
        visual: (aiSharePercent ?? 0) >= 50 ? "happy" : "idle",
        tint: null,
        shake: false,
        pulse: false,
        urgent: false,
      };
  }
}

/**
 * 形象主题 = 一套 4 态角色图 + 双色环在窗口中的位置。
 * 主题只换「角色皮肤」与「环的位置」,不碰信息层语义色(INK_AI / INK_YOU 全局锁死)。
 */
export interface PetTheme {
  id: string;
  /** 4 种姿态图的资源 URL(vite 处理后的路径)。 */
  images: Record<PetVisual, string>;
  /** 双色环中心与半径,窗口归一化坐标 [0,1](对准角色胸口屏 / 身前卷轴)。 */
  gauge: { cx: number; cy: number; r: number };
}

/** v1 三套内置形象主题(不做主题文件加载,推 v2)。 */
export const PET_THEMES: readonly PetTheme[] = [
  {
    id: "robot3d",
    images: { idle: robot3dIdle, happy: robot3dHappy, sleep: robot3dSleep, alert: robot3dAlert },
    gauge: { cx: 0.5, cy: 0.6, r: 0.14 },
  },
  {
    id: "robotflat",
    images: {
      idle: robotFlatIdle,
      happy: robotFlatHappy,
      sleep: robotFlatSleep,
      alert: robotFlatAlert,
    },
    gauge: { cx: 0.5, cy: 0.62, r: 0.13 },
  },
  {
    id: "inkbeast",
    images: {
      idle: inkbeastIdle,
      happy: inkbeastHappy,
      sleep: inkbeastSleep,
      alert: inkbeastAlert,
    },
    gauge: { cx: 0.48, cy: 0.64, r: 0.12 },
  },
] as const;

export const DEFAULT_PET_THEME_ID = "robot3d";

/** 按 id 取主题;未知 id(含 null / undefined)退回默认,绝不崩。 */
export function resolveTheme(themeId: string | null | undefined): PetTheme {
  return PET_THEMES.find((t) => t.id === themeId) ?? PET_THEMES[0];
}
