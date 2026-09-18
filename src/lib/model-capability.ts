import type { GatewayModelItem } from "./types";

/**
 * model-capability.ts 把 `/v1/models` 的一条模型条目翻译成「可展示的三项能力」。
 *
 * 为什么单独成模块（而不是写在 AgentsPage 里）：这里的每一条都是**三态语义**
 * 的判定（知道 / 明确的否 / 不知道），而这些判定最容易在页面代码里被顺手
 * 压成布尔值 —— 一旦压掉，「上游没声明」就会被谎报成「不支持」，用户据此
 * 关掉一个本来可用的能力。集中在一处 + 有注释，比散在 JSX 里更难写错。
 *
 * 数据源：网关 `GET /v1/models`，经 Rust `ai_gateway_core::modules::gateway::fetch_models`
 * 的**原样透传**（逐字段 clone，不做形状改写）到达前端。没有新增任何 HTTP 调用：
 * 页面本来就在拉这份列表，只是此前只读了 `id`。
 *
 * 为什么能力字段要按多种拼写去读：网关**故意**一次下发多种拼写，因为各客户端
 * 解析器读的键名不统一且无统一约定（见 go-gateway/internal/server/capability.go
 * 的 `modelCapabilityFields` / `modelReasoningFields`）。这里按「主拼写优先、
 * 容错拼写兜底」读取。同一条响应里这些拼写表达的是**同一个事实**，因此优先级
 * 只在响应被手工篡改/半截损坏时才会起作用。
 */

/** 三态布尔：`undefined` = 未声明（不知道），不是「否」。 */
export type TriBool = boolean | undefined;

/** 取第一个「已定义」的值 —— 注意必须用 `!== undefined`，不能用真值判断，
 *  否则显式的 `false` 会被跳过，回落到另一个拼写上（正是这一压会把
 *  「明确的否」错读成「是」）。 */
function firstDefined<T>(...values: (T | undefined | null)[]): T | undefined {
  for (const v of values) {
    if (v !== undefined && v !== null) return v;
  }
  return undefined;
}

/** 归一化一个可能为字符串数组的字段：非数组或含非字符串 → 不采信。 */
function asStringList(value: unknown): string[] | undefined {
  if (!Array.isArray(value)) return undefined;
  const out = value.filter((x): x is string => typeof x === "string" && x.trim() !== "");
  // 空数组按「未声明」处理：网关在无档位时**根本不下发**这些键，
  // 真收到空数组只可能是半截/手工响应，采信它会得到「确定没有档位」这个假结论。
  return out.length > 0 ? out : undefined;
}

/** 把模态列表翻译成三态图片能力：列出 image → true；列出了别的但没 image → false。 */
function visionFromModalities(list: unknown): TriBool {
  const items = asStringList(list);
  if (!items) return undefined;
  return items.some((m) => m.toLowerCase() === "image");
}

/**
 * 模型是否支持图片输入（三态）。
 *
 * 优先读 `supportsImages`（网关主拼写）；其余拼写是同一事实的别名，
 * 兼容手工构造或其它来源的响应。全部缺失 → `undefined`（未声明）。
 */
export function visionOf(model: GatewayModelItem): TriBool {
  const cap = model.capabilities;
  const arch = model.architecture;
  return firstDefined<TriBool>(
    typeof model.supportsImages === "boolean" ? model.supportsImages : undefined,
    typeof cap?.vision === "boolean" ? cap.vision : undefined,
    typeof cap?.supports?.vision === "boolean" ? cap.supports.vision : undefined,
    visionFromModalities(model.input_modalities),
    visionFromModalities(model.inputModalities),
    visionFromModalities(arch?.input_modalities),
    // OpenRouter 风格："text+image->text" / "text->text"
    typeof arch?.modality === "string"
      ? arch.modality.toLowerCase().includes("image")
      : undefined,
  );
}

/**
 * 支持的思考档位（**全部**档位名，不是个数）。
 *
 * 返回 `undefined` = 未声明（含固定档模型）—— 必须与「有档位但为空」区分开：
 * 后者在本契约里不存在（网关不下发空数组），所以不采信空数组。
 * 顺序**沿用上游**：上游给的就是它自己的档位阶梯，重排会让「哪一档更强」
 * 这件事变成我们的猜测。
 */
