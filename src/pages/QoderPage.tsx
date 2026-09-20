import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { toast } from "sonner";
import {
  AlertTriangle,
  CheckCircle2,
  Download,
  ExternalLink,
  Gift,
  Globe,
  Loader2,
  RefreshCw,
  Upload,
} from "lucide-react";
import { QoderMark } from "@/components/product-marks";
import { ProductAccountCard, ProductAccountGrid } from "@/components/product-account-card";
// 记录视图：任务执行记录 + 额度消耗明细（所有者 2026-09-20 要求）。
// 与 WorkBuddy 账号卡共用同一个组件 —— 三处的筛选与措辞必须一致。
import { AccountRecordsView } from "@/components/account-records-view";
import type { ProductAccountTask } from "@/components/product-account-card";
import { openInDefaultBrowser } from "@/lib/open-browser";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Separator } from "@/components/ui/separator";
import { Skeleton } from "@/components/ui/skeleton";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import * as api from "@/lib/api";
import type { QoderAccountRow, QoderRegion, QoderSummary } from "@/lib/api";
import { cn } from "@/lib/utils";

/**
 * Qoder 账号页（C4）。
 *
 * 两种登录方式（所有者要求「两个方式都要支持」）：
 *   A 软件内授权 —— 生成 PKCE 设备流链接 → 用户在浏览器确认 → 轮询取令牌
 *   B 导入凭证   —— 用户自备的凭证文件（单个文件或目录）
 *
 * 两者最终写入**同一套账号存储**，登录后行为完全一致。
 *
 * ## 区域为什么必须显式选
 *
 * 国服与国际版的**授权页不同**（qoder.com.cn / qoder.com），API 域也不同
 * （qoder.com.cn / qoder.sh）。选错区域会让用户打开错误区域的页面、
 * 登录后账号却指向另一个区 —— 这种错很难被发现，故不给默认值。
 *
 * ## 为什么显示「凭证缺失」而不是直接隐藏
 *
 * 账号元信息与凭证文件是分开存的：删了凭证文件但元信息还在时，
 * 该账号**在网关里已不可用**。若界面照常显示"正常"，用户会以为还能用。
 * 故显式标出并提示重新登录。
 */

/** 区域标签。空串是"未指定"，不是国服 —— 见文件头注释。 */
function regionLabel(region: QoderRegion): string {
  if (region === "cn") return "国服";
  if (region === "intl") return "国际版";
  return "未指定";
}

/** 区域徽章样式：两区用不同颜色，一眼能分辨。 */
function regionVariant(region: QoderRegion): "default" | "secondary" | "outline" {
  if (region === "intl") return "default";
  if (region === "cn") return "secondary";
  return "outline";
}

/** 到期时间展示。 */
function expiryText(expireAt: number): { text: string; urgent: boolean } {
  if (!expireAt) return { text: "未知", urgent: false };
  // 后端给的是 Unix 秒；>1e12 说明已经是毫秒（容错）
  const ms = expireAt > 1e12 ? expireAt : expireAt * 1000;
  const days = Math.floor((ms - Date.now()) / 86400000);
  const date = new Date(ms).toLocaleDateString();
  if (days < 0) return { text: `已过期（${date}）`, urgent: true };
  if (days <= 7) return { text: `${days} 天后（${date}）`, urgent: true };
  return { text: date, urgent: false };
}


/** 活动的一条"展示位"内容（上游给的**真实文案**，按语言分组）。 */
type CampaignPlacementContent = {
  buttonText?: string;
  description?: string;
  detailUrl?: string;
};

/** 从活动里取**中文**展示内容（优先 zh，回落 zh-CN/zh-Hans，再回落第一个）。 */
function placementContentOf(
  c: api.QoderCampaign,
): CampaignPlacementContent | undefined {
  const placements = c.placements || [];
  // ⚠ 必须挑**有 description 的**那一条：上游同时给 POPUP 与 USAGE 两个
  // 展示位，且 `campaignUrl` 只在 POPUP 上有。若按"第一个"取，
  // 遇到 USAGE 在前的账号会拿到 campaignUrl 为空的那条。
  const withContent = placements.filter((p) => {
    const zh = p.content?.zh;
    const any = Object.values(p.content || {})[0] as CampaignPlacementContent | undefined;
    return Boolean((zh?.description || any?.description) ?? (zh?.detailUrl || any?.detailUrl));
  });
  const pick = withContent[0] || placements[0];
  if (!pick?.content) return undefined;
  const byLang = pick.content;
  return (
    byLang.zh ||
    byLang["zh-CN"] ||
    byLang["zh-Hans"] ||
    byLang.zh_CN ||
    // 没有中文时回落第一个语言 —— **宁可显示英文**也不要显示我自己编的中文
    (Object.values(byLang)[0] as CampaignPlacementContent | undefined)
  );
}

/**
 * 活动的**标题**（全部来自接口，不写死）。
 *
 * # 为什么改（所有者 2026-09-20 指出）
 *
 * 他问：「活动既然是通过接口下发的,是不是能领取的活动也可以动态实现,
 * 而不是写死?」—— 问得对，而且原实现**确实在界面上显示了我自己编的文案**：
 *
 *	benefit 存在        → `领 100 Credits`         ← 这条是接口数据，对的
 *	actionType=VIEW_DETAILS → `活动详情`            ← **写死**
 *	benefit.validity 缺失   → `限时活动`            ← **写死**
 *
 * 而接口**明明给了真实文案**（实测 wish 账号的原始返回）：
 *
 *	placements[0].content.zh.description
 *	  = "每日 10:00（UTC+8）刷新，领取后 30 天有效"
 *	placements[1].content.zh.description
 *	  = "专业版 4,000 Qwen Credits，高级版 12,000。续费、升级加赠 1,000。"
 *
 * 也就是说：**上游的活动名称/描述都会变**（活动是按日期轮换的，
 * 实测 campaignKey 形如 `act-20260918-628`），写死意味着上游换个活动、
 * 我们就显示错的名字。用户会以为"还是上次那个活动"，而其实是新的。
 *
 * # 优先级
 *
 *	1. 可领取（有 benefit）→ `领 {金额} {单位}` —— 这是**用户最关心的信息**
 *	   （能领多少），比活动描述更重要，故仍排在前面
 *	2. 接口给的描述（`placements[].content.zh.description`）
 *	3. 上游活动标识 `campaignKey`（**真实值**，只是不好看）
 *
 * ⚠ 不再有"活动详情"这种兜底 —— 那是编的。取不到就显示上游的真实标识。
 */
function campaignTitleOf(c: api.QoderCampaign): string {
  if (c.benefit) {
    const unit = c.benefit.kind === "CREDITS" ? "Credits" : c.benefit.kind;
    return `领 ${c.benefit.amount} ${unit}`;
  }
  const desc = placementContentOf(c)?.description;
  if (desc) return desc;
  // 最后回落到**上游真实标识**（不是编的文案）
  return c.campaignKey || c.campaignId || "未知活动";
}

/**
 * 活动的**副标题**（全部来自接口，不写死）。
 *
 * 优先级：
 *	1. 接口给的描述（当标题已经用了"领 N Credits"时，描述放这里最合适）
 *	2. 有效期（`benefit.validity`）
 *	3. 活动时间窗（`startAt`~`endAt`，上游给的真实时间）
 *
 * ⚠ 原实现兜底是 `"限时活动"` —— 那也是编的。上游**给了真实时间窗**，
 * 没有任何理由显示一个空泛的四字词。
 */
function campaignSubtitleOf(c: api.QoderCampaign): string | undefined {
  const parts: string[] = [];
  // ⚠ 标题已经用了描述时不再重复显示（避免同一个活动名出现两遍）
  const desc = placementContentOf(c)?.description;
  if (desc && desc !== campaignTitleOf(c)) parts.push(desc);
  if (c.benefit?.validity?.days) {
    parts.push(`领取后 ${c.benefit.validity.days} 天内有效`);
  }
  if (parts.length === 0 && c.startAt > 1e9 && c.endAt > 1e9) {
    const fmt = (s: number) =>
      new Date(s * 1000).toLocaleDateString("zh-CN", { month: "numeric", day: "numeric" });
    parts.push(`${fmt(c.startAt)} 至 ${fmt(c.endAt)}`);
  }
  return parts.length > 0 ? parts.join(" · ") : undefined;
}


