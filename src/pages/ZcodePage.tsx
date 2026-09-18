import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { toast } from "sonner";
import {
  AlertTriangle,
  CheckCircle2,
  ExternalLink,
  Globe,
  KeyRound,
  Loader2,
  Pencil,
  RefreshCw,
  Trash2,
  Upload,
} from "lucide-react";
import { ZcodeMark } from "@/components/product-marks";
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
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
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

export default function ZcodePage() {
  const [rows, setRows] = useState<ZcodeAccountRow[]>([]);
  const [orphans, setOrphans] = useState<string[]>([]);
  const [summary, setSummary] = useState<ZcodeSummary | null>(null);
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
      window.open(r.authUrl, "_blank", "noopener,noreferrer");
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
              ZCode 的额度查询需要浏览器授权登录（OAuth）才会拿到查询令牌。
              <strong>只粘贴了凭证的账号查不到额度</strong>，但完全可以正常使用。
              若要查看额度，请用「浏览器登录」方式重新添加。
            </AlertDescription>
          </Alert>
        )}

        {!loading && totals.missing > 0 && (
          <Alert>
            <AlertTriangle className="h-4 w-4" />
            <AlertTitle>有 {totals.missing} 个账号缺少凭证</AlertTitle>
            <AlertDescription>
              这些账号的凭证文件已不存在，<strong>在网关里已无法使用</strong>。请重新导入凭证。
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
              <div className="overflow-x-auto">
                <table className="w-full text-sm">
                  <thead>
                    <tr className="border-b text-left text-muted-foreground">
                      <th className="py-2 pr-3 font-medium">账号</th>
                      <th className="py-2 pr-3 font-medium">服务商</th>
                      <th className="py-2 pr-3 font-medium">状态</th>
                      <th className="py-2 pr-3 font-medium">额度</th>
                      <th className="py-2 pr-3 font-medium">到期</th>
                      <th className="py-2 pr-3 font-medium">备注</th>
                      <th className="py-2 text-right font-medium">操作</th>
                    </tr>
                  </thead>
                  <tbody>
                    {rows.map((row) => {
                      const exp = expiryText(row.expireAt);
                      return (
                        <tr key={row.uid} className="border-b last:border-0">
                          <td className="py-3 pr-3">
                            <div className="font-medium">{row.nickname || "（未命名）"}</div>
                            <div className="font-mono text-xs text-muted-foreground">
                              {row.uid.slice(0, 16)}
                            </div>
                          </td>
                          <td className="py-3 pr-3">
                            <Badge variant={providerVariant(row.provider)}>
                              {providerLabel(row.provider)}
                            </Badge>
                          </td>
                          <td className="py-3 pr-3">
                            {!row.hasCredential ? (
                              <Tooltip>
                                <TooltipTrigger asChild>
                                  <Badge variant="destructive">凭证缺失</Badge>
                                </TooltipTrigger>
                                <TooltipContent>
                                  凭证文件不存在，该账号在网关里已不可用，需重新导入
                                </TooltipContent>
                              </Tooltip>
                            ) : row.disabled ? (
                              <Tooltip>
                                <TooltipTrigger asChild>
                                  <Badge variant="outline">已停用</Badge>
                                </TooltipTrigger>
                                <TooltipContent>不参与网关选号；凭证与额度信息仍然保留</TooltipContent>
                              </Tooltip>
                            ) : (
                              <Badge variant="secondary">正常</Badge>
                            )}
                          </td>
                          <td className="py-3 pr-3 tabular-nums">
                            {row.credits > 0 ? (
                              <>
                                {row.credits.toLocaleString()}
                                {row.creditsTotal > 0 && (
                                  <span className="text-muted-foreground">
                                    {" / "}
                                    {row.creditsTotal.toLocaleString()}
                                  </span>
                                )}
                              </>
                            ) : (
                              // 额度未知显示"未知"而不是 0 —— 0 会被误读成"额度耗尽"
                              <Tooltip>
                                <TooltipTrigger asChild>
                                  <span className="cursor-help text-muted-foreground underline decoration-dotted">
                                    未知
                                  </span>
                                </TooltipTrigger>
                                <TooltipContent>
                                  额度查询需要浏览器授权登录；只粘贴凭证的账号查不到额度，但不影响使用
                                </TooltipContent>
                              </Tooltip>
                            )}
                          </td>
                          <td className={cn("py-3 pr-3", exp.urgent && "text-amber-600")}>{exp.text}</td>
                          <td className="max-w-[14rem] truncate py-3 pr-3 text-muted-foreground">
                            {row.note || "—"}
                          </td>
                          <td className="py-3 text-right">
                            <div className="flex justify-end gap-1">
                              <Tooltip>
                                <TooltipTrigger asChild>
                                  <Button
                                    variant="ghost"
                                    size="icon"
                                    // 图标按钮**必须**有可访问名：Tooltip 的内容要悬停
                                    // 才进 DOM，屏幕阅读器（与自动化测试）都拿不到。
                                    aria-label={`编辑备注：${row.nickname || row.uid}`}
                                    onClick={() => {
                                      setEditTarget(row);
                                      setEditNote(row.note);
                                    }}
                                  >
                                    <Pencil className="h-4 w-4" />
                                  </Button>
                                </TooltipTrigger>
                                <TooltipContent>编辑备注</TooltipContent>
                              </Tooltip>
                              <Tooltip>
                                <TooltipTrigger asChild>
                                  <Button
                                    variant="ghost"
                                    size="icon"
                                    disabled={busy === row.uid}
                                    aria-label={`${row.disabled ? "恢复参与路由" : "停止接流量"}：${row.nickname || row.uid}`}
                                    onClick={() => void toggleDisabled(row)}
                                  >
                                    {row.disabled ? (
                                      <CheckCircle2 className="h-4 w-4" />
                                    ) : (
                                      <AlertTriangle className="h-4 w-4" />
                                    )}
                                  </Button>
                                </TooltipTrigger>
                                <TooltipContent>{row.disabled ? "恢复参与路由" : "停止接流量"}</TooltipContent>
                              </Tooltip>
                              <Tooltip>
                                <TooltipTrigger asChild>
                                  <Button
                                    variant="ghost"
                                    size="icon"
                                    disabled={busy === row.uid}
                                    aria-label={`删除账号：${row.nickname || row.uid}`}
                                    onClick={() => void remove(row)}
                                  >
                                    <Trash2 className="h-4 w-4 text-destructive" />
                                  </Button>
                                </TooltipTrigger>
                                <TooltipContent>删除账号（含凭证）</TooltipContent>
                              </Tooltip>
                            </div>
                          </td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              </div>
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
