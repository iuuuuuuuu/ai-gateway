import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { toast } from "sonner";
import {
  AlertTriangle,
  CheckCircle2,
  Clock3,
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
import { openInDefaultBrowser } from "@/lib/open-browser";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
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
import { TooltipProvider } from "@/components/ui/tooltip";
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

/**
 * 把剩余毫秒格式化成倒计时（`22:25:46`）。
 *
 * 用于权益活动的"还剩多久可领" —— 活动是**限时**的，用户需要看到
 * 时间在走才会去领。超过 24 小时时显示天数 + 时分，否则显示时分秒。
 *
 * 已过期时返回"已结束"而不是负数（负的倒计时会让人困惑）。
 */
function formatCountdown(msLeft: number): string {
  if (msLeft <= 0) return "已结束";
  const total = Math.floor(msLeft / 1000);
  const d = Math.floor(total / 86400);
  const h = Math.floor((total % 86400) / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  const p2 = (n: number) => String(n).padStart(2, "0");
  if (d > 0) return `${d} 天 ${p2(h)}:${p2(m)}:${p2(s)}`;
  return `${p2(h)}:${p2(m)}:${p2(s)}`;
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
  /** 正在领取的活动 id（用于置灰与转圈）。 */
  const [claimingId, setClaimingId] = useState<string | null>(null);
  /** 一键领取进行中（所有账号）。 */
  const [claimingAll, setClaimingAll] = useState(false);
  // 倒计时用的"now"：每秒更新一次，让剩余时间真的在走。
  const [now, setNow] = useState(() => Date.now());

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
    try {
      const r = await api.qoderCampaignsAll();
      setCampaigns(r);
      if (r.claimableTotal > 0) {
        toast.success(`有 ${r.claimableTotal} 个可领取的权益活动`, {
          description: `分布在 ${r.accountsWithClaimable} 个账号上，可一键领取`,
          duration: 8000,
        });
      }
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

  /** 在系统默认浏览器里打开活动页（领取要用户自己点，含人机验证）。 */
  const openCampaignPage = useCallback(async (url: string) => {
    try {
      await openInDefaultBrowser(url);
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e));
    }
  }, []);

  /**
   * 领取**某个账号**的某个活动。
   *
   * ## 必须显式传 uid（不能靠"找第一个有凭证的账号"）
   *
   * 活动是每账号专属的。原实现用 `rows.find(r => r.hasCredential)` 猜账号 ——
   * 在活动卡片按账号分组展示后，那会把**当前账号**的活动当成**第一个账号**的去领，
   * 领错对象（或直接被上游拒）。
   *
   * ## 只能由用户点击触发
   *
   * 领取本身是安全的（用用户自己的令牌打官方接口，与官方客户端点那个
   * 「领取」按钮同构，**不需要人机验证**）。但**不做定时自动领取** ——
   * 那与"用户点一下"不是一回事，且会让账号表现出非人类的活动模式。
   *
   * ## `replayed` 必须区分
   *
   * 上游对"之前已领过"会回 `replayed:true`（而不是报错）。
   * 若一律说"领取成功"，用户会以为又领了一份 —— 必须如实说"已领过"。
   */
  const claimOne = useCallback(
    async (uid: string, c: api.QoderCampaign) => {
      setClaimingId(c.campaignId);
      try {
        const r = await api.qoderClaimCampaign(uid, c.campaignId);
        if (r.replayed) {
          toast.message("这个活动之前已经领过了", {
            description: "上游确认是重复请求，没有重复发放（领取时间：" +
              (r.claimedAt ? new Date(r.claimedAt).toLocaleString() : "未知") + "）",
          });
        } else {
          const amt = c.benefit ? `${c.benefit.amount} ${c.benefit.kind === "CREDITS" ? "Credits" : c.benefit.kind}` : "奖励";
          toast.success(`已领取 ${amt}`, {
            description: c.benefit?.validity ? `有效期 ${c.benefit.validity.days} 天` : undefined,
          });
        }
        // 领取后状态会变（CLAIMABLE → CLAIMED），重新拉一次才算数
        await openCampaigns();
      } catch (e) {
        toast.error("领取失败", {
          description: e instanceof Error ? e.message : String(e),
        });
      } finally {
        setClaimingId(null);
      }
    },
    [openCampaigns],
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

  // 倒计时每秒走一格
  useEffect(() => {
    const t = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(t);
  }, []);

  // 账号列表就绪后**自动查一次**权益活动。
  //
  // 为什么不在页面加载时直接查：那时 rows 还是空的，挑不出账号。
  //
  // 为什么只查一次（`campaignsAutoDone` 标记）：`openCampaigns` 依赖
  // `rows`，而 `rows` 会被 refresh 反复替换 —— 不设标记就会每次刷新
  // 都打一次上游。用户想更新时点「刷新活动」。
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

        {/* ── 权益活动（「每天领 100 Credits」那类）──
            活动是**限时**的（实测那条只差 22 小时），且每天重置 ——
            用户不知道就白白错过。故用醒目的卡片 + 实时倒计时。

            可以直接在这里领取：领取接口**不需要人机验证**
            （所有者实测确认），用的是用户自己的令牌打官方接口，
            与官方客户端点那个「领取」按钮完全同构。 */}
        {campaigns && campaigns.accounts && campaigns.accounts.length > 0 && (
          <Card
            data-slot="qoder-campaigns"
            className={cn(
              campaigns.claimableTotal > 0 && "border-emerald-500/40 bg-emerald-50/20 dark:bg-emerald-950/10",
            )}
          >
            <CardHeader>
              <div className="flex flex-wrap items-center justify-between gap-2">
                <div>
                  <CardTitle className="flex items-center gap-2">
                    <Gift className="h-4 w-4 text-emerald-600 dark:text-emerald-400" />
                    权益活动
                    {campaigns.claimableTotal > 0 && (
                      <Badge className="border-emerald-500/30 bg-emerald-50 text-[10px] text-emerald-700 dark:bg-emerald-950/40 dark:text-emerald-300">
                        {campaigns.claimableTotal} 个可领取
                      </Badge>
                    )}
                  </CardTitle>
                  <CardDescription>
                    {/* ⚠ 必须说清"每账号专属" —— 这是所有者反馈的核心 */}
                    活动是**每个账号各自**的（A 领了，B 仍可领）。可一键领取所有账号，
                    也可在下面单独领某一个账号的。
                  </CardDescription>
                </div>
                <div className="flex gap-2">
                  <Button variant="outline" size="sm" onClick={() => void openCampaigns()}>
                    <RefreshCw className={cn("mr-2 h-4 w-4", campaignsLoading && "animate-spin")} />
                    刷新活动
                  </Button>
                  {/* 一键领取（所有账号）—— 所有者的需求。
                      只在真有可领的时候才可点，避免用户点了却什么也没发生。 */}
                  <Button
                    size="sm"
                    data-slot="qoder-claim-all"
                    disabled={claimingAll || campaigns.claimableTotal === 0}
                    onClick={() => void claimAll()}
                    aria-label={`一键领取所有账号的 ${campaigns.claimableTotal} 个权益活动`}
                  >
                    {claimingAll ? (
                      <Loader2 className="mr-2 h-4 w-4 animate-spin" />
                    ) : (
                      <Gift className="mr-2 h-4 w-4" />
                    )}
                    一键领取（所有账号）
                  </Button>
                  {campaigns.accounts.find((a) => a.campaignUrl)?.campaignUrl && (
                    <Button
                      variant="outline"
                      size="sm"
                      onClick={() =>
                        void openCampaignPage(campaigns.accounts.find((a) => a.campaignUrl)!.campaignUrl!)
                      }
                    >
                      <ExternalLink className="mr-2 h-4 w-4" />
                      打开活动页
                    </Button>
                  )}
                </div>
              </div>
            </CardHeader>
            <CardContent className="space-y-4">
              {/* 按**账号**分组渲染 —— 每个账号一段，各带自己的领取按钮。
                  这样"A 已领、B 还能领"一眼可见，不会互相盖住。 */}
              {campaigns.accounts.map((acc) => (
                <div
                  key={acc.uid}
                  data-slot="qoder-campaign-account"
                  data-uid={acc.uid}
                  data-claimable={acc.claimable}
                  className="space-y-2"
                >
                  <div className="flex flex-wrap items-center gap-2">
                    <span className="text-xs font-medium">
                      {acc.nickname || acc.uid.slice(0, 12)}
                    </span>
                    {acc.status === "error" ? (
                      <Badge variant="destructive" className="text-[10px]">
                        查询失败
                      </Badge>
                    ) : acc.claimable > 0 ? (
                      <Badge className="border-emerald-500/30 bg-emerald-50 text-[10px] text-emerald-700 dark:bg-emerald-950/40 dark:text-emerald-300">
                        {acc.claimable} 个可领取
                      </Badge>
                    ) : (
                      <Badge variant="secondary" className="text-[10px]">
                        已领完
                      </Badge>
                    )}
                  </div>

                  {acc.status === "error" ? (
                    // 单个账号查失败要说清原因 —— 否则用户会以为"这个账号没活动"
                    // （事实上是查不到，两者完全不同）
                    <div className="rounded-lg border border-destructive/30 bg-destructive/5 px-3 py-2 text-xs text-muted-foreground">
                      {acc.message || "查询失败"}
                    </div>
                  ) : (
                    <div className="space-y-2">
                      {(acc.campaigns || []).map((c) => {
                        const claimable = c.claimStatus !== "CLAIMED" && c.claimStatus !== "EXPIRED";
                        const left = c.endAt > 1e9 ? c.endAt * 1000 - now : 0;
                        return (
                          <div
                            key={c.campaignId || c.campaignKey}
                            data-slot="qoder-campaign-item"
                            data-claim-status={c.claimStatus}
                            className="flex flex-wrap items-center justify-between gap-2 rounded-lg border border-border/70 px-3 py-2"
                          >
                            <div className="min-w-0">
                              <div className="text-sm font-medium">
                                {c.benefit
                                  ? `领 ${c.benefit.amount} ${c.benefit.kind === "CREDITS" ? "Credits" : c.benefit.kind}`
                                  : c.actionType === "VIEW_DETAILS"
                                    ? "活动详情"
                                    : c.campaignKey}
                              </div>
                              <div className="text-xs text-muted-foreground">
                                {c.benefit?.validity
                                  ? `领取后 ${c.benefit.validity.days} 天内有效`
                                  : "限时活动"}
                              </div>
                            </div>
                            <div className="flex items-center gap-2">
                              {claimable ? (
                                <>
                                  <Badge variant="success" className="gap-1">
                                    <Clock3 className="h-3 w-3" />
                                    {formatCountdown(left)}
                                  </Badge>
                                  {/* 只有 actionType=CLAIM_BENEFIT 才能领，
                                      VIEW_DETAILS 那种没有可领的东西。 */}
                                  {c.actionType === "CLAIM_BENEFIT" && (
                                    <Button
                                      size="sm"
                                      disabled={claimingId === c.campaignId}
                                      onClick={() => void claimOne(acc.uid, c)}
                                      aria-label={`为账号 ${acc.nickname || acc.uid} 领取：${c.campaignKey}`}
                                    >
                                      {claimingId === c.campaignId ? (
                                        <Loader2 className="mr-1.5 h-3.5 w-3.5 animate-spin" />
                                      ) : null}
                                      领取
                                    </Button>
                                  )}
                                </>
                              ) : (
                                <Badge variant="secondary">
                                  {c.claimStatus === "CLAIMED" ? "已领取" : "不可领取"}
                                </Badge>
                              )}
                            </div>
                          </div>
                        );
                      })}
                      {(acc.campaigns || []).length === 0 && (
                        <div className="rounded-lg border border-border/70 px-3 py-2 text-xs text-muted-foreground">
                          该账号暂无活动
                        </div>
                      )}
                    </div>
                  )}
                </div>
              ))}
            </CardContent>
          </Card>
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
                <Button variant="outline" size="sm" onClick={() => void refresh()} disabled={loading}>
                  <RefreshCw className={cn("mr-2 h-4 w-4", loading && "animate-spin")} />
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
                              : null
                      }
                      data={{
                        uid: row.uid,
                        nickname: row.nickname,
                        note: row.note,
                        avatarUrl: row.avatarUrl,
                        creditsText,
                        expiryText: exp.text,
                        expiryUrgent: exp.urgent,
                        models: row.models,
                        hasCredential: row.hasCredential,
                        disabled: row.disabled,
                        variantLabel: regionLabel(row.region),
                        variantKind: regionVariant(row.region),
                      }}
                      onRefresh={() => void refreshAccount(row)}
                      onEditNote={() => {
                        setEditTarget(row);
                        setEditNote(row.note);
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
      </div>
    </TooltipProvider>
  );
}