/**
 * 把若干条奖励累加成可读文本（如 `+100 Credits`）。
 *
 * **同单位相加、不同单位并列**：上游的 `kind` 实测是 `CREDITS`，
 * 但它是字符串而不是枚举 —— 遇到没见过的新单位时如实显示它的原值，
 * 不猜成 Credits（猜错等于报了一个假的金额单位）。
 */
function rewardTextOf(items: Array<{ kind: string; amount: number }>): string | undefined {
  if (items.length === 0) return undefined;
  const byUnit = new Map<string, number>();
  for (const b of items) {
    const unit = b.kind === "CREDITS" ? "Credits" : b.kind;
    byUnit.set(unit, (byUnit.get(unit) || 0) + b.amount);
  }
  return [...byUnit.entries()]
    .map(([unit, amount]) => `+${amount.toLocaleString()} ${unit}`)
    .join("、");
}


/**
 * 该活动**是否已领**（前端幂等判断的唯一依据）。
 *
 * ⚠ 前端置灰只减少无谓点击，**不能替代后端判断** —— 上游对重复领取会回
 * `replayed:true`（见 `api.qoderClaimCampaign` 的说明），那才是权威答案。
 */
function isClaimedCampaign(c: api.QoderCampaign): boolean {
  return c.claimStatus === "CLAIMED";
}

/**
 * 该活动**是不是"能领东西"的真活动**（而不是广告位）。
 *
 * # 为什么单独一个函数
 *
 * 这条判据要在**三处**用同一份：任务清单、可领计数、单账号领取的目标筛选。
 * 各写一遍必然分叉 —— 那正是我第一版的错误来源（后端与前端各写了一份
 * `claimStatus` 判据，两份都漏了 `actionType`）。
 *
 * 判据：`actionType === "CLAIM_BENEFIT"` 或有 `benefit`。
 *
 *	两者取"或"而不是"与"：实测真活动**两者都有**
 *	（`CLAIM_BENEFIT` + `benefit{100 CREDITS}`），但上游字段名历史上变过，
 *	任一条成立就足以说明"这条能领到东西"。
 *
 * `VIEW_DETAILS` 是**广告位**：它也带 placements、也回 CLAIMED，
 * 但它上面那个「已领取」是**整个活动页**的状态，不是"你领到了这个"。
 * 故它既不该进任务清单，也不该显示为"已完成"。
 */
function isBenefitCampaign(c: api.QoderCampaign): boolean {
  if (Boolean(c.benefit)) return true;
  return (c.actionType || "").toUpperCase() === "CLAIM_BENEFIT";
}

/**
 * 该活动**能不能领**。
 *
 *	· 已领（CLAIMED）/ 已过期（EXPIRED）→ 不能领
 *	· **不是领取类**（`VIEW_DETAILS` 等广告位）→ 不能领
 *	  （见 `isBenefitCampaign`：那类条目根本没有 benefit）
 *	· 状态为空 → **仍按可领处理**，与后端 `count_claimable` 同一取向
 *	  （"上游没给状态时不预设为已领取，否则用户明明能领却看不到按钮"）
 *
 * ⚠ 判据必须与后端 `is_claimable_campaign`（`qoder_login.rs`）**逐条一致**。
 * 两边分叉的后果很具体：界面说"2 个可领取"而实际只领到 1 个 ——
 * 用户会认为这个按钮在骗他。此前两边**都**漏了 actionType 这一条。
 */
function isClaimableCampaign(c: api.QoderCampaign): boolean {
  if (isClaimedCampaign(c) || c.claimStatus === "EXPIRED") return false;
  return isBenefitCampaign(c);
}

/**
 * 一个账号下**要展示的活动**（只做去重与排序，文案一律来自接口）。
 *
 * # 为什么要去重
 *
 * 上游对一个活动会给多条"展示位"（实测 POPUP + USAGE 两条 `placements`，
 * **`campaignId` 相同**）。若按展示位逐条渲染，卡片上会出现两行一模一样的
 * 「领 100 Credits」。去重键取 `campaignId`，缺 id 时回落 `campaignKey`。
 *
 * # 为什么可领的排前面
 *
 * 用户打开这一页就是为了"把能领的领掉"，已领的排在前面会让他多滚一屏。
 * 排序只在展示层做，不改接口数据；已领的**照常展示**（所有者要"可记录"）。
 */
function campaignEntriesFor(acc: api.QoderAccountCampaigns): api.QoderCampaign[] {
  const seen = new Set<string>();
  const out: api.QoderCampaign[] = [];
  for (const c of acc.campaigns || []) {
    const key = c.campaignId || c.campaignKey;
    if (key) {
      if (seen.has(key)) continue;
      seen.add(key);
    }
    out.push(c);
  }
  return out.sort((a, b) => Number(isClaimedCampaign(a)) - Number(isClaimedCampaign(b)));
}

/**
 * 把一个账号的权益活动转成**账号卡片上的任务清单**。
 *
 * # 为什么是"推导"而不是"写死一张表"（所有者明确要求）
 *
 * 原话：「qoder 还支持的任务（**因为 qoder 活动是动态的，所以这里支持的
 * 任务也是动态的**）」。
 *
 * 上游的活动是按账号下发、随时上新的（实测 `campaignKey` 形如
 * `act-20260918-628`）。写死一张任务表意味着：上游换个活动，菜单里就少了
 * 那项，而**用户只会看到"任务不见了"** —— 他不会知道是我们要改代码。
 *
 * 故这里直接用活动清单做任务清单：**活动有多少，任务就有多少**。
 *
 * # 三个状态的判定
 *
 *	done    —— 上游明确说已领（`isClaimedCampaign`）
 *	blocked —— 已过期或不是"查看类"动作（`!isClaimableCampaign` 且未领）
 *	ready   —— 其余（可领）
 *
 * ⚠ `blocked` 与 `done` 都置灰但**都显示**：用户需要知道"这个任务存在"。
 * 隐藏会让他以为功能没了（见 ProductAccountTask 的注释）。
 */
function qoderTaskStateOf(c: api.QoderCampaign): "done" | "ready" | "blocked" {
  if (isClaimedCampaign(c)) return "done";
  if (!isClaimableCampaign(c)) return "blocked";
  return "ready";
}

/** 该账号当前有哪些任务、各自什么状态。
 *
 * # ⚠ 只把**真活动**当成任务（所有者 2026-09-20 两次反馈）
 *
 * 原话：「qoder只有一个活动能领取,第二个只是优惠说明」
 * 第二次：「qoder不是说只有一个能领取吗?你这里应该能做出区分吧,
 *           他应该也有字段能区分是否能领取才对」
 *
 * **他说对了 —— 有字段，而且我第一版用错了。**
 *
 * 抓真实数据（`gateway.exe qoder-login campaigns`，2026-09-20）：
 *
 *	[0] act-20260920-044  actionType=CLAIM_BENEFIT  claimStatus=CLAIMED  benefit=有(100 CREDITS)
 *	[1] act-20260901-922  actionType=VIEW_DETAILS   claimStatus=CLAIMED  benefit=**无**
 *
 * 区分字段就是 **`actionType`**：
 *
 *	CLAIM_BENEFIT → 能领真东西（每日 100 Credits）
 *	VIEW_DETAILS  → **只是"查看详情"**，配的是优惠说明文案
 *	                （实测那条：「专业版 4,000 Qwen Credits，高级版 12,000…」）
 *
 * # 我第一版错在哪（值得记下来）
 *
 * 我写的过滤是 `Boolean(c.benefit) || isClaimedCampaign(c)` ——
 * 那个 `|| isClaimedCampaign(c)` 是致命的：**两条都是 CLAIMED**，
 * 于是宣传那条照样被放行，界面上仍然是"两个任务"。
 *
 * 我当时加那个或条件，理由是"用户需要看到自己领过什么"。但那是**错位**的
 * 关心：`VIEW_DETAILS` 本来就不是"领过的活动"，它是**广告位** ——
 * 它上面显示的那个「已领取」是上游给整个活动页的状态，不是"你领到了这个"。
 * 所以它既不该显示为任务，也不该显示为已完成。
 *
 * 判据只看 `actionType === "CLAIM_BENEFIT"`（与后端
 * `is_claimable_campaign` 的取向一致：benefit 或 actionType 二者之一）。
 */
