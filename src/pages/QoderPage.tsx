import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { toast } from "sonner";
import {
  AlertTriangle,
  CheckCircle2,
  Download,
  ExternalLink,
  Globe,
  Loader2,
  Pencil,
  RefreshCw,
  Trash2,
  Upload,
} from "lucide-react";
import { QoderMark } from "@/components/product-marks";
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
      // 自动打开浏览器；被拦截也没关系，界面上有链接可点
      window.open(r.authUrl, "_blank", "noopener,noreferrer");
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
                <Button variant="outline" size="sm" onClick={() => void refresh()} disabled={loading}>
                  <RefreshCw className={cn("mr-2 h-4 w-4", loading && "animate-spin")} />
                  刷新
                </Button>
                <Button variant="outline" size="sm" onClick={() => setImportOpen(true)}>
                  <Upload className="mr-2 h-4 w-4" />
                  导入凭证
                </Button>
                <Button size="sm" onClick={() => setLoginOpen(true)}>
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
              <div className="overflow-x-auto">
                <table className="w-full text-sm">
                  <thead>
                    <tr className="border-b text-left text-muted-foreground">
                      <th className="py-2 pr-3 font-medium">账号</th>
                      <th className="py-2 pr-3 font-medium">区域</th>
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
                              {row.uid.slice(0, 12)}
                            </div>
                          </td>
                          <td className="py-3 pr-3">
                            <Badge variant={regionVariant(row.region)}>{regionLabel(row.region)}</Badge>
                          </td>
                          <td className="py-3 pr-3">
                            <div className="flex flex-wrap items-center gap-1.5">
                              {!row.hasCredential ? (
                                <Tooltip>
                                  <TooltipTrigger asChild>
                                    <Badge variant="destructive">凭证缺失</Badge>
                                  </TooltipTrigger>
                                  <TooltipContent>
                                    凭证文件不存在，该账号在网关里已不可用，需重新登录
                                  </TooltipContent>
                                </Tooltip>
                              ) : row.disabled ? (
                                <Tooltip>
                                  <TooltipTrigger asChild>
                                    <Badge variant="outline">已停用</Badge>
                                  </TooltipTrigger>
                                  <TooltipContent>
                                    不参与网关选号；凭证与额度信息仍然保留
                                  </TooltipContent>
                                </Tooltip>
                              ) : (
                                <Badge variant="secondary">正常</Badge>
                              )}
                            </div>
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
                              <span className="text-muted-foreground">未知</span>
                            )}
                          </td>
                          <td className={cn("py-3 pr-3", exp.urgent && "text-amber-600")}>
                            {exp.text}
                          </td>
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
                                <TooltipContent>
                                  {row.disabled ? "恢复参与路由" : "停止接流量"}
                                </TooltipContent>
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