export function effortsOf(model: GatewayModelItem): string[] | undefined {
  const reasoning = model.reasoning;
  return firstDefined<string[]>(
    asStringList(model.supported_efforts),
    asStringList(model.supportedEfforts),
    asStringList(model.reasoning_efforts),
    asStringList(model.reasoningEfforts),
    asStringList(reasoning?.supported_efforts),
  );
}

/** 默认思考档（未声明 → undefined；**绝不**用档位列表首项冒充）。 */
export function defaultEffortOf(model: GatewayModelItem): string | undefined {
  return firstDefined<string>(
    typeof model.default_effort === "string" ? model.default_effort : undefined,
    typeof model.defaultEffort === "string" ? model.defaultEffort : undefined,
    typeof model.default_reasoning_effort === "string" ? model.default_reasoning_effort : undefined,
    typeof model.reasoning?.default_effort === "string" ? model.reasoning.default_effort : undefined,
  );
}

/** 上下文窗口（token 数）。非有限正数 → `undefined`（未知，而不是 0）。 */
export function contextWindowOf(model: GatewayModelItem): number | undefined {
  const n = model.context_length;
  return typeof n === "number" && Number.isFinite(n) && n > 0 ? n : undefined;
}

/** 最大输出 token 数。 */
export function maxOutputTokensOf(model: GatewayModelItem): number | undefined {
  const n = model.max_output_tokens;
  return typeof n === "number" && Number.isFinite(n) && n > 0 ? n : undefined;
}

/**
 * 该模型名确有真值的上游区域。
 *
 * 只在网关**确认真值**单区可用时下发（capability.go 的 `capabilityFieldsFor`）。
 * 返回空数组 = 两边都有 / 未知 —— 这两种情况界面上都不加区域标记，
 * 因为「不知道」不该显示成「仅某一侧」。
 */
export function regionsOf(model: GatewayModelItem): string[] {
  const list = asStringList(model.supported_regions);
  return list ? list.map((r) => r.toLowerCase()) : [];
}

export interface RegionHint {
  /** 徽标文字，如「仅国服」。 */
  label: string;
  /** 悬浮说明：解释「只在一侧存在」意味着什么。 */
  title: string;
}

/**
 * 区域差异提示。
 *
 * 关键口径：**「只在一侧存在」不等于「不支持图片」** —— 网关按区域路由，会把
 * 该模型的请求送到它所在的那一侧，能力照常可用（capability.go 明确禁止把它
 * 降级成 false）。因此这里只加一条「在哪一侧」的说明，绝不改视觉/档位的结论，
 * 而是把 11102 这类报错的成因讲清楚（否则用户会怀疑模型名拼错）。
 */
export function regionHintOf(model: GatewayModelItem): RegionHint | undefined {
  const regions = regionsOf(model);
  if (regions.length !== 1) return undefined;

  const isCN = regions[0] === "cn";
  const label = isCN ? "仅国服" : "仅国际版";
  const other = isCN ? "国际版" : "国服";
  // 优先用网关给的原话：它是与路由策略同源生成的，措辞改动只该发生在一处。
  const title =
    typeof model.region_note === "string" && model.region_note.trim() !== ""
      ? model.region_note
      : `该模型名仅在${isCN ? "国服" : "国际版"}上游存在；${other}账号调用它会返回 11102 model service info not found。网关会把它的请求路由到有它的那一侧。`;
  return { label, title };
}

/**
 * 把 token 数格式化成可读文本（如 128K / 1M）。
 *
 * 为什么**不**统一用 1000 进制：本项目的上下文值两种进制混用，而两者在
 * 1000 进制下会算错成同一个数 —— 1048576（真实 1Mi）与 1000000（真实 1M）
 * 都变成 "1.0M"，于是用户无法判断模型到底是 1024Ki 还是 1000K。
 * 这里优先选**能整除**的那种进制，两者都不整除时才退回一位小数。
 *
 * 精确原始值不进返回值，由调用方放进 `title` 悬浮提示 —— 概览要短，
 * 但「信息不为了清爽而丢」要求精确值仍然拿得到。
 */
