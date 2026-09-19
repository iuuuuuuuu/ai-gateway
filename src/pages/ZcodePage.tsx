import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { toast } from "sonner";
import {
  AlertTriangle,
  CheckCircle2,
  ExternalLink,
  Globe,
  KeyRound,
  Loader2,
  RefreshCw,
  Search,
  Upload,
} from "lucide-react";
import { ZcodeMark } from "@/components/product-marks";
import { ProductAccountCard, ProductAccountGrid } from "@/components/product-account-card";
import { openInDefaultBrowser } from "@/lib/open-browser";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Checkbox } from "@/components/ui/checkbox";
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
import type { ZcodeAccountRow, ZcodeProvider, ZcodeSummary } from "@/lib/api";
import { cn } from "@/lib/utils";

/**
 * ZCode 账号页（Z5）。
 *
 * ## 与 Qoder 页的关键差异
 *
 * 1. **服务商而非区域**：Z.AI 与智谱是两家不同公司（不同域名、不同凭证来源），
 *    不是同一产品的两个区域。故选择器标签是「服务商」。
 *
 * 2. **导入凭证是主路径**：ZCode 的凭证是用户能从控制台复制的字符串
 *    （`{apiKey}.{secret}`），所以「粘贴凭证」放在最显眼位置；
 *    OAuth 设备流作为备选（因为部分用户可能更习惯浏览器授权）。
 *
 * 3. **额度可能是"未知"而不是 0**：额度查询需要 OAuth 换来的 JWT。
 *    只导入了凭证的账号查不到额度 —— 显示 0 会被用户误读成"额度耗尽"，
 *    故显式显示"未知"并说明原因。
 */

/** 服务商标签。空串是"未指定"，不是 Z.AI —— 见 api.ts 的 ZcodeProvider。 */
function providerLabel(provider: ZcodeProvider): string {
  if (provider === "zai") return "Z.AI";
  if (provider === "bigmodel") return "智谱";
  return "未指定";
}

/** 服务商徽章样式：两家用不同颜色，一眼能分辨。 */
function providerVariant(provider: ZcodeProvider): "default" | "secondary" | "outline" {
  if (provider === "zai") return "default";
  if (provider === "bigmodel") return "secondary";
  return "outline";
}

/** 到期时间展示。 */
function expiryText(expireAt: number): { text: string; urgent: boolean } {
  if (!expireAt) return { text: "未知", urgent: false };
  const ms = expireAt > 1e12 ? expireAt : expireAt * 1000;
  const days = Math.floor((ms - Date.now()) / 86400000);
  const date = new Date(ms).toLocaleDateString();
  if (days < 0) return { text: `已过期（${date}）`, urgent: true };
  if (days <= 7) return { text: `${days} 天后（${date}）`, urgent: true };
  return { text: date, urgent: false };
}

/**
 * 体验套餐的徽章文案。
 *
 * ⚠ 只在**判出来了**才显示。`planKind` 为 `unknown` 时返回空 ——
 * 猜一个会给长期订阅的用户假的"体验套餐"标签。
 *
 * 判据（Go 侧 `plan.go`）：拿上游静态配置 `startPlanPreview` 的赠送量
 * 与实际总量比对，**不硬编码数字**。
 */
function planBadgeOf(row: ZcodeAccountRow): string | undefined {
  if (row.planKind === "trial") return "体验套餐";
  // 付费套餐不额外标注：界面上"有额度"本身就说明是付费的，
  // 多一个标签反而噪声。
  return undefined;
}

/**
 * **套餐整体**到期的说明。
 *
 * ⚠ 与 `expiryText`（各模型桶的**每日周期**结束）不是一回事：
 *
 *     expiryText            今天 23:59:59 → 明天重置，额度回来了
 *     本函数                9/23 23:59:59 → 套餐结束，额度**归零**
 *
 * 只显示前者，用户会以为"明天额度就没了"；只显示后者，他会以为
 * "今天用不完就浪费了"。所以两个都要显示，且文案要各自说清。
 *
 * 只在**体验套餐**上显示：付费套餐的 `endsAt` 是订阅周期，
 * 显示出来会让人误以为"到期就没了"（实际会自动续费）。
 */
function planExpiryTextOf(row: ZcodeAccountRow, now: number): string | undefined {
  if (row.planKind !== "trial") return undefined;
  const at = row.planExpireAt ?? 0;
  if (!at) return undefined;
  const ms = at > 1e12 ? at : at * 1000;
  const days = Math.ceil((ms - now) / 86400000);
  const date = new Date(ms);
  const stamp = `${String(date.getMonth() + 1).padStart(2, "0")}-${String(date.getDate()).padStart(2, "0")} ${String(date.getHours()).padStart(2, "0")}:${String(date.getMinutes()).padStart(2, "0")}`;
  if (ms <= now) return `体验套餐已于 ${stamp} 到期`;
  return `体验额度 ${stamp} 到期（剩 ${days} 天），到期后未用完的会失效`;
}