function qoderTasksOf(
  acc: api.QoderAccountCampaigns | undefined,
  failReason?: string,
): ProductAccountTask[] {
  if (!acc) return [];
  return campaignEntriesFor(acc)
    // 只保留"领取类"动作。`VIEW_DETAILS` 是广告位 —— 显示成任务会让用户
    // 以为还有第二个活动可领（那正是他反馈的问题）。
    .filter(isBenefitCampaign)
    .map((c) => {
      const state = qoderTaskStateOf(c);
      // 失败原因只在**可领**（ready）的任务上提示 —— 那才是用户刚点过的那条。
      // done/blocked 的原因另有来源（已领/不可领），不该被覆盖。
      const reason =
        state === "ready" && failReason
          ? failReason
          : state === "blocked"
            ? campaignSubtitleOf(c) || "当前不可领取"
            : undefined;
      return {
        // id 用 campaignId：领取接口要的就是它（campaignKey 只是好看的名字）
        id: c.campaignId || c.campaignKey || "",
        label: campaignTitleOf(c),
        state,
        reason,
      };
    })
    .filter((t) => t.id !== "");
}

/**
 * 账号名口径：**备注 → 昵称 → uid 前缀**。
 *
 * 与 `GatewayPage` 的 `accountLabel` **逐字一致** —— 同一个账号在账号池页叫
 * 「公司号」、在 Qoder 页叫昵称，是所有者明确反馈过的那类不一致。
 *
 * 注：`QoderAccountCampaigns` 里**没有备注**（活动接口只回 uid + 昵称），
 * 故调用方需要从账号列表补一个 `note` 进来，否则备注这一档永远取不到。
 */
function accountLabel(input: { uid: string; nickname?: string; note?: string }): string {
  return input.note?.trim() || input.nickname?.trim() || input.uid.slice(0, 8);
}