export function formatTokenCount(n: number | undefined): string | undefined {
  if (n === undefined || !Number.isFinite(n) || n <= 0) return undefined;
  const K1024 = 1024;
  const M1024 = 1024 * 1024;
  if (n >= M1024 && n % M1024 === 0) return `${n / M1024}M`;
  if (n >= 1_000_000 && n % 1_000_000 === 0) return `${n / 1_000_000}M`;
  if (n >= K1024 && n % K1024 === 0) return `${n / K1024}K`;
  if (n >= 1000 && n % 1000 === 0) return `${n / 1000}K`;
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1000) return `${(n / 1000).toFixed(1)}K`;
  return String(n);
}

/** 原始 token 数（带千分位），供悬浮提示展示精确值。 */
export function exactTokenCount(n: number | undefined): string | undefined {
  if (n === undefined || !Number.isFinite(n) || n <= 0) return undefined;
  return n.toLocaleString("en-US");
}

/**
 * 思考档位的中文标签。
 *
 * 只对**有公认中文说法**的档位给标签；表里没有的名字**原样显示**（不翻译、
 * 不猜测强度顺序）—— 编一个中文名等于替上游下结论。
 */
const EFFORT_LABELS: Record<string, string> = {
  none: "不思考",
  minimal: "最低",
  low: "低",
  medium: "中",
  high: "高",
  xhigh: "极高",
  max: "最高",
};

/** 档位显示文本：中文标签（若有）+ 原始档位名。 */
export function effortLabel(effort: string): string {
  const zh = EFFORT_LABELS[effort.toLowerCase()];
  return zh ? `${zh} ${effort}` : effort;
}

/** 一项能力的展示态。 */
export interface CapabilityDisplay {
  /** 主文本（概览行的值）。 */
  text: string;
  /** 悬浮提示：精确值 / 语义解释。 */
  title?: string;
  /** 是否为「未知」态。未知与「明确的否」必须用不同表达。 */
  unknown: boolean;
  /**
   * 需要**逐项**渲染时的原始条目（目前只有思考档位用）。
   *
   * 为什么返回数组而不是让调用方去切 `text`：档位名来自上游，理论上可以含
   * 任何字符，靠分隔符切字符串是脆弱的隐式契约。这里显式给出条目列表，
   * 调用方按项渲染，显示与数据不会因为一个奇怪的档位名而错位。
   */
  items?: string[];
}

/** 「未知」的统一表达。全项目「—」= 不知道（见 GatewayPage 的既有口径）。 */
export const UNKNOWN_TEXT = "—";

/**
 * 上下文长度展示态。
 *
 * 未知时是「—」而不是「0」：0 在本项目里是**确定的否定**，用它表示「拿不到」
 * 会让用户以为模型真的没有上下文。
 */
export function contextDisplay(model: GatewayModelItem): CapabilityDisplay {
  const ctx = contextWindowOf(model);
  if (ctx === undefined) {
    return { text: UNKNOWN_TEXT, title: "网关未下发该模型的上下文长度，无法确认", unknown: true };
  }
  const maxOut = maxOutputTokensOf(model);
  const parts = [`上下文 ${exactTokenCount(ctx)} tokens`];
  if (maxOut !== undefined) parts.push(`最大输出 ${exactTokenCount(maxOut)} tokens`);
  // formatTokenCount 对任何 >0 的有限数都返回字符串，故这里必然有值；
  // 仍用 `?? UNKNOWN_TEXT` 兜底而不是 `!`：断言在运行时不存在，
  // 万一契约变化会静默渲染出 "undefined"，兜底至少还是「未知」的表达。
  return { text: formatTokenCount(ctx) ?? UNKNOWN_TEXT, title: parts.join(" · "), unknown: false };
}