export default function ZcodePage() {
  const [rows, setRows] = useState<ZcodeAccountRow[]>([]);
  const [orphans, setOrphans] = useState<string[]>([]);
  const [summary, setSummary] = useState<ZcodeSummary | null>(null);
  // 用于体验套餐的剩余天数显示 —— 不每秒更新（那是"天"级信息，
  // 每秒渲染纯属浪费）。每分钟一次足够。
  const [now, setNow] = useState(() => Date.now());
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  // 粘贴凭证对话框（**主路径**）
  const [pasteOpen, setPasteOpen] = useState(false);
  const [pasteCredential, setPasteCredential] = useState("");
  const [pasteProvider, setPasteProvider] = useState<Exclude<ZcodeProvider, "">>("zai");
  const [pasteNickname, setPasteNickname] = useState("");

  // OAuth 登录对话框（备选路径）
  const [loginOpen, setLoginOpen] = useState(false);
  const [loginProvider, setLoginProvider] = useState<Exclude<ZcodeProvider, ""> | "">("");
  const [loginUrl, setLoginUrl] = useState<string | null>(null);
  const [loginState, setLoginState] = useState<"idle" | "waiting" | "ok" | "error">("idle");
  const [loginError, setLoginError] = useState<string | null>(null);
  const pollTimer = useRef<number | null>(null);

  // 目录导入
  const [dirOpen, setDirOpen] = useState(false);
  const [dirPath, setDirPath] = useState("");
  const [dirResult, setDirResult] = useState<api.ZcodeImportDirResult | null>(null);

  // 编辑备注
  const [editTarget, setEditTarget] = useState<ZcodeAccountRow | null>(null);
  const [editNote, setEditNote] = useState("");

  // 扫描本机凭证
  //
  // ZCode 官方客户端把 apiKey 明文落在 ~/.zcode/v2/config.json，
  // 我们能直接读 —— 用户不必手工去控制台复制粘贴。
  const [scanOpen, setScanOpen] = useState(false);
  const [scanning, setScanning] = useState(false);
  const [scanResult, setScanResult] = useState<api.ZcodeScanResult | null>(null);
  const [scanError, setScanError] = useState<string | null>(null);
  /** 已勾选的项（按 index）。默认全选**可导入且未导入**的。 */
  const [scanPicked, setScanPicked] = useState<Set<number>>(new Set());
  const [scanImporting, setScanImporting] = useState(false);
  const [scanImported, setScanImported] = useState<api.ZcodeImportScannedResult | null>(null);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const res = await api.zcodeListAccounts();
      // 防御性取值：**后端可能比前端旧**（升级了界面但没重启服务），
      // 此时接口返回空对象而不是报错 —— 直接读 res.accounts.map 会抛异常、
      // 整个 React 树崩掉 → 整页白屏。白屏是最糟的失败形态。
      setRows(Array.isArray(res?.accounts) ? res.accounts : []);
      setOrphans(Array.isArray(res?.orphanCredentials) ? res.orphanCredentials : []);
      setSummary(res?.summary ?? null);
      if (!res || !Array.isArray(res.accounts)) {
        setLoadError(
          "后端返回的数据结构不正确（缺少 accounts 字段）。可能是网关服务版本过旧，请重启或更新后重试。",
        );
      } else {
        setLoadError(null);
      }
    } catch (e) {
      setLoadError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // 让"体验额度还剩 N 天"随时间推进。
  // 每分钟一次足够（那是天级信息），不必每秒渲染。
  useEffect(() => {
    const t = window.setInterval(() => setNow(Date.now()), 60_000);
    return () => window.clearInterval(t);
  }, []);

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
        const r = await api.zcodeLoginPoll(sessionId);
        if (r.status === "pending") {
          pollTimer.current = window.setTimeout(() => void pollOnce(sessionId), 2500);
          return;
        }
        stopPolling();
        setLoginState("ok");
        toast.success("ZCode 账号已登录");
        void refresh();
      } catch (e) {
        stopPolling();
        setLoginState("error");
        setLoginError(e instanceof Error ? e.message : String(e));
      }
    },
    [refresh, stopPolling],
  );

  const doPaste = useCallback(async () => {
    const cred = pasteCredential.trim();
    if (!cred) {
      toast.error("请粘贴凭证（形如 apiKey.secret）");
      return;
    }
    setBusy("paste");
    try {
      const r = await api.zcodeImportCredential({
        credential: cred,
        provider: pasteProvider,
        nickname: pasteNickname.trim(),
      });
      toast.success(`已添加账号（${providerLabel(r.provider)}）`);
      setPasteOpen(false);
      setPasteCredential("");
      setPasteNickname("");
      void refresh();
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  }, [pasteCredential, pasteProvider, pasteNickname, refresh]);

  const doDirImport = useCallback(async () => {
    if (!dirPath.trim()) {
      toast.error("请填写凭证目录路径");
      return;
    }
    setBusy("dir");
    try {
      const r = await api.zcodeImportFromDir(dirPath.trim());
      setDirResult(r);
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
  }, [dirPath, refresh]);

  // ---- 扫描本机凭证 ----

  /**
   * 打开弹窗并立即扫描。
   *
   * 为什么打开就扫、不等用户再点一次：这个功能的**全部价值**就是省掉
   * 手工操作 —— 再要求他点一下「开始扫描」就打了折扣。
   */
  const openScan = useCallback(async () => {
    setScanOpen(true);
    setScanImported(null);
    setScanError(null);
    setScanResult(null);
    setScanning(true);
    try {
      const r = await api.zcodeScanLocal();
      setScanResult(r);
      // 默认勾选**可导入且未导入**的：
      //   · jwt 形态是额度查询用的，当对话凭证用会失败 → 不默认选
      //   · 已导入的再选一次会产生重复账号 → 不默认选
      const preset = new Set<number>();
      for (const it of r.items) {
        if (it.shape !== "jwt" && !it.alreadyImported) preset.add(it.index);
      }
      setScanPicked(preset);
    } catch (e) {
      setScanError(e instanceof Error ? e.message : String(e));
    } finally {
      setScanning(false);
    }
  }, []);

  const toggleScanPick = useCallback((index: number) => {
    setScanPicked((prev) => {
      const next = new Set(prev);
      if (next.has(index)) next.delete(index);
      else next.add(index);
      return next;
    });
  }, []);

  const doScanImport = useCallback(async () => {
    const indices = [...scanPicked].sort((a, b) => a - b);
    if (indices.length === 0) {
      toast.error("请先勾选要导入的凭证");
      return;
    }
    setScanImporting(true);
    try {
      const r = await api.zcodeImportScanned(indices);
      setScanImported(r);
      if (r.importedCount > 0) {
        // ⚠ 导入后额度/套餐/模型是**后端顺手查好**的（否则界面显示「未知」，
        // 用户以为导入失败 —— 所有者的反馈）。
        //
        // 这里如实报告补全结果：查不到时说清原因，而不是让用户
        // 面对一片「未知」去猜是导入没生效还是账号真没额度。
        const enrichFailed = (r.imported || []).filter((x) => x.enriched === false);
        if (enrichFailed.length > 0) {
          const why = enrichFailed.find((x) => x.enrichError)?.enrichError || "上游未返回数据";
          toast.success(`已导入 ${r.importedCount} 个账号`, {
            description: `${enrichFailed.length} 个的额度/模型没查到：${why}`,
            duration: 8000,
          });
        } else {
          toast.success(`已导入 ${r.importedCount} 个账号（含额度与模型）`);
        }
        // 重新扫一次：让"已导入"标记立刻反映出来，
        // 否则用户会以为没生效、再点一次 → 重复导入
        const again = await api.zcodeScanLocal();
        setScanResult(again);
        setScanPicked(new Set());
        void refresh();
      }
      if (r.failedCount > 0) {
        toast.error(`${r.failedCount} 个导入失败，详见下方`);
      }
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e));
    } finally {
      setScanImporting(false);
    }
  }, [scanPicked, refresh]);

  const startLogin = useCallback(async () => {
    if (!loginProvider) {
      toast.error("请先选择服务商");
      return;
    }
    setBusy("login");
    setLoginError(null);
    setLoginState("idle");
    try {
      const r = await api.zcodeLoginStart(loginProvider);
      setLoginUrl(r.authUrl);
      setLoginState("waiting");
      // 在**系统默认浏览器**里打开（不是 WebView 内的新窗口）。
      //
      // 此前用 window.open —— 在 Tauri 的 WebView 里它不会交给系统浏览器，
      // 表现为"点了没反应"。见 lib/open-browser.ts 的说明。
      //
      // 打开失败**不算登录失败**：弹窗里已经显示可复制的链接与可点的
      // 原生 <a>，用户仍能继续。故这里只记一个提示，不改登录状态机。
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
  }, [loginProvider, pollOnce]);

  const closeLogin = useCallback(() => {
    stopPolling();
    setLoginOpen(false);
    setLoginUrl(null);
    setLoginState("idle");
    setLoginError(null);
    setLoginProvider("");
  }, [stopPolling]);

  /**
   * 刷新单个账号的额度 / 到期时间 / 支持模型。
   *
   * ## 为什么必须有这个按钮
   *
   * 后端的 `FetchQuota` / `FetchModels` 早已实现，但**没有任何生产者
   * 调用**（实测 `grep FetchQuota` 只命中定义与测试）—— 于是额度恒为 0
   *（界面显示"未知"）、到期恒为空、看不到支持模型。用户看到的现象
   * 就是"查不到额度"。
   *
   * 这个按钮补上那条链路：调后端查一次并落库，然后重载列表。
   *
   * ## 为什么要显示具体的失败原因
   *
   * 取不到的原因有多种（没有 JWT、上游不认、该账号真没额度），
   * 用户需要知道是哪种才能决定下一步。故把 `quota.error` /
   * `modelsError` 原样提示，而不是笼统的"刷新失败"。
   */
  const refreshAccount = useCallback(
    async (row: ZcodeAccountRow) => {
      setBusy(`refresh:${row.uid}`);
      try {
        const r = await api.zcodeRefreshAccount(row.uid);
        const parts: string[] = [];
        if (r.quota.error) {
          parts.push(`额度：${r.quota.error}`);
        } else if (r.quota.remaining !== null && r.quota.remaining !== undefined) {
          parts.push(`额度 ${r.quota.remaining.toLocaleString()}`);
        }
        if (r.modelsError) {
          parts.push(`模型：${r.modelsError}`);
        } else if (r.models.length > 0) {
          parts.push(`模型 ${r.models.length} 个`);
        }
        toast.success(parts.length ? `已刷新：${parts.join("；")}` : "已刷新", { duration: 6000 });
        void refresh();
      } catch (e) {
        toast.error(e instanceof Error ? e.message : String(e));
      } finally {
        setBusy(null);
      }
    },
    [refresh],
  );

  const toggleDisabled = useCallback(async (row: ZcodeAccountRow) => {
    setBusy(row.uid);
    try {
      await api.zcodeSaveAccount(row.uid, { disabled: !row.disabled });
      setRows((prev) => prev.map((r) => (r.uid === row.uid ? { ...r, disabled: !row.disabled } : r)));
      toast.success(row.disabled ? "已恢复接流量" : "已停止接流量");
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  }, []);

  const saveNote = useCallback(async () => {
    if (!editTarget) return;
    setBusy(editTarget.uid);
    try {
      await api.zcodeSaveAccount(editTarget.uid, { note: editNote });
      setRows((prev) => prev.map((r) => (r.uid === editTarget.uid ? { ...r, note: editNote } : r)));
      setEditTarget(null);
      toast.success("备注已保存");
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  }, [editNote, editTarget]);

  const remove = useCallback(async (row: ZcodeAccountRow) => {
    const name = row.nickname || row.uid.slice(0, 12);
    if (!window.confirm(`确定删除账号「${name}」吗？\n\n这会同时删除它的凭证文件。`)) return;
    setBusy(row.uid);
    try {
      await api.zcodeDeleteAccount(row.uid);
      setRows((prev) => prev.filter((r) => r.uid !== row.uid));
      toast.success("账号已删除");
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  }, []);

  const totals = useMemo(() => {
    const total = rows.length;
    const disabled = rows.filter((r) => r.disabled).length;
    const missing = rows.filter((r) => !r.hasCredential).length;
    // 只统计**已知**额度：未知的账号不参与合计（否则会显示成 0 拉低总数）
    const known = rows.filter((r) => r.credits > 0);
    const credits = known.reduce((s, r) => s + r.credits, 0);
    const unknownCredits = rows.length - known.length;
    return { total, disabled, missing, credits, unknownCredits };
  }, [rows]);

  return (
    <TooltipProvider delayDuration={200}>
      <div className="w-full space-y-6 px-5 py-6 sm:px-8 sm:py-8">
        <header className="mb-6">
          <div className="flex items-start gap-3">
            <ZcodeMark size={34} className="mt-0.5" />
            <div className="min-w-0 flex-1">
              <h1 className="text-[28px] font-semibold tracking-tight">ZCode 账号</h1>
              <p className="mt-2 text-sm leading-6 text-muted-foreground">
                管理 ZCode（Z.AI / 智谱 GLM 编码套餐）账号，与 WorkBuddy、Qoder 一起参与网关路由。
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
              {/* 额度未知的账号数：让"合计"这个数字有明确的口径 */}
              {!loading && totals.unknownCredits > 0 && (
                <CardDescription className="text-xs">
                  另有 {totals.unknownCredits} 个账号额度未知
                </CardDescription>
              )}
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

        {/* 额度未知说明：这不是错误，但用户需要知道为什么 */}
        {!loading && totals.unknownCredits > 0 && (
          <Alert>
            <AlertTriangle className="h-4 w-4" />
            <AlertTitle>有 {totals.unknownCredits} 个账号的额度未知</AlertTitle>
            <AlertDescription>
              {/* ⚠ 标点必须用 `{"…"}` 表达式紧跟元素，不能写在下一行的文本里。
                  JSX 会把元素后的换行 + 缩进压成**一个空格**，于是
                  `</strong>` 与 `，` 之间多出一个空格，中文排版上表现为
                  "逗号被挤到下一行开头"（实测 1500px 宽就能看到）。 */}
              ZCode 的额度查询需要浏览器授权登录（OAuth）才会拿到查询令牌。
              <strong>只粘贴了凭证的账号查不到额度</strong>
              {"，但完全可以正常使用。若要查看额度，请用「浏览器登录」方式重新添加。"}
            </AlertDescription>
          </Alert>
        )}

        {!loading && totals.missing > 0 && (
          <Alert>
            <AlertTriangle className="h-4 w-4" />
            <AlertTitle>有 {totals.missing} 个账号缺少凭证</AlertTitle>
            <AlertDescription>
              这些账号的凭证文件已不存在，
              <strong>在网关里已无法使用</strong>
              {"。请重新导入凭证。"}
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

        {!loading && orphans.length > 0 && (
          <Alert>
            <AlertTriangle className="h-4 w-4" />
            <AlertTitle>发现 {orphans.length} 个未登记的凭证</AlertTitle>
            <AlertDescription>
              这些凭证文件存在于磁盘上，但没有对应的账号记录。
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
                  Z.AI 与智谱共用同一套账号管理；服务商决定请求发往哪个域名。
                </CardDescription>
              </div>
              <div className="flex flex-wrap gap-2">
                <Button variant="outline" size="sm" onClick={() => void refresh()} disabled={loading}>
                  <RefreshCw className={cn("mr-2 h-4 w-4", loading && "animate-spin")} />
                  刷新
                </Button>
                <Button variant="outline" size="sm" onClick={() => setDirOpen(true)}>
                  <Upload className="mr-2 h-4 w-4" />
                  从目录导入
                </Button>
                {/* 「扫描本机凭证」放在「粘贴凭证」旁边：两者都是"把已有凭证弄进来"，
                    而扫描更省事（ZCode 客户端已经把凭证明文落在本机了）。 */}
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => void openScan()}
                  disabled={scanning}
                >
                  <Search className={cn("mr-2 h-4 w-4", scanning && "animate-pulse")} />
                  扫描本机凭证
                </Button>
                <Button variant="outline" size="sm" onClick={() => setLoginOpen(true)}>
                  <Globe className="mr-2 h-4 w-4" />
                  浏览器登录
                </Button>
                {/* 「粘贴凭证」是 ZCode 的主路径，故用主按钮 */}
                <Button size="sm" onClick={() => setPasteOpen(true)}>
                  <KeyRound className="mr-2 h-4 w-4" />
                  粘贴凭证
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
                <ZcodeMark size={28} className="mx-auto mb-3 opacity-60" />
                {/* 读取失败时不显示"还没有账号" —— 那会把"后端坏了/版本旧"
                    误导成"我自己没加过账号"。 */}
                {loadError ? (
                  <>
                    <p className="text-sm font-medium">无法读取账号列表</p>
                    <p className="mt-1 text-sm text-muted-foreground">
                      请按上方提示处理后重试；若问题持续，请检查网关服务是否在运行。
                    </p>
                  </>
                ) : (
                  <>
                    <p className="text-sm font-medium">还没有 ZCode 账号</p>
                    <p className="mt-1 text-sm text-muted-foreground">
                      点「粘贴凭证」把控制台复制的凭证贴进来，或「浏览器登录」用账号授权。
                    </p>
                  </>
                )}
              </div>
            ) : (
              /* 卡片布局（对齐 WorkBuddy 账号页）。
                 此前这里是 `<table>` —— 三者形态不一致，且窄屏下会横向溢出。
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
                      mark={(size) => <ZcodeMark size={size} />}
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
                        // 套餐类型与**套餐整体到期**。
                        //
                        // 所有者的原话：「这个体验套餐是 9月23号23:59 过期时间,
                        // 这是我刚登录的新账号赠送的额度,要区分好」——
                        // 这两个字段此前根本没从上游读出来（我们漏读了
                        // billing/balance 的 plans）。
                        planBadge: planBadgeOf(row),
                        planExpiryText: planExpiryTextOf(row, now),
                        models: row.models,
                        hasCredential: row.hasCredential,
                        disabled: row.disabled,
                        variantLabel: providerLabel(row.provider),
                        variantKind: providerVariant(row.provider),
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

        {/* 粘贴凭证（主路径） */}
        <Dialog open={pasteOpen} onOpenChange={setPasteOpen}>
          <DialogContent className="sm:max-w-lg">
            <DialogHeader>
              <DialogTitle>粘贴 ZCode 凭证</DialogTitle>
              <DialogDescription>
                从 ZCode / Z.AI 控制台复制凭证后粘贴到这里。形如{" "}
                <code className="font-mono">apiKey.secret</code>（智谱可能只有一段）。
              </DialogDescription>
            </DialogHeader>
            <div className="space-y-4 py-2">
              <div className="space-y-2">
                <Label htmlFor="zcode-cred">凭证</Label>
                <Input
                  id="zcode-cred"
                  type="password"
                  placeholder="粘贴凭证"
                  value={pasteCredential}
                  onChange={(e) => setPasteCredential(e.target.value)}
                  autoComplete="off"
                  spellCheck={false}
                />
              </div>
              <div className="space-y-2">
                <Label>服务商</Label>
                <div className="grid grid-cols-2 gap-2">
                  {(["zai", "bigmodel"] as const).map((p) => (
                    <Button
                      key={p}
                      type="button"
                      variant={pasteProvider === p ? "default" : "outline"}
                      onClick={() => setPasteProvider(p)}
                      className="justify-start"
                    >
                      <Globe className="mr-2 h-4 w-4" />
                      {providerLabel(p)}
                      <span className="ml-2 text-xs opacity-70">
                        {p === "zai" ? "api.z.ai" : "open.bigmodel.cn"}
                      </span>
                    </Button>
                  ))}
                </div>
                <p className="text-xs text-muted-foreground">
                  两家的接口域名不同，请按凭证来源选择。
                </p>
              </div>
              <div className="space-y-2">
                <Label htmlFor="zcode-nick">昵称（可选）</Label>
                <Input
                  id="zcode-nick"
                  placeholder="例如：主力号"
                  value={pasteNickname}
                  onChange={(e) => setPasteNickname(e.target.value)}
                />
              </div>
            </div>
            <DialogFooter>
              <Button variant="outline" onClick={() => setPasteOpen(false)}>
                取消
              </Button>
              <Button onClick={() => void doPaste()} disabled={busy === "paste"}>
                {busy === "paste" && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
                添加
              </Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>

        {/* 浏览器登录（备选） */}
        <Dialog open={loginOpen} onOpenChange={(o) => (o ? setLoginOpen(true) : closeLogin())}>
          <DialogContent className="sm:max-w-lg">
            <DialogHeader>
              <DialogTitle>浏览器登录 ZCode</DialogTitle>
              <DialogDescription>
                选择服务商后会在浏览器打开授权页，完成授权即可自动添加账号
                （这种方式还能查看额度）。
              </DialogDescription>
            </DialogHeader>
            <div className="space-y-4 py-2">
              <div className="space-y-2">
                <Label>服务商</Label>
                <div className="grid grid-cols-2 gap-2">
                  {(["zai", "bigmodel"] as const).map((p) => (
                    <Button
                      key={p}
                      type="button"
                      variant={loginProvider === p ? "default" : "outline"}
                      onClick={() => setLoginProvider(p)}
                      disabled={loginState === "waiting"}
                      className="justify-start"
                    >
                      <Globe className="mr-2 h-4 w-4" />
                      {providerLabel(p)}
                    </Button>
                  ))}
                </div>
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
                <Button onClick={() => void startLogin()} disabled={busy === "login" || !loginProvider}>
                  {busy === "login" && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
                  {loginUrl ? "重新发起" : "打开授权页"}
                </Button>
              )}
            </DialogFooter>
          </DialogContent>
        </Dialog>

        {/* 目录导入 */}
        <Dialog open={dirOpen} onOpenChange={setDirOpen}>
          <DialogContent className="sm:max-w-lg">
            <DialogHeader>
              <DialogTitle>从目录导入凭证</DialogTitle>
              <DialogDescription>
                填写凭证文件所在目录，会扫描其中的 zcode*.json 文件。
              </DialogDescription>
            </DialogHeader>
            <div className="space-y-4 py-2">
              <div className="space-y-2">
                <Label htmlFor="zcode-dir">目录</Label>
                <Input
                  id="zcode-dir"
                  placeholder="C:\Users\你\zcode-auths"
                  value={dirPath}
                  onChange={(e) => setDirPath(e.target.value)}
                />
              </div>
              {dirResult && (
                <>
                  <Separator />
                  {dirResult.imported.length > 0 && (
                    <div className="space-y-1">
                      <div className="text-sm font-medium text-emerald-600">
                        成功 {dirResult.importedCount} 个
                      </div>
                      {dirResult.imported.map((x) => (
                        <div key={x.file} className="font-mono text-xs text-muted-foreground">
                          {x.file} → {providerLabel(x.provider)}
                        </div>
                      ))}
                    </div>
                  )}
                  {dirResult.failed.length > 0 && (
                    <div className="space-y-1">
                      <div className="text-sm font-medium text-destructive">
                        失败 {dirResult.failedCount} 个
                      </div>
                      {dirResult.failed.map((x) => (
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
              <Button variant="outline" onClick={() => setDirOpen(false)}>
                关闭
              </Button>
              <Button onClick={() => void doDirImport()} disabled={busy === "dir"}>
                {busy === "dir" && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
                开始导入
              </Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>

        {/* 扫描本机凭证 */}
        <Dialog open={scanOpen} onOpenChange={setScanOpen}>
          <DialogContent className="sm:max-w-2xl">
            <DialogHeader>
              <DialogTitle>扫描本机凭证</DialogTitle>
              <DialogDescription>
                读取 ZCode 客户端已登录的凭证，不必手工粘贴。
                <span className="text-muted-foreground">
                  {" "}
                  （只读扫描，不会改动客户端自己的配置文件）
                </span>
              </DialogDescription>
            </DialogHeader>

            <div className="max-h-[26rem] space-y-3 overflow-y-auto py-2">
              {scanning && (
                <div className="space-y-2">
                  <Skeleton className="h-16 w-full" />
                  <Skeleton className="h-16 w-full" />
                </div>
              )}

              {scanError && (
                <Alert variant="destructive">
                  <AlertTitle>扫描失败</AlertTitle>
                  <AlertDescription className="break-all">{scanError}</AlertDescription>
                </Alert>
              )}

              {/* 扫不到时说清楚**扫过哪里** —— 否则用户对着"没找到"无从判断
                  是"确实没有"还是"路径不对"。 */}
              {!scanning && scanResult && scanResult.count === 0 && (
                <div className="rounded-lg border border-dashed px-5 py-8 text-center">
                  <Search className="mx-auto mb-3 h-6 w-6 opacity-50" />
                  <p className="text-sm font-medium">没有扫描到可用的凭证</p>
                  <p className="mt-1 text-xs text-muted-foreground">
                    请先在 ZCode 客户端里登录，或改用「粘贴凭证」。
                  </p>
                  {scanResult.scannedPaths.length > 0 && (
                    <details className="mt-3 text-left">
                      <summary className="cursor-pointer text-xs text-muted-foreground">
                        查看扫描过哪些位置
                      </summary>
                      <div className="mt-2 space-y-0.5">
                        {scanResult.scannedPaths.map((p) => (
                          <div key={p} className="break-all font-mono text-[11px] text-muted-foreground">
                            {p}
                          </div>
                        ))}
                      </div>
                    </details>
                  )}
                </div>
              )}

              {!scanning && scanResult && scanResult.count > 0 && (
                <>
                  {/* 已登录的账号 —— 这条信息解决一个真实的困惑：
                      "我明明登录了 wish，为什么导入的账号叫别的名字"。
                      名字取自客户端登录态，不是套餐名。 */}
                  {scanResult.identity && (
                    <div className="flex items-center gap-3 rounded-lg border bg-muted/40 px-4 py-3">
                      {scanResult.identity.avatarUrl ? (
                        <img
                          src={scanResult.identity.avatarUrl}
                          alt=""
                          className="h-9 w-9 shrink-0 rounded-full"
                          referrerPolicy="no-referrer"
                        />
                      ) : (
                        <div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-full bg-primary/10 text-sm font-medium">
                          {(scanResult.identity.displayName || scanResult.identity.username || "?").slice(0, 1)}
                        </div>
                      )}
                      <div className="min-w-0">
                        <div className="text-sm font-medium">
                          已登录：{scanResult.identity.displayName || scanResult.identity.username}
                        </div>
                        <div className="truncate text-xs text-muted-foreground">
                          {scanResult.identity.activeProvider
                            ? `当前服务商：${scanResult.identity.activeProvider === "bigmodel" ? "智谱" : "Z.AI"}`
                            : "账号名读自客户端登录态"}
                        </div>
                      </div>
                    </div>
                  )}
                  <p className="text-xs text-muted-foreground">
                    找到 {scanResult.count} 条凭证。勾选后导入（凭证不会离开本机进程）。
                  </p>
                  <div className="space-y-2">
                    {scanResult.items.map((it) => {
                      const isJwt = it.shape === "jwt";
                      // jwt 不能当对话凭证；已导入的再导会产生重复账号。
                      // 两者都**可以**勾（用户可能有自己的理由），但要给出提示。
                      return (
                        <label
                          key={it.index}
                          className={cn(
                            "flex cursor-pointer items-start gap-3 rounded-lg border p-3 transition-colors",
                            scanPicked.has(it.index) && "border-primary/50 bg-accent/40",
                          )}
                        >
                          <Checkbox
                            checked={scanPicked.has(it.index)}
                            onCheckedChange={() => toggleScanPick(it.index)}
                            className="mt-0.5"
                          />
                          <div className="min-w-0 flex-1 space-y-1">
                            <div className="flex flex-wrap items-center gap-2">
                              <span className="truncate text-sm font-medium">
                                {it.suggestedNickname || it.originName}
                              </span>
                              <Badge variant="outline" className="shrink-0 text-[11px]">
                                {it.providerLabel}
                              </Badge>
                              {it.alreadyImported && (
                                <Badge variant="secondary" className="shrink-0 text-[11px]">
                                  已导入
                                </Badge>
                              )}
                              {isJwt && (
                                <Badge variant="outline" className="shrink-0 text-[11px] text-amber-600">
                                  仅额度令牌
                                </Badge>
                              )}
                              {!it.enabled && (
                                <Badge variant="outline" className="shrink-0 text-[11px]">
                                  客户端里已停用
                                </Badge>
                              )}
                              {/* 额度令牌的配对情况要说清楚：额度查询只认 JWT，
                                  没有它导入后额度会显示「未知」—— 用户会以为
                                  是自己的账号没额度。 */}
                              <Badge
                                variant="outline"
                                className={cn(
                                  "shrink-0 text-[11px]",
                                  it.hasQuotaToken ? "text-emerald-600" : "text-muted-foreground",
                                )}
                              >
                                {it.hasQuotaToken ? "含额度令牌" : "无额度令牌"}
                              </Badge>
                            </div>
                            <div className="font-mono text-xs text-muted-foreground">{it.masked}</div>
                            {isJwt && (
                              <p className="text-[11px] text-amber-600">
                                这是额度查询令牌（JWT），不能用于对话。导入后额度可读，但发不出请求。
                              </p>
                            )}
                            <div className="break-all text-[11px] text-muted-foreground">
                              {it.source}
                            </div>
                          </div>
                        </label>
                      );
                    })}
                  </div>

                  {scanResult.problems.length > 0 && (
                    <Alert>
                      <AlertTitle>有 {scanResult.problems.length} 个位置读取异常</AlertTitle>
                      <AlertDescription>
                        <div className="mt-1 space-y-0.5">
                          {scanResult.problems.map((p) => (
                            <div key={p} className="break-all text-xs">
                              {p}
                            </div>
                          ))}
                        </div>
                      </AlertDescription>
                    </Alert>
                  )}
                </>
              )}

              {scanImported && (
                <>
                  <Separator />
                  {scanImported.importedCount > 0 && (
                    <div className="space-y-1">
                      <div className="text-sm font-medium text-emerald-600">
                        成功导入 {scanImported.importedCount} 个
                      </div>
                      {scanImported.imported.map((x) => (
                        <div key={x.uid} className="font-mono text-xs text-muted-foreground">
                          {x.origin} → {x.uid.slice(0, 16)}
                        </div>
                      ))}
                    </div>
                  )}
                  {scanImported.failedCount > 0 && (
                    <div className="space-y-1">
                      <div className="text-sm font-medium text-destructive">
                        失败 {scanImported.failedCount} 个
                      </div>
                      {scanImported.failed.map((x) => (
                        <div key={`${x.index}-${x.error}`} className="text-xs text-muted-foreground">
                          <span className="font-mono">{x.origin ?? `#${x.index}`}</span>：{x.error}
                        </div>
                      ))}
                    </div>
                  )}
                </>
              )}
            </div>

            <DialogFooter>
              <Button variant="outline" onClick={() => setScanOpen(false)}>
                关闭
              </Button>
              <Button onClick={() => void openScan()} disabled={scanning || scanImporting} variant="ghost">
                <RefreshCw className={cn("mr-2 h-4 w-4", scanning && "animate-spin")} />
                重新扫描
              </Button>
              <Button
                onClick={() => void doScanImport()}
                disabled={scanImporting || scanPicked.size === 0}
              >
                {scanImporting && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
                导入所选（{scanPicked.size}）
              </Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>

        {/* 编辑备注 */}
        <Dialog open={editTarget !== null} onOpenChange={(o) => !o && setEditTarget(null)}>
          <DialogContent className="sm:max-w-md">
            <DialogHeader>
              <DialogTitle>编辑备注</DialogTitle>
              <DialogDescription>{editTarget?.nickname || editTarget?.uid.slice(0, 16)}</DialogDescription>
            </DialogHeader>
            <div className="space-y-2 py-2">
              <Label htmlFor="zcode-note">备注</Label>
              <Input
                id="zcode-note"
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