export default function QoderPage() {
  const [rows, setRows] = useState<QoderAccountRow[]>([]);
  const [orphans, setOrphans] = useState<string[]>([]);
  const [summary, setSummary] = useState<QoderSummary | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  // 登录对话框
  const [loginOpen, setLoginOpen] = useState(false);
  const [loginRegion, setLoginRegion] = useState<Exclude<QoderRegion, ""> | "">("");
  const [loginUrl, setLoginUrl] = useState<string | null>(null);
  const [loginState, setLoginState] = useState<"idle" | "waiting" | "ok" | "error">("idle");
  const [loginError, setLoginError] = useState<string | null>(null);
  const pollTimer = useRef<number | null>(null);

  // 导入对话框
  const [importOpen, setImportOpen] = useState(false);
  const [importPath, setImportPath] = useState("");
  const [importResult, setImportResult] = useState<api.QoderImportResult | null>(null);

  // 编辑备注
  const [editTarget, setEditTarget] = useState<QoderAccountRow | null>(null);
  const [editNote, setEditNote] = useState("");

/** 正在查看记录的账号（null = 关闭）。 */
const [recordsFor, setRecordsFor] = useState<QoderAccountRow | null>(null);

  // 从客户端一键导入（**主路径**）
  //
  // 网页授权在本机走不通：授权链接的 redirect_uri 是 `qoder-work-cn://`，
  // 一个只有真正的 Qoder 客户端才会注册的自定义协议 —— 我们不是它，
  // 浏览器授权完成后无处回调。
  //
  // 而客户端已经登录了，登录态就在它的数据目录里。读它即可。
  const [clientImporting, setClientImporting] = useState(false);

  // 权益活动（「每天领 100 Credits」那类）
  //
  // ⚠ 按**账号**存放，不是全局一份。
  //
  // 活动是每账号专属的：A 账号领了 100 Credits，B 账号还有 100 没领。
  // 此前只查一个账号（`rows.find(r => r.hasCredential)`）就把结果当全局，
  // 于是**其他账号的活动永远发现不了** —— 所有者的反馈正是这个：
  //
  //	「qoder 那个任务跟 workbuddy 一样都属于每个账号的专属任务,
  //	  每个账号都能领取」
  const [campaigns, setCampaigns] = useState<api.QoderCampaignsAllResult | null>(null);
  const [campaignsLoading, setCampaignsLoading] = useState(false);
  /**
   * **正在领取的账号 uid**（不是"某个活动 id"）。
   *
   * 按账号而不是按活动：领取成功后状态会变（CLAIMABLE → CLAIMED），
   * 而重查是**整账号**级别的 —— 期间该账号的其它活动此时也是"状态待定"的，
   * 按活动置灰会留出"同一账号连点两个活动"的窗口。
   *
   * 同一时刻只允许一个账号在领：并发打多轮上游请求时，"领取后重查"的
   * 结果会互相覆盖（谁后回来谁说了算），界面会闪回旧状态。
   */
  const [claimingUid, setClaimingUid] = useState<string | null>(null);
  /** 一键领取（所有账号）进行中。 */
  const [claimingAll, setClaimingAll] = useState(false);
  /**
   * 本会话**领取失败**的账号 → 原因。
   *
   * 为什么需要它：领取失败时上游的 `claimStatus` **不会变**（仍是可领取），
   * 若界面只读接口状态，用户点完看不到任何变化，会以为"点了没反应"。
   * 这是**会话内**提示，重新查询活动时清空（那时接口状态才是权威）。
   */
  const [claimErrors, setClaimErrors] = useState<Record<string, string>>({});

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const res = await api.qoderListAccounts();
      // 防御性取值：**后端可能比前端旧**（用户升级了界面但没重启网关服务），
      // 此时接口会返回空对象而不是报错 —— 直接读 res.accounts.map 会抛
      // "Cannot read properties of undefined"，整个 React 树崩掉 → 整页白屏。
      //
      // 白屏是最糟的失败形态：用户看不到任何原因，也无从判断是没账号还是坏了。
      // 实测踩到：mock 对未知路由返回 {} 时，本页正是这样白屏的。
      setRows(Array.isArray(res?.accounts) ? res.accounts : []);
      setOrphans(Array.isArray(res?.orphanCredentials) ? res.orphanCredentials : []);
      setSummary(res?.summary ?? null);
      // 结构不对时给出可读提示（而不是静默显示"还没有账号"）
      if (!res || !Array.isArray(res.accounts)) {
        setLoadError(
          "后端返回的数据结构不正确（缺少 accounts 字段）。可能是网关服务版本过旧，请重启或更新后重试。",
        );
      } else {
        setLoadError(null);
      }
    } catch (e) {
      // 明确报错而不是显示空列表：空列表会被误读成"还没有账号"
      setLoadError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // 轮询授权结果；组件卸载或对话框关闭时清理定时器
  const stopPolling = useCallback(() => {
    if (pollTimer.current !== null) {
      window.clearTimeout(pollTimer.current);
      pollTimer.current = null;
    }
  }, []);

  useEffect(() => stopPolling, [stopPolling]);

  const pollOnce = useCallback(
    async (sessionId: string) => {
      try {
        const r = await api.qoderLoginPoll(sessionId);
        if (r.status === "pending") {
          // 用户还没在浏览器确认 —— 继续等
          pollTimer.current = window.setTimeout(() => void pollOnce(sessionId), 2500);
          return;
        }
        stopPolling();
        setLoginState("ok");
        toast.success("Qoder 账号已登录");
        void refresh();
      } catch (e) {
        stopPolling();
        setLoginState("error");
        setLoginError(e instanceof Error ? e.message : String(e));
      }
    },
    [refresh, stopPolling],
  );

  const startLogin = useCallback(async () => {
    if (!loginRegion) {
      toast.error("请先选择区域（国服或国际版）");
      return;
    }
    setBusy("login");
    setLoginError(null);
    setLoginState("idle");
    try {
      const r = await api.qoderLoginStart(loginRegion);
      setLoginUrl(r.authUrl);
      setLoginState("waiting");
      // 在**系统默认浏览器**里打开（不是 WebView 内的新窗口）。
      // 此前用 window.open，在 Tauri 里不会交给系统浏览器 → "点了没反应"。
      // 见 lib/open-browser.ts 的说明。
      try {
        await openInDefaultBrowser(r.authUrl);
      } catch (e) {
        setLoginError(
          `未能自动打开浏览器（${e instanceof Error ? e.message : String(e)}）。` +
            `请点下面的链接手动打开。`,
        );
      }
      pollTimer.current = window.setTimeout(() => void pollOnce(r.sessionId), 2500);
    } catch (e) {
      setLoginState("error");
      setLoginError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  }, [loginRegion, pollOnce]);

  const closeLogin = useCallback(() => {
    stopPolling();
    setLoginOpen(false);
    setLoginUrl(null);
    setLoginState("idle");
    setLoginError(null);
    setLoginRegion("");
  }, [stopPolling]);

  const doImport = useCallback(async () => {
    if (!importPath.trim()) {
      toast.error("请填写凭证文件或目录的路径");
      return;
    }
    setBusy("import");
    try {
      const r = await api.qoderImportCredentials(importPath.trim());
      setImportResult(r);
      if (r.importedCount > 0) {
        toast.success(`已导入 ${r.importedCount} 个账号`);
        void refresh();
      }
      if (r.failedCount > 0) {
        toast.error(`${r.failedCount} 个文件导入失败，详见下方`);
      }
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  }, [importPath, refresh]);

  /**
   * 从客户端一键导入。
   *
   * 这是**主路径**：网页授权在本机走不通（`redirect_uri` 是自定义协议
   * `qoder-work-cn://`，只有真正的 Qoder 客户端才注册它），
   * 而客户端已经登录了 —— 直接读它的登录态即可，用户什么都不用点。
   */
  const doClientImport = useCallback(async () => {
    setClientImporting(true);
    try {
      const r = await api.qoderImportFromClient();
      // ⚠ 导入后额度/套餐/模型是**后端顺手查好**的（见 qoder_login.rs 的
      // import_from_client）。这里如实报告结果：查不到时说清原因，
      // 而不是让用户面对一片「未知」去猜是导入没生效还是账号真没额度
      //（所有者的反馈：「导入后也不自动更新状态,也不自动更新这些信息」
      //  「明明是有套餐容量的」）。
      if (r.enriched === false) {
        toast.success(`已从客户端导入：${r.nickname || r.uid.slice(0, 12)}`, {
          description: `额度/模型没查到：${r.enrichError || "上游未返回数据"}`,
          duration: 8000,
        });
      } else {
        toast.success(`已从客户端导入：${r.nickname || r.uid.slice(0, 12)}（含额度与模型）`);
      }
      void refresh();
    } catch (e) {
      // 错误信息由后端给出**具体原因**（找不到客户端/未登录/解密失败），
      // 不要在这里改写成笼统的"导入失败"—— 那会让用户无从下手。
      toast.error(e instanceof Error ? e.message : String(e), { duration: 8000 });
    } finally {
      setClientImporting(false);
    }
  }, [refresh]);

  /**
   * 刷新单个账号的额度 / 到期时间 / 支持模型。
   *
   * 后端的 `FetchQuota` / `FetchModels` 早已实现，但**没有任何生产者
   * 调用** —— 于是额度恒为 0（界面显示"未知"）、到期恒为空、
   * 看不到支持模型。用户看到的现象就是"查不到额度"。
   *
   * 这里把失败原因原样透出（上游不认 / 账号还没分配额度 / 拿不到模型），
   * 用户需要知道是哪种才知道下一步该做什么。
   */
  const refreshAccount = useCallback(
    async (row: QoderAccountRow) => {
      setBusy(`refresh:${row.uid}`);
      try {
        const r = await api.qoderRefreshAccount(row.uid);
        const parts: string[] = [];
        if (r.quota.error) {
          parts.push(`额度：${r.quota.error}`);
        } else if (r.quota.remaining !== null && r.quota.remaining !== undefined) {
          // 新账号常见 total=0 且 exceeded=true —— 那不是"用超了"，
          // 而是"还没分配额度"。措辞要区分，否则用户以为自己的额度被扣光。
          const zero = (r.quota.total ?? 0) === 0;
          parts.push(
            zero
              ? "额度：该账号尚未分配额度"
              : `额度 ${r.quota.remaining.toLocaleString()}`,
          );
        }
        if (r.modelsError) {
          parts.push(`模型：${r.modelsError}`);
        } else if (r.models.length > 0) {
          parts.push(`模型 ${r.models.length} 个`);
        }
        toast.success(parts.length ? `已刷新：${parts.join("；")}` : "已刷新", { duration: 7000 });
        void refresh();
      } catch (e) {
        toast.error(e instanceof Error ? e.message : String(e));
      } finally {
        setBusy(null);
      }
    },
    [refresh],
  );

  /**
   * 查询**所有账号**的权益活动（**只读**）。
   *
   * # 为什么改成批量（所有者的反馈）
   *
   * 原实现只查**一个**账号：
   *
   *	```text
   *	const target = rows.find((r) => r.hasCredential) ?? rows[0];
   *	const r = await api.qoderCampaigns(target.uid);
   *	setCampaigns(r);
   *	```
   *
   * 当时的理由是"多账号时逐个查会把界面搞复杂"—— 这个取舍**是错的**：
   * 活动是**每账号专属**的，只查 A 会让 B 的活动**永远发现不了**。
   *
   * 所有者的话：「qoder 那个任务跟 workbuddy 一样都属于每个账号的
   * 专属任务,**每个账号都能领取**」。
   */
  const openCampaigns = useCallback(async () => {
    setCampaignsLoading(true);
    // 重新查询 = 以上游状态为准，故清掉会话内的失败/金额提示，
    // 否则用户会一直看到一个上游早已改观的「领取失败」。
    setClaimErrors({});
    try {
      const r = await api.qoderCampaignsAll();
      setCampaigns(r);
      // 查询结果本身就在卡片上（每账号一个状态徽标），**不再弹 toast** ——
      // 每次进页面都弹一句"有 N 个可领取"会变成噪音，而用户一眼就能看到。
    } catch (e) {
      // 查不到活动**不是错误**（可能只是当前没有），故不弹红色错误。
      // 但要把原因说清楚，而不是静默。
      toast.message("暂时查不到权益活动", {
        description: e instanceof Error ? e.message : String(e),
      });
    } finally {
      setCampaignsLoading(false);
    }
  }, []);


  /**
   * 领取**一个账号**名下所有可领的活动。
   *
   * # 所有者的要求：每个账号都能**独立**签到，已签到就不让再签
   *
   *	「按照第一个,然后每个账号都可以独立签到,当然已经签到就不让签到了,
   *	  幂等操作是吧好像是叫」
   *
   * 所以按钮长在**账号卡片**上，而不是按活动逐个点：
   * 一个账号名下常有多个可领活动（实测 POPUP + USAGE 两个展示位），
   * 逐个点既啰嗦、又会在"点完一个、另一个还是可领"时让人以为没生效。
   *
   * # 为什么前端要循环调 `qoderClaimCampaign`
   *
   * 单账号批量接口**不存在**（后端只有 `claim_all_campaigns`＝全部账号，
   * 与 `claim_campaign`＝单活动）。前端循环是这里唯一可行的做法；
   * 而"先查可领清单"的规则仍只在后端（`claim_all_campaigns` 的 targets 过滤）
   * 与前端展示层各写一次，两者的判据都取自上游的 `claimStatus`。
   *
   * # 必须显式传 uid（不能靠"找第一个有凭证的账号"）
   *
   * 活动是每账号专属的。原实现用 `rows.find(r => r.hasCredential)` 猜账号 ——
   * 在卡片按账号展示后，那会把**当前卡片**的活动当成**第一个账号**的去领。
   *
   * # `replayed` 必须区分
   *
   * 上游对"之前已领过"会回 `replayed:true`（而不是报错）。
   * 若一律说"领取成功"，用户会以为又领了一份 —— 必须如实说"已领过"。
   */
  const claimOneAccount = useCallback(
    async (acc: api.QoderAccountCampaigns, onlyCampaignId?: string) => {
      // `onlyCampaignId` 非空 = 从账号卡的任务菜单**只跑选中那一项**；
      // 为空 = 卡片上的「领取全部」按钮，跑该账号所有可领活动。
      //
      // 为什么需要"只跑一项"：所有者要求任务要能**单独执行**
      //（「点击后进行执行 qoder 还支持的任务」）—— 他点的是某个任务，
      // 期望影响的就是那一个，而不是顺带把别的也领了。
      let targets = campaignEntriesFor(acc).filter(isClaimableCampaign);
      if (onlyCampaignId) {
        targets = targets.filter(
          (c) => (c.campaignId || c.campaignKey) === onlyCampaignId,
        );
      }
      if (targets.length === 0) return;

      setClaimingUid(acc.uid);
      // 开领前先清掉上一次的失败标记（否则成功后卡片上仍挂着「领取失败」）
      setClaimErrors((prev) => {
        const next = { ...prev };
        delete next[acc.uid];
        return next;
      });

      let ok = 0;
      let replayed = 0;
      const failed: string[] = [];
      const granted: Array<{ kind: string; amount: number }> = [];

      for (const c of targets) {
        try {
          const r = await api.qoderClaimCampaign(acc.uid, c.campaignId);
          if (r.replayed) {
            replayed += 1;
          } else {
            ok += 1;
          }
          // 金额取**这次领取的活动**自带的 benefit。
          //
          // ⚠ `QoderClaimResult` 里**没有** benefit 字段（后端透传的是上游的
          // 领取响应，它只回 grantId / claimedAt 这类凭据，不含金额），
          // 故金额只能来自活动本身 —— 那同样是上游给的数据，不是编的。
          if (c.benefit && !r.replayed) {
            granted.push({ kind: c.benefit.kind, amount: c.benefit.amount });
          }
        } catch (e) {
          failed.push(c.campaignKey || c.campaignId || "未知活动");
          // 失败原因要给**第一个**就够（多活动失败时逐个列会把 toast 撑爆）
          if (failed.length === 1) {
            setClaimErrors((prev) => ({
              ...prev,
              [acc.uid]: e instanceof Error ? e.message : String(e),
            }));
          }
        }
      }

      const who = accountLabel({
        uid: acc.uid,
        nickname: acc.nickname,
        note: rows.find((r) => r.uid === acc.uid)?.note,
      });
      const parts: string[] = [];
      if (ok > 0) parts.push(`新领到 ${ok} 个`);
      if (replayed > 0) parts.push(`${replayed} 个之前已领过`);
      if (failed.length > 0) parts.push(`失败 ${failed.length} 个`);

      if (ok > 0) {
        toast.success(`已领取：${who}`, {
          description: rewardTextOf(granted) || parts.join("，"),
          duration: 8000,
        });
      } else if (failed.length > 0) {
        toast.error(`领取失败：${who}`, {
          description: failed.join("；") || "上游未说明原因",
          duration: 8000,
        });
      } else if (replayed > 0) {
        // 上游确认是重复请求，没有重复发放 —— 不能说"领取成功"
        toast.message("这些活动之前已经领过了", {
          description: `${who}：上游确认是重复请求，没有重复发放`,
        });
      }

      // 领取后状态会变（CLAIMABLE → CLAIMED），重新拉一次才算数
      await openCampaigns();
      setClaimingUid(null);
    },
    [openCampaigns, rows],
  );

  /**
   * **一键领取所有账号**的可领权益活动。
   *
   * # 所有者的需求
   *
   *	「qoder 那个任务跟 workbuddy 一样都属于每个账号的专属任务,
   *	  每个账号都能领取,可以跟 workbuddy 一样显示一个一键领取(所有账号),
   *	  然后单个账号单独跑」
   *
   * # 为什么放后端而不是前端循环
   *
   * 后端 `claim_all_campaigns` 会**先查每个账号有哪些可领的、再逐个领**，
   * 并把"该账号没活动"与"该活动领失败"区分开。前端循环做不到这个区分，
   * 而且要把每个账号的令牌状态判断重复一遍 —— 那些规则只该有一处实现。
   *
   * # 结果如实汇报
   *
   * 三类**分开**报，不合并成一句"完成"：
   *
   *	新领到 N 个  —— 真的拿到了
   *	失败 M 个    —— 有原因，用户可以针对性处理
   *	无可领 K 个  —— 本来就领完了，不是失败
   *
   * 合并会让"部分失败"被掩盖成"成功"，那是最容易让人误判的报法。
   */
  const claimAll = useCallback(async () => {
    setClaimingAll(true);
    setClaimErrors({});
    try {
      const r = await api.qoderClaimAllCampaigns();
      const parts: string[] = [];
      if (r.claimedCount > 0) parts.push(`新领到 ${r.claimedCount} 个`);
      if (r.failedCount > 0) parts.push(`失败 ${r.failedCount} 个`);
      if (r.nothingCount > 0) parts.push(`${r.nothingCount} 个账号无可领`);

      const failedDetail = (r.accounts || [])
        .flatMap((a) => (a.claimed || []).filter((c) => !c.ok).map((c) => c.error || "未知原因"))
        .slice(0, 3)
        .join("；");

      // 逐账号记账：金额（补进卡片）与失败原因（让卡片能标出「领取失败」）
      const grantedNext: Record<string, string> = {};
      const errorsNext: Record<string, string> = {};
      for (const a of r.accounts || []) {
        const amounts = (a.claimed || [])
          .filter((c) => c.ok && !c.replayed && c.benefit)
          .map((c) => ({ kind: c.benefit!.kind, amount: c.benefit!.amount }));
        const text = rewardTextOf(amounts);
        if (text) grantedNext[a.uid] = text;
        const err = (a.claimed || []).find((c) => !c.ok)?.error;
        if (err) errorsNext[a.uid] = err;
      }
      if (Object.keys(grantedNext).length > 0) {
      }
      if (Object.keys(errorsNext).length > 0) setClaimErrors(errorsNext);

      if (r.claimedCount > 0) {
        toast.success(parts.join("，") || "已处理", {
          description: failedDetail || undefined,
          duration: 8000,
        });
      } else if (r.failedCount > 0) {
        toast.error("没有领到", { description: failedDetail || "全部失败", duration: 8000 });
      } else {
        toast.message("没有可领取的活动", {
          description: "所有账号的活动都已经领过了",
        });
      }
      // 领取后状态会变，重新拉一次才算数
      await openCampaigns();
    } catch (e) {
      toast.error("一键领取失败", {
        description: e instanceof Error ? e.message : String(e),
      });
    } finally {
      setClaimingAll(false);
    }
  }, [openCampaigns]);

  /**
   * 账号列表的「刷新」按钮：**同时**重查账号与权益活动。
   *
   * 为什么把两者合起来：活动区原来的「刷新活动」按钮被删了（所有者要求删掉
   * 大横幅，顶部只留一个「一键领取」）。若只刷新账号列表，用户改完备注/导入
   * 新账号后就再也没有办法主动重查活动 —— 那是把能力删掉了，而不只是删按钮。
   *
   * 合在这里比另加一个按钮更符合语义：活动本来就是账号的附属数据，
   * 「刷新」刷新的是这一页看到的东西。
   */
  const refreshAll = useCallback(async () => {
    await refresh();
    await openCampaigns();
  }, [refresh, openCampaigns]);

  // ⚠ 原先这里有一个每秒 setInterval 驱动倒计时（`now`），
  // 供旧的 `QoderAccountCampaignCard` 显示"还剩多久可领"。
  // 该卡已删（活动并进账号卡片，见 qoderTasksOf），故倒计时一并移除 ——
  // 否则会留一个每秒触发重渲染的定时器，白耗电且让页面持续抖动。

  // 账号列表就绪后**自动查一次**权益活动。
  //
  // 为什么不在页面加载时直接查：那时 rows 还是空的，挑不出账号。
  //
  // 为什么只查一次（`campaignsAutoDone` 标记）：`openCampaigns` 依赖
  // `rows`，而 `rows` 会被 refresh 反复替换 —— 不设标记就会每次刷新
  // 都打一次上游。用户想更新时点账号列表上的「刷新」（见 `refreshAll`）。
  const campaignsAutoDone = useRef(false);
  useEffect(() => {
    if (campaignsAutoDone.current) return;
    if (loading || rows.length === 0) return;
    campaignsAutoDone.current = true;
    void openCampaigns();
  }, [loading, rows, openCampaigns]);

  const toggleDisabled = useCallback(
    async (row: QoderAccountRow) => {
      setBusy(row.uid);
      try {
        await api.qoderSaveAccount(row.uid, { disabled: !row.disabled });
        // 就地更新，避免整页重载造成闪烁
        setRows((prev) =>
          prev.map((r) => (r.uid === row.uid ? { ...r, disabled: !row.disabled } : r)),
        );
        toast.success(row.disabled ? "已恢复接流量" : "已停止接流量");
      } catch (e) {
        toast.error(e instanceof Error ? e.message : String(e));
      } finally {
        setBusy(null);
      }
    },
    [],
  );

  const saveNote = useCallback(async () => {
    if (!editTarget) return;
    setBusy(editTarget.uid);
    try {
      await api.qoderSaveAccount(editTarget.uid, { note: editNote });
      setRows((prev) =>
        prev.map((r) => (r.uid === editTarget.uid ? { ...r, note: editNote } : r)),
      );
      setEditTarget(null);
      toast.success("备注已保存");
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  }, [editNote, editTarget]);

  const remove = useCallback(
    async (row: QoderAccountRow) => {
      const name = row.nickname || row.uid.slice(0, 8);
      if (!window.confirm(`确定删除账号「${name}」吗？\n\n这会同时删除它的凭证文件。`)) return;
      setBusy(row.uid);
      try {
        await api.qoderDeleteAccount(row.uid);
        setRows((prev) => prev.filter((r) => r.uid !== row.uid));
        toast.success("账号已删除");
      } catch (e) {
        toast.error(e instanceof Error ? e.message : String(e));
      } finally {
        setBusy(null);
      }
    },
    [],
  );

  const totals = useMemo(() => {
    const total = rows.length;
    const disabled = rows.filter((r) => r.disabled).length;
    const missing = rows.filter((r) => !r.hasCredential).length;
    const credits = rows.reduce((s, r) => s + (r.credits || 0), 0);
    return { total, disabled, missing, credits };
  }, [rows]);


  return (
    <TooltipProvider delayDuration={200}>
      <div className="w-full space-y-6 px-5 py-6 sm:px-8 sm:py-8">
        <header className="mb-6">
          <div className="flex items-start gap-3">
            <QoderMark size={34} className="mt-0.5" />
            <div className="min-w-0 flex-1">
              <h1 className="text-[28px] font-semibold tracking-tight">Qoder 账号</h1>
              <p className="mt-2 text-sm leading-6 text-muted-foreground">
                管理 Qoder 账号的登录态、额度与到期时间，并与 WorkBuddy 账号一起参与网关路由。
              </p>
            </div>
          </div>
        </header>

        {/* 概览 */}
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
          <Card>
            <CardHeader className="pb-2">
              <CardDescription>账号总数</CardDescription>
              <CardTitle className="text-3xl">{loading ? "—" : totals.total}</CardTitle>
            </CardHeader>
          </Card>
          <Card>
            <CardHeader className="pb-2">
              <CardDescription>参与路由</CardDescription>
              <CardTitle className="text-3xl">{loading ? "—" : totals.total - totals.disabled}</CardTitle>
            </CardHeader>
          </Card>
          <Card>
            <CardHeader className="pb-2">
              <CardDescription>剩余额度合计</CardDescription>
              <CardTitle className="text-3xl">{loading ? "—" : totals.credits.toLocaleString()}</CardTitle>
            </CardHeader>
          </Card>
          <Card>
            <CardHeader className="pb-2">
              <CardDescription>凭证缺失</CardDescription>
              <CardTitle className={cn("text-3xl", totals.missing > 0 && "text-amber-600")}>
                {loading ? "—" : totals.missing}
              </CardTitle>
            </CardHeader>
          </Card>
        </div>

        {/* 凭证缺失提醒：这类账号在网关里已不可用，必须显式告知 */}
        {!loading && totals.missing > 0 && (
          <Alert>
            <AlertTriangle className="h-4 w-4" />
            <AlertTitle>有 {totals.missing} 个账号缺少凭证</AlertTitle>
            <AlertDescription>
              这些账号的凭证文件已不存在（可能被手动删除或从未导入），
              <strong>在网关里已无法使用</strong>。请重新登录或导入凭证。
            </AlertDescription>
          </Alert>
        )}

        {loadError && (
          <Alert variant="destructive">
            <AlertTriangle className="h-4 w-4" />
            <AlertTitle>读取账号失败</AlertTitle>
            <AlertDescription>{loadError}</AlertDescription>
          </Alert>
        )}


        {/* 孤儿凭证：有凭证文件但没登记进账号库 */}
        {!loading && orphans.length > 0 && (
          <Alert>
            <AlertTriangle className="h-4 w-4" />
            <AlertTitle>发现 {orphans.length} 个未登记的凭证</AlertTitle>
            <AlertDescription>
              这些凭证文件存在于磁盘上，但没有对应的账号记录（可能是手动放入的）。
              它们<strong>会被网关使用</strong>，但这里看不到额度与备注。
            </AlertDescription>
          </Alert>
        )}

        {/* 账号列表 */}
        <Card>
          <CardHeader>
            <div className="flex flex-wrap items-center justify-between gap-3">
              <div>
                <CardTitle>账号列表</CardTitle>
                <CardDescription>
                  国服与国际版共用同一套账号管理；区域决定登录与调用使用哪套端点。
                </CardDescription>
              </div>
              <div className="flex flex-wrap gap-2">
                <TooltipProvider delayDuration={400}>
                  {/* 一键领取（所有账号）—— 所有者的需求。
                      只在真有可领的时候才可点，避免用户点了却什么也没发生。 */}
                  <Tooltip>
                    <TooltipTrigger asChild>
                      <span>
                        <Button
                          size="sm"
                          data-slot="qoder-claim-all"
                          className="h-9 gap-1.5 rounded-lg px-3"
                          disabled={
                            claimingAll || campaignsLoading || (campaigns?.claimableTotal ?? 0) === 0
                          }
                          onClick={() => void claimAll()}
                          aria-label={`一键领取所有账号的 ${campaigns?.claimableTotal ?? 0} 个权益活动`}
                        >
                          {claimingAll ? (
                            <Loader2 className="h-4 w-4 animate-spin" />
                          ) : (
                            <Gift className="h-4 w-4" />
                          )}
                          一键领取
                        </Button>
                      </span>
                    </TooltipTrigger>
                    <TooltipContent side="top">
                      {campaigns?.claimableTotal
                        ? `一次领取 ${campaigns.accountsWithClaimable} 个账号上共 ${campaigns.claimableTotal} 个可领活动`
                        : "当前没有可领取的活动（已领的按钮是灰的）"}
                    </TooltipContent>
                  </Tooltip>
                </TooltipProvider>
                {/* 账号与权益活动一起刷新（见 refreshAll 的注释：活动区原来的
                    「刷新活动」按钮已随大横幅删掉，能力并入这里）。 */}
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => void refreshAll()}
                  disabled={loading || campaignsLoading}
                >
                  <RefreshCw
                    className={cn("mr-2 h-4 w-4", (loading || campaignsLoading) && "animate-spin")}
                  />
                  刷新
                </Button>
                {/* 「从客户端导入」是**主按钮** —— 它读 Qoder 客户端已登录的
                    登录态，用户不需要做任何事。

                    而「浏览器登录」在本机**走不通**：授权链接的 redirect_uri
                    是自定义协议 `qoder-work-cn://`，只有真正的 Qoder 客户端
                    才注册它，浏览器授权完成后我们收不到回调。故它降级为
                    次要入口，并在弹窗里说明原因。 */}
                <Button size="sm" onClick={() => void doClientImport()} disabled={clientImporting}>
                  <Download className={cn("mr-2 h-4 w-4", clientImporting && "animate-spin")} />
                  从客户端导入
                </Button>
                <Button variant="outline" size="sm" onClick={() => setImportOpen(true)}>
                  <Upload className="mr-2 h-4 w-4" />
                  导入凭证文件
                </Button>
                <Button variant="outline" size="sm" onClick={() => setLoginOpen(true)}>
                  <Download className="mr-2 h-4 w-4" />
                  登录新账号
                </Button>
              </div>
            </div>
          </CardHeader>
          <CardContent>
            {loading ? (
              <div className="space-y-3">
                <Skeleton className="h-14 w-full" />
                <Skeleton className="h-14 w-full" />
                <Skeleton className="h-14 w-full" />
              </div>
            ) : rows.length === 0 ? (
              <div className="rounded-lg border border-dashed px-6 py-10 text-center">
                <QoderMark size={28} className="mx-auto mb-3 opacity-60" />
                {/* 读取失败时**不显示**"还没有账号" —— 那会把"后端坏了/版本旧"
                    误导成"我自己没加过账号"，用户会去反复点登录而不是查后端。 */}
                {loadError ? (
                  <>
                    <p className="text-sm font-medium">无法读取账号列表</p>
                    <p className="mt-1 text-sm text-muted-foreground">
                      请按上方提示处理后重试；若问题持续，请检查网关服务是否在运行。
                    </p>
                  </>
                ) : (
                  <>
                    <p className="text-sm font-medium">还没有 Qoder 账号</p>
                    <p className="mt-1 text-sm text-muted-foreground">
                      点「登录新账号」用浏览器授权，或「导入凭证」使用已有文件。
                    </p>
                  </>
                )}
              </div>
            ) : (
              /* 卡片布局（对齐 WorkBuddy 与 ZCode 账号页）。
                 所有者的要求：「ZCODE和Qoder和Workbuddy采用一样的卡片布局」。 */
              <ProductAccountGrid>
                {rows.map((row) => {
                  const exp = expiryText(row.expireAt);
                  const creditsText =
                    row.credits > 0
                      ? row.creditsTotal > 0
                        ? `${row.credits.toLocaleString()} / ${row.creditsTotal.toLocaleString()}`
                        : row.credits.toLocaleString()
                      : undefined;
                  return (
                    <ProductAccountCard
                      key={row.uid}
                      mark={(size) => <QoderMark size={size} />}
                      busyKey={
                        busy === `refresh:${row.uid}`
                          ? "refresh"
                          : busy === row.uid
                            ? "toggle"
                            : busy === `delete:${row.uid}`
                              ? "delete"
                              : // 领取进行中复用同一套忙碌渲染。
                                //
                                // ⚠ 这个状态**必须接上**：删掉旧的
                                // `QoderAccountCampaignCard` 后，`claimingUid`
                                // 就只剩"写入"而看不到"读取"了 —— 我第一版
                                // 据此把它当死代码删掉，tsc 立刻报
                                // `Cannot find name 'setClaimingUid'`：
                                // 领取流程**仍在调用**这些 setter。
                                // 死的是那张**渲染它们的卡**，不是这些状态。
                                claimingUid === row.uid
                                ? "claim"
                                : null
                      }
                      data={{
                        uid: row.uid,
                        nickname: row.nickname,
                        note: row.note,
                        avatarUrl: row.avatarUrl,
                        creditsText,
                        // 单位说明：Qoder 的额度单位是 **Credits**。
                        creditsLabel: "剩余 Credits",
                        expiryText: exp.text,
                        expiryUrgent: exp.urgent,
                        models: row.models,
                        hasCredential: row.hasCredential,
                        disabled: row.disabled,
                        variantLabel: regionLabel(row.region),
                        variantKind: regionVariant(row.region),
                        // ── 额度进度条（所有者要求「积分进度条」）──
                        //
                        // ⚠ 传的是**剩余**占比（不是已用）—— 与 WorkBuddy 卡
                        // 的 `remaining / total` 逐字同款。我第一版按"已用占比"
                        // 算，方向反了：用户看到条快满了会以为额度快用完，
                        // 实际那是才用了一点。
                        //
                        // 只在**确实有总量**时给比例：`creditsTotal` 为 0
                        // 表示上游没给容量（如按次计费），此时传 undefined
                        // ⇒ 卡片不画进度条。
                        //
                        // ⚠ 不能把"没有总量"当成 0% —— 那会让用户以为额度
                        // 耗尽（反向误导）。
                        usageRatio:
                          row.creditsTotal > 0
                            ? Math.min(1, Math.max(0, row.credits / row.creditsTotal))
                            : undefined,
                        usageText:
                          row.creditsTotal > 0
                            ? `剩余 ${row.credits.toLocaleString()} / ${row.creditsTotal.toLocaleString()}`
                            : undefined,
                        // 临近到期转橙（同 WorkBuddy：额度的问题是"快过期用不完"）
                        usageWarn: exp.urgent,
                        // ── 任务清单（所有者 2026-09-20：活动并进账号卡）──
                        //
                        // 原话：「qoder这个活动卡片和账号卡片应该合到一起，
                        // 应该是 这个账号还有多少任务没运行…而且也没有跟
                        // workbuddy 有个菜单按钮，点击后进行执行 qoder 还支持的任务
                        //（因为 qoder 活动是动态的，所以这里支持的任务也是动态的）」
                        //
                        // 故任务清单**从活动数据实时推导**（不是写死的列表）：
                        // 上游下发哪些活动，菜单里就有哪些任务。
                        //
                        // ⚠ 放在 `data` 里而不是顶层 prop：`tasks` 是**这个账号的
                        // 数据**（它有几个任务、各什么状态），与 uid/nickname 同类。
                        // 我第一版放成顶层 prop，被 tsc 挡住 —— 类型检查帮了忙。
                        tasks: qoderTasksOf(
                          (campaigns?.accounts || []).find((a) => a.uid === row.uid),
                          // 领取失败原因接进任务清单的 `reason`。
                          //
                          // ⚠ 必须接：否则「点了领取但失败」在界面上毫无痕迹 ——
                          // 上游的 claimStatus 失败时**不会变**（仍是可领取），
                          // 用户点完看不到任何变化，会以为按钮坏了。
                          // 这也是 `claimErrors` 存在的理由（见其声明处的注释）。
                          claimErrors[row.uid],
                        ),
                      }}
                      onRefresh={() => void refreshAccount(row)}
                      onViewRecords={() => setRecordsFor(row)}
                      onEditNote={() => {
                        setEditTarget(row);
                        setEditNote(row.note);
                      }}
                      onRunTask={(taskId) => {
                        // 目前 qoder 的可执行任务就是"领取某个权益活动"，
                        // 故任务 id 即活动 id。
                        const acc = (campaigns?.accounts || []).find((a) => a.uid === row.uid);
                        if (acc) void claimOneAccount(acc, taskId);
                      }}
                      onToggleDisabled={() => void toggleDisabled(row)}
                      onDelete={() => void remove(row)}
                    />
                  );
                })}
              </ProductAccountGrid>
            )}
          </CardContent>
        </Card>

        {/* 存储位置（排障用） */}
        {summary && (
          <Card>
            <CardHeader className="pb-3">
              <CardDescription>存储位置</CardDescription>
            </CardHeader>
            <CardContent className="space-y-2 text-xs text-muted-foreground">
              <div className="flex items-center gap-2">
                <span className="w-16 shrink-0">账号库</span>
                <code className="break-all font-mono">{summary.storeDir}</code>
              </div>
              <div className="flex items-center gap-2">
                <span className="w-16 shrink-0">凭证目录</span>
                <code className="break-all font-mono">{summary.authDir}</code>
              </div>
            </CardContent>
          </Card>
        )}

        {/* 登录对话框 */}
        <Dialog open={loginOpen} onOpenChange={(o) => (o ? setLoginOpen(true) : closeLogin())}>
          <DialogContent className="sm:max-w-lg">
            <DialogHeader>
              <DialogTitle>登录 Qoder 账号</DialogTitle>
              <DialogDescription>
                选择区域后会在浏览器打开授权页，完成授权即可自动添加账号。
              </DialogDescription>
            </DialogHeader>

            <div className="space-y-4 py-2">
              {/* ⚠ 先说清楚这条路在**本机走不通**，并把用户引向能用的那条。
                  这是实测结论，不是猜测：
                    授权链接的 redirect_uri = `qoder-work-cn://`
                    那是一个自定义协议，只有真正的 Qoder 客户端才注册它；
                    我们不是它，浏览器授权完成后**收不到回调**。
                  （我们的轮询端点是好的 —— GET 回 401「User not
                   authenticated」正是"还没授权"的预期响应。） */}
              <Alert>
                <AlertTitle>推荐改用「从客户端导入」</AlertTitle>
                <AlertDescription className="space-y-1">
                  <p>
                    这条浏览器授权在本机**无法完成**：它要求系统注册
                    <code className="mx-1 rounded bg-muted px-1 font-mono text-[11px]">
                      qoder-work-cn://
                    </code>
                    协议来回调结果，而那个协议只有 Qoder 客户端才会注册。
                  </p>
                  <p>
                    若你已在 Qoder 客户端里登录过，直接用账号列表上的
                    <span className="mx-1 font-medium">「从客户端导入」</span>
                    即可 —— 一步到位，不需要任何浏览器操作。
                  </p>
                </AlertDescription>
              </Alert>

              <div className="space-y-2">
                <Label>区域</Label>
                {/* 不给默认值：两区授权页与端点都不同，选错会登录到另一个区 */}
                <div className="grid grid-cols-2 gap-2">
                  {(["cn", "intl"] as const).map((r) => (
                    <Button
                      key={r}
                      type="button"
                      variant={loginRegion === r ? "default" : "outline"}
                      onClick={() => setLoginRegion(r)}
                      disabled={loginState === "waiting"}
                      className="justify-start"
                    >
                      <Globe className="mr-2 h-4 w-4" />
                      {regionLabel(r)}
                      <span className="ml-2 text-xs opacity-70">
                        {r === "cn" ? "qoder.com.cn" : "qoder.com"}
                      </span>
                    </Button>
                  ))}
                </div>
                <p className="text-xs text-muted-foreground">
                  两区的授权页与接口地址不同，请按你的账号所在区域选择。
                </p>
              </div>

              {loginUrl && (
                <>
                  <Separator />
                  <div className="space-y-2">
                    <Label>授权链接</Label>
                    <div className="flex gap-2">
                      <Input readOnly value={loginUrl} className="font-mono text-xs" />
                      <Button
                        variant="outline"
                        size="icon"
                        onClick={() => {
                          void navigator.clipboard.writeText(loginUrl);
                          toast.success("已复制授权链接");
                        }}
                      >
                        <ExternalLink className="h-4 w-4" />
                      </Button>
                    </div>
                    <p className="text-xs text-muted-foreground">
                      若浏览器没有自动打开，请手动复制上面的链接访问。
                    </p>
                  </div>
                </>
              )}

              {loginState === "waiting" && (
                <div className="flex items-center gap-2 rounded-md border bg-muted/40 px-3 py-2 text-sm">
                  <Loader2 className="h-4 w-4 animate-spin" />
                  等待你在浏览器中完成授权…
                </div>
              )}
              {loginState === "ok" && (
                <div className="flex items-center gap-2 rounded-md border border-emerald-500/40 bg-emerald-500/10 px-3 py-2 text-sm">
                  <CheckCircle2 className="h-4 w-4 text-emerald-600" />
                  授权成功，账号已添加。
                </div>
              )}
              {loginError && (
                <Alert variant="destructive">
                  <AlertTriangle className="h-4 w-4" />
                  <AlertDescription>{loginError}</AlertDescription>
                </Alert>
              )}
            </div>

            <DialogFooter>
              <Button variant="outline" onClick={closeLogin}>
                {loginState === "ok" ? "关闭" : "取消"}
              </Button>
              {loginState !== "ok" && (
                <Button onClick={() => void startLogin()} disabled={busy === "login" || !loginRegion}>
                  {busy === "login" && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
                  {loginUrl ? "重新发起" : "打开授权页"}
                </Button>
              )}
            </DialogFooter>
          </DialogContent>
        </Dialog>

        {/* 导入对话框 */}
        <Dialog open={importOpen} onOpenChange={setImportOpen}>
          <DialogContent className="sm:max-w-lg">
            <DialogHeader>
              <DialogTitle>导入已有凭证</DialogTitle>
              <DialogDescription>
                填写凭证文件或所在目录的路径。目录会扫描其中的 qoder*.json 文件。
              </DialogDescription>
            </DialogHeader>

            <div className="space-y-4 py-2">
              <div className="space-y-2">
                <Label htmlFor="qoder-import-path">路径</Label>
                <Input
                  id="qoder-import-path"
                  placeholder="C:\Users\你\qoder-auths"
                  value={importPath}
                  onChange={(e) => setImportPath(e.target.value)}
                />
                <p className="text-xs text-muted-foreground">
                  支持嵌套形与扁平形两种凭证格式；区域会按凭证里的域名自动判断。
                </p>
              </div>

              {importResult && (
                <>
                  <Separator />
                  {importResult.imported.length > 0 && (
                    <div className="space-y-1">
                      <div className="text-sm font-medium text-emerald-600">
                        成功 {importResult.importedCount} 个
                      </div>
                      {importResult.imported.map((x) => (
                        <div key={x.file} className="font-mono text-xs text-muted-foreground">
                          {x.file} → {x.uid.slice(0, 12)}（{regionLabel(x.region)}）
                        </div>
                      ))}
                    </div>
                  )}
                  {importResult.failed.length > 0 && (
                    <div className="space-y-1">
                      <div className="text-sm font-medium text-destructive">
                        失败 {importResult.failedCount} 个
                      </div>
                      {importResult.failed.map((x) => (
                        <div key={x.file} className="text-xs text-muted-foreground">
                          <span className="font-mono">{x.file}</span>：{x.error}
                        </div>
                      ))}
                    </div>
                  )}
                </>
              )}
            </div>

            <DialogFooter>
              <Button variant="outline" onClick={() => setImportOpen(false)}>
                关闭
              </Button>
              <Button onClick={() => void doImport()} disabled={busy === "import"}>
                {busy === "import" && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
                开始导入
              </Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>

        {/* 编辑备注 */}
        <Dialog open={editTarget !== null} onOpenChange={(o) => !o && setEditTarget(null)}>
          <DialogContent className="sm:max-w-md">
            <DialogHeader>
              <DialogTitle>编辑备注</DialogTitle>
              <DialogDescription>
                {editTarget?.nickname || editTarget?.uid.slice(0, 12)}
              </DialogDescription>
            </DialogHeader>
            <div className="space-y-2 py-2">
              <Label htmlFor="qoder-note">备注</Label>
              <Input
                id="qoder-note"
                value={editNote}
                onChange={(e) => setEditNote(e.target.value)}
                placeholder="例如：公司号"
              />
            </div>
            <DialogFooter>
              <Button variant="outline" onClick={() => setEditTarget(null)}>
                取消
              </Button>
              <Button onClick={() => void saveNote()} disabled={busy === editTarget?.uid}>
                保存
              </Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>


      {/* 记录弹窗：任务执行记录 + 额度消耗明细（所有者 2026-09-20 要求）。

          账号标识用**本页的 uid**（`row.uid`），与后端写记录时的
          accountId 口径一致（见 qoder_record_identity 的说明：
          Qoder 账号不在宿主账号库里，故用 Qoder 自己的 uid 作 accountId）。

          日期范围由组件自己管（默认今天，可切近 7/30 天）—— 与 WorkBuddy
          的记录入口完全同一份交互，不在这里另造一套。 */}
      <Dialog open={!!recordsFor} onOpenChange={(o) => !o && setRecordsFor(null)}>
        <DialogContent className="sm:max-w-4xl">
          <DialogHeader>
            <DialogTitle>账号记录</DialogTitle>
            <DialogDescription>
              {recordsFor ? recordsFor.note || recordsFor.nickname || recordsFor.uid : ""}
              的任务执行、额度消耗与领取记录；可按日期区间筛选。
            </DialogDescription>
          </DialogHeader>
          <div className="min-h-0 max-h-[70vh] overflow-y-auto pr-1">
            <AccountRecordsView
              accounts={[]}
              fixedAccountId={recordsFor?.uid}
              compact
            />
          </div>
        </DialogContent>
      </Dialog>
      </div>
    </TooltipProvider>
  );
}