/**
 * 视觉能力展示态。
 *
 * 三态各有**不同**表达：
 *   支持 → 「视觉」；不支持 → 「纯文本」；未声明 → 「—」+「未声明」。
 * 「未声明」绝不能写成「不支持」：网关在拿不到真值时故意不下发该字段，
 * 把它渲染成否定会让用户关掉一个可能本来可用的能力。
 */
export function visionDisplay(model: GatewayModelItem): CapabilityDisplay {
  const vision = visionOf(model);
  if (vision === undefined) {
    return {
      text: UNKNOWN_TEXT,
      title: "上游未声明图片能力（网关在无真值时不下发该字段），不代表不支持",
      unknown: true,
    };
  }
  return vision
    ? { text: "视觉", title: "支持图片输入（能收图片）", unknown: false }
    : { text: "纯文本", title: "上游明确声明不支持图片输入", unknown: false };
}

/**
 * 思考档位展示态。
 *
 * **列出全部档位名**（所有者明确要求「信息不能为了清爽而丢」）：
 * 只显示「支持 3 档」会把用户真正要选的 max 藏起来。
 * 未知时是「—」，不是「无档位」—— 后者是确定的否定。
 */
export function effortsDisplay(model: GatewayModelItem): CapabilityDisplay {
  const efforts = effortsOf(model);
  if (!efforts) {
    return {
      text: UNKNOWN_TEXT,
      title: "上游未声明思考档位（固定档模型或网关无真值时都不下发），不代表没有档位",
      unknown: true,
    };
  }
  const def = defaultEffortOf(model);
  // 显示用**上游原始档位名**（`low`/`high`/`max`）：它是用户真正要写进配置、
  // 也是网关降级逻辑实际比对的值（见 upstream/payload.go 的
  // normalizeReasoningEffort），翻译成中文会让「照着界面上写」写错。
  // 中文含义放 Tooltip 里补充，两者都不丢。
  const known = efforts.filter((e) => EFFORT_LABELS[e.toLowerCase()] !== undefined);
  const title =
    `支持 ${efforts.length} 档：${efforts.join(" / ")}` +
    (known.length > 0 ? `（${known.map(effortLabel).join(" / ")}）` : "") +
    (def ? ` · 未指定时默认 ${def}` : "");
  return { text: efforts.join(" / "), title, unknown: false, items: efforts };
}

/** 一条模型的完整能力展示模型。 */
export interface ModelCapabilityView {
  model: GatewayModelItem;
  context: CapabilityDisplay;
  vision: CapabilityDisplay;
  efforts: CapabilityDisplay;
  region?: RegionHint;
}

/** 一次性算出该模型要展示的全部能力（页面据此渲染，避免多次解析）。 */
export function capabilityViewOf(model: GatewayModelItem): ModelCapabilityView {
  return {
    model,
    context: contextDisplay(model),
    vision: visionDisplay(model),
    efforts: effortsDisplay(model),
    region: regionHintOf(model),
  };
}

/**
 * 能力概览统计：给区块标题一个「有多少模型缺真值」的交代。
 *
 * 为什么需要它：单项显示「—」时用户不知道这是个别现象还是网关整体拉不到真值。
 * 给出计数后，「全部 12 个模型都未知」与「只有 1 个未知」一眼可分 ——
 * 前者是网关/上游问题，后者是那个模型本身没声明。
 */
export interface CapabilitySummary {
  total: number;
  visionKnown: number;
  effortsKnown: number;
  contextKnown: number;
  /** 仅单区可用的模型数（区域差异需要用户注意）。 */
  singleRegion: number;
}

export function summarizeCapabilities(models: GatewayModelItem[]): CapabilitySummary {
  let visionKnown = 0;
  let effortsKnown = 0;
  let contextKnown = 0;
  let singleRegion = 0;
  for (const m of models) {
    if (visionOf(m) !== undefined) visionKnown++;
    if (effortsOf(m)) effortsKnown++;
    if (contextWindowOf(m) !== undefined) contextKnown++;
    if (regionsOf(m).length === 1) singleRegion++;
  }
  return { total: models.length, visionKnown, effortsKnown, contextKnown, singleRegion };
}
