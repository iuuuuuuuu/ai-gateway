import { useCallback, useEffect, useRef, useState } from "react";
import { toast } from "sonner";
import {
  AlertTriangle,
  Coins,
  Download,
  FileDown,
  KeyRound,
  Loader2,
  MessageSquare,
  Power,
  RefreshCw,
  Stethoscope,
  Trash2,
  Upload,
  UserPlus,
} from "lucide-react";

import { DoubaoMark } from "@/components/product-marks";
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
import type { AppEnvStatus, DoubaoAccountView, DoubaoQuotaView } from "@/lib/api";
import { cn } from "@/lib/utils";

/** 会话状态展示。 */
function sessionBadge(account: DoubaoAccountView) {
  const map = {
    ok: { label: "会话有效", variant: "secondary" as const, tip: "最近一次探活确认会话可用" },
    expired: {
      label: "会话失效",
      variant: "destructive" as const,
      tip: "需要重新登录豆包并抓取新凭证",
    },
    unknown: {
      label: "未探活",
      variant: "outline" as const,
      tip: "尚未确认会话状态，可点「探活」检查",
    },
    none: {
      label: "无凭证",
      variant: "outline" as const,
      tip: "该账号还没有 sessionid，开启本地代理后访问豆包可自动抓取",
    },
  };
  const item = map[account.sessionState];
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Badge variant={item.variant} className="cursor-help">
          {item.label}
        </Badge>
      </TooltipTrigger>
      <TooltipContent side="top" className="max-w-xs">
        {item.tip}
      </TooltipContent>
    </Tooltip>
  );
}

/** 额度徽标。 */
function quotaBadge(account: DoubaoAccountView) {
  const label = account.quotaLevel || (account.quotaCheckedAt ? "免费" : null);
  if (!label) return null;
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Badge variant="outline" className="cursor-help gap-1">
          <Coins className="size-3" />
          {label}
        </Badge>
      </TooltipTrigger>
      <TooltipContent side="top" className="max-w-xs space-y-1">
        <div>{account.quotaSummary || "暂无额度明细"}</div>
        {account.quotaExpireAt && <div>到期：{account.quotaExpireAt}</div>}
        {account.quotaCheckedAt && (
          <div className="text-muted-foreground">查询于 {account.quotaCheckedAt}</div>
        )}
      </TooltipContent>
    </Tooltip>
  );
}

/** 额度明细弹窗。 */
function QuotaDialog({
  quota,
  onClose,
}: {
  quota: DoubaoQuotaView | null;
  onClose: () => void;
}) {
  if (!quota) return null;
  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>会员额度</DialogTitle>
          <DialogDescription>{quota.summary}</DialogDescription>
        </DialogHeader>
        <div className="space-y-3">
          <div className="flex flex-wrap gap-2 text-xs">
            <Badge variant={quota.hasSubscription ? "secondary" : "outline"}>
              {quota.level || "免费"}
            </Badge>
            {quota.isGift && <Badge variant="outline">赠送</Badge>}
            {quota.expireAt && <Badge variant="outline">到期 {quota.expireAt}</Badge>}
          </div>

          {quota.windows.length > 0 && (
            <div className="space-y-2">
              {quota.windows.map((win, index) => {
                const percent = Math.min(100, Math.max(0, win.usedPercent ?? 0));
                return (
                  <div key={`${win.name}-${index}`} className="space-y-1">
                    <div className="flex items-center justify-between text-xs">
                      <span className="font-medium">{win.name}</span>
                      <span className="text-muted-foreground">
                        {win.exhausted ? "已用完" : `${percent.toFixed(0)}%`}
                        {win.resetAt ? ` · ${win.resetAt} 重置` : ""}
                      </span>
                    </div>
                    <div className="h-1.5 w-full overflow-hidden rounded-full bg-muted">
                      <div
                        className={cn(
                          "h-full rounded-full transition-all",
                          win.exhausted ? "bg-destructive" : "bg-primary",
                        )}
                        style={{ width: `${percent}%` }}
                      />
                    </div>
                  </div>
                );
              })}
            </div>
          )}

          {quota.items.length > 0 && (
            <div className="space-y-1">
              {quota.items.map((item, index) => (
                <div
                  key={`${item.name}-${index}`}
                  className="flex items-center justify-between rounded border border-border/60 px-3 py-1.5 text-xs"
                >
                  <span>{item.name}</span>
                  <span className="text-muted-foreground">
                    {item.left != null ? `${item.left} / ${item.total}` : item.total}
                  </span>
                </div>
              ))}
            </div>
          )}

          {quota.subscription && (
            <div className="rounded-lg border border-border/60 p-3 text-xs">
              <div className="mb-1 font-medium">订阅记录</div>
              <pre className="overflow-x-auto whitespace-pre-wrap break-all text-muted-foreground">
                {JSON.stringify(quota.subscription, null, 2)}
              </pre>
            </div>
          )}
        </div>
        <DialogFooter>
          <Button variant="outline" onClick={onClose}>
            关闭
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

export default function DoubaoPage() {
  const [accounts, setAccounts] = useState<DoubaoAccountView[]>([]);
  const [lastKeepaliveAt, setLastKeepaliveAt] = useState<string | null>(null);
  const [env, setEnv] = useState<AppEnvStatus | null>(null);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [quota, setQuota] = useState<DoubaoQuotaView | null>(null);
  const [diagOpen, setDiagOpen] = useState(false);
  const [diag, setDiag] = useState<Record<string, unknown> | null>(null);

  // 新增/编辑
  const [editOpen, setEditOpen] = useState(false);
  const [editUid, setEditUid] = useState("");
  const [editName, setEditName] = useState("");
  const [editNote, setEditNote] = useState("");
  const [editSession, setEditSession] = useState("");
  const [editGuard, setEditGuard] = useState("");
  const [editTtwid, setEditTtwid] = useState("");
  const [isNew, setIsNew] = useState(false);
  // 编辑已有账号时，凭证是否已成功回填。未回填（读取失败）时禁止保存凭证区，
  // 否则空串会被后端当成「清除凭证」。
  const [credPrefilled, setCredPrefilled] = useState(false);

  // 抓包凭证轮询的请求序号：慢响应回来时若已过期就丢弃，
  // 否则会把旧数据盖在新状态上。
  const applySeqRef = useRef(0);

  const load = useCallback(async () => {
    try {
      const [list, envStatus] = await Promise.all([
        api.doubaoListAccounts(),
        api.appEnvCheck("Doubao"),
      ]);
      setAccounts(list.accounts);
      setLastKeepaliveAt(list.lastKeepaliveAt);
      setEnv(envStatus);
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // 抓包凭证自动回写：每 20 秒轮询一次。
  // 后端只在「内容确实变了」时返回 applied=true，因此这里不会反复刷屏。
  useEffect(() => {
    let disposed = false;
    const tick = async () => {
      const seq = ++applySeqRef.current;
      try {
        const res = await api.doubaoCredentialAutoApply();
        if (disposed || seq !== applySeqRef.current) return;
        if (res.applied && res.account) {
          toast.success(`已从代理抓包更新账号 ${res.account.name} 的凭证`);
          await load();
        }
      } catch {
        // 轮询失败静默：抓包未开启是常态，不该每次弹错误
      }
    };
    void tick();
    const timer = window.setInterval(() => void tick(), 20_000);
    return () => {
      disposed = true;
      window.clearInterval(timer);
    };
  }, [load]);

  const openNew = () => {
    setIsNew(true);
    setEditUid("");
    setEditName("");
    setEditNote("");
    setEditSession("");
    setEditGuard("");
    setEditTtwid("");
    setCredPrefilled(true);
    setEditOpen(true);
  };

  const openEdit = async (account: DoubaoAccountView) => {
    setIsNew(false);
    setEditUid(account.userId);
    setEditName(account.name ?? "");
    setEditNote(account.note ?? "");
    setCredPrefilled(false);
    setEditOpen(true);
    try {
      // 列表里的凭证是掩码，编辑需要明文回填
      const cred = await api.doubaoGetCredential(account.userId);
      setEditSession(cred.sessionId ?? "");
      setEditGuard(cred.sidGuard ?? "");
      setEditTtwid(cred.ttwid ?? "");
      setCredPrefilled(true);
    } catch (e) {
      toast.error(api.asError(e));
      // **不能**把字段清空后当作用户输入保存：后端把空串解释为「清除凭证」，
      // 一次读取失败再点保存就会把好账号的 sessionid/sid_guard 抹掉。
      // 这里保持空白并锁住凭证区，强制用户显式重填或改从抓包填充。
      setEditSession("");
      setEditGuard("");
      setEditTtwid("");
      setCredPrefilled(false);
    }
  };

  const fillFromCapture = async () => {
    try {
      const captured = await api.doubaoCapturedCredential();
      if (!captured.available) {
        toast.info("还没有抓包凭证。请先在「设置」中开启本地代理，然后用豆包客户端访问一次。");
        return;
      }
      setEditSession(captured.sessionId ?? "");
      setEditGuard(captured.sidGuard ?? "");
      setEditTtwid(captured.ttwid ?? "");
      // 抓包填充是可用的凭证来源，解除「读取失败」的保存锁
      setCredPrefilled(true);
      toast.success(
        `已填入抓包凭证${captured.uid ? `（账号 ${captured.uid}）` : ""}${
          captured.capturedAt ? `，抓取于 ${captured.capturedAt}` : ""
        }`,
      );
    } catch (e) {
      toast.error(api.asError(e));
    }
  };

  const saveAccount = async () => {
    if (!editUid.trim()) {
      toast.error("请填写账号标识");
      return;
    }
    // 凭证未成功回填时只允许改昵称/备注，绝不提交凭证 ——
    // 后端的空串语义是「清除凭证」，提交空白会把好账号的凭证抹掉。
    if (!credPrefilled) {
      toast.error("凭证未能读取，请点「从代理抓包填充」或手动填入后再保存（留空会清除凭证）");
      return;
    }
    setBusy(true);
    try {
      await api.doubaoPublishAccount({
        userId: editUid.trim(),
        name: editName.trim() || undefined,
        note: editNote.trim() || undefined,
      });
      // 凭证单独提交：空串表示清除
      await api.doubaoSetCredential({
        userId: editUid.trim(),
        sessionId: editSession.trim(),
        sidGuard: editGuard.trim(),
        ttwid: editTtwid.trim(),
      });
      toast.success("已保存");
      setEditOpen(false);
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const doDelete = async (account: DoubaoAccountView) => {
    if (!window.confirm(`确认删除账号 ${account.name ?? account.userId}？`)) return;
    setBusy(true);
    try {
      await api.doubaoDeleteAccount(account.userId);
      toast.success("已删除");
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const doKeepalive = async () => {
    setBusy(true);
    try {
      const res = await api.doubaoKeepalive();
      toast.success(res.message);
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const doRenew = async () => {
    setBusy(true);
    try {
      const res = await api.doubaoRenew(false);
      const parts = [`成功 ${res.ok}`];
      if (res.expired) parts.push(`失效 ${res.expired}`);
      if (res.skipped) parts.push(`跳过 ${res.skipped}`);
      if (res.errors) parts.push(`错误 ${res.errors}`);
      if (res.expired > 0 || res.errors > 0) toast.warning(`探活完成：${parts.join(" · ")}`);
      else toast.success(`探活完成：${parts.join(" · ")}`);
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const doQuota = async (account: DoubaoAccountView) => {
    setBusy(true);
    try {
      const res = await api.doubaoFetchQuota(account.userId);
      setQuota(res);
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const doQuotaBatch = async () => {
    setBusy(true);
    try {
      const res = await api.doubaoQuotaBatch();
      const parts = [`成功 ${res.ok}`];
      if (res.failed) parts.push(`失败 ${res.failed}`);
      if (res.exhausted.length) parts.push(`额度耗尽 ${res.exhausted.length} 个`);
      toast.success(`额度巡检完成：${parts.join(" · ")}`);
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const doProbe = async (account: DoubaoAccountView) => {
    setBusy(true);
    try {
      const res = await api.doubaoProbeAccount(account.userId);
      if (res.ok) toast.success(`会话有效：${res.detail ?? ""}`);
      else toast.warning(res.error ?? res.detail ?? "会话不可用");
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const doBackup = async (account: DoubaoAccountView) => {
    setBusy(true);
    try {
      const res = await api.doubaoBackupChatdata(account.userId);
      toast.success(`已备份 ${res.files} 个文件到 ${res.path}`);
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const doRestore = async (account: DoubaoAccountView) => {
    if (
      !window.confirm(
        `确认把备份的对话状态恢复到 ${account.name ?? account.userId}？\n\n` +
          "恢复会覆盖客户端当前的本地对话缓存。对话正文存在豆包云端，重新登录后会重新同步，不会丢失。",
      )
    )
      return;
    setBusy(true);
    try {
      const res = await api.doubaoRestoreChatdata(account.userId);
      toast.success(`已恢复 ${res.profiles} 个 Profile 的对话状态`);
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const doExport = async (account: DoubaoAccountView) => {
    setBusy(true);
    try {
      const res = await api.doubaoExportChats(account.userId);
      toast.success(
        `已导出 ${res.conversations} 个会话、${res.messages} 条消息到 ${res.mdPath}`,
      );
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const openDiagnose = async () => {
    setBusy(true);
    try {
      setDiag(await api.doubaoDiagnose());
      setDiagOpen(true);
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <TooltipProvider delayDuration={250}>
      <div className="mx-auto max-w-5xl space-y-5 p-6">
        <header className="flex flex-wrap items-center justify-between gap-3">
          <div>
            <h1 className="flex items-center gap-2 text-xl font-semibold tracking-tight">
              <DoubaoMark size={26} />
              豆包账号管理
            </h1>
            <p className="mt-1 text-sm text-muted-foreground">
              豆包客户端在 Chromium 加密之外还有一层客户端级加密，离线读不出明文凭证 ——
              请用本地代理抓包自动获取，或用「保活」触发服务端滑动续期。
            </p>
          </div>
          <div className="flex flex-wrap items-center gap-2">
            <Button variant="outline" size="sm" disabled={busy} onClick={() => void openDiagnose()}>
              <Stethoscope className="size-3.5" />
              诊断
            </Button>
            <Button variant="outline" size="sm" disabled={busy} onClick={() => void doRenew()}>
              <RefreshCw className={cn("size-3.5", busy && "animate-spin")} />
              探活续期
            </Button>
            <Button variant="outline" size="sm" disabled={busy} onClick={() => void doQuotaBatch()}>
              <Coins className="size-3.5" />
              额度巡检
            </Button>
            <Button size="sm" disabled={busy} onClick={() => void doKeepalive()}>
              <Power className="size-3.5" />
              保活
            </Button>
          </div>
        </header>

        {env && (
          <Card>
            <CardHeader className="pb-3">
              <CardTitle className="text-base">环境</CardTitle>
              <CardDescription>
                {env.installed
                  ? `已检测到客户端：${env.exePath}`
                  : "未检测到客户端，请在设置中手动指定安装路径。"}
              </CardDescription>
            </CardHeader>
            <CardContent className="flex flex-wrap items-center gap-2 text-xs">
              <Badge variant={env.running ? "secondary" : "outline"}>
                {env.running ? "运行中" : "未运行"}
              </Badge>
              <Badge variant="outline">快照 {env.snapshotCount} 个</Badge>
              {lastKeepaliveAt && <Badge variant="outline">上次保活 {lastKeepaliveAt}</Badge>}
              <span className="font-mono text-muted-foreground">{env.dataDir}</span>
            </CardContent>
          </Card>
        )}

        <section className="space-y-3">
          <div className="flex items-center justify-between">
            <h2 className="text-sm font-medium">账号（{accounts.length}）</h2>
            <div className="flex items-center gap-2">
              <Button variant="ghost" size="sm" disabled={busy} onClick={() => void load()}>
                <RefreshCw className={cn("size-3.5", busy && "animate-spin")} />
                刷新
              </Button>
              <Button size="sm" disabled={busy} onClick={openNew}>
                <UserPlus className="size-3.5" />
                添加账号
              </Button>
            </div>
          </div>

          {loading ? (
            <div className="space-y-2">
              <Skeleton className="h-20 w-full" />
              <Skeleton className="h-20 w-full" />
            </div>
          ) : accounts.length === 0 ? (
            <Alert>
              <KeyRound className="size-4" />
              <AlertTitle>还没有豆包账号</AlertTitle>
              <AlertDescription>
                点「添加账号」手动录入，或开启本地代理后用豆包客户端访问一次，
                应用会自动抓取凭证并回写。
              </AlertDescription>
            </Alert>
          ) : (
            <div className="space-y-2">
              {accounts.map((account) => (
                <div
                  key={account.userId}
                  className="flex flex-wrap items-center gap-3 rounded-lg border border-border/60 bg-card px-4 py-3"
                >
                  <DoubaoMark size={34} />
                  <div className="min-w-0 flex-1">
                    <div className="flex flex-wrap items-center gap-2">
                      <span className="truncate text-sm font-medium">
                        {account.name ?? account.userId}
                      </span>
                      {sessionBadge(account)}
                      {quotaBadge(account)}
                      {!account.hasTtwid && (
                        <Tooltip>
                          <TooltipTrigger asChild>
                            <Badge
                              variant="outline"
                              className="cursor-help gap-1 border-amber-500/40 text-amber-600 dark:text-amber-400"
                            >
                              <AlertTriangle className="size-3" />
                              缺 ttwid
                            </Badge>
                          </TooltipTrigger>
                          <TooltipContent side="top" className="max-w-xs">
                            对话导出需要 ttwid。开启本地代理后访问一次豆包即可自动抓取。
                          </TooltipContent>
                        </Tooltip>
                      )}
                    </div>
                    <div className="mt-0.5 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-xs text-muted-foreground">
                      <span className="font-mono">{account.userId}</span>
                      {account.note && <span>{account.note}</span>}
                      {account.hasSessionId && <span>凭证 {account.sessionIdMasked}</span>}
                      {account.sessionExpireAt && <span>会话到期 {account.sessionExpireAt}</span>}
                      {account.lastRenewAt && <span>上次续期 {account.lastRenewAt}</span>}
                    </div>
                  </div>
                  <div className="flex shrink-0 flex-wrap items-center gap-1">
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <Button
                          size="sm"
                          variant="ghost"
                          disabled={busy || !account.hasSessionId}
                          onClick={() => void doProbe(account)}
                        >
                          <Stethoscope className="size-3.5" />
                        </Button>
                      </TooltipTrigger>
                      <TooltipContent side="top">探活</TooltipContent>
                    </Tooltip>
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <Button
                          size="sm"
                          variant="ghost"
                          disabled={busy || !account.hasSessionId}
                          onClick={() => void doQuota(account)}
                        >
                          <Coins className="size-3.5" />
                        </Button>
                      </TooltipTrigger>
                      <TooltipContent side="top">查询额度</TooltipContent>
                    </Tooltip>
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <Button size="sm" variant="ghost" disabled={busy} onClick={() => void doBackup(account)}>
                          <Download className="size-3.5" />
                        </Button>
                      </TooltipTrigger>
                      <TooltipContent side="top">备份对话状态</TooltipContent>
                    </Tooltip>
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <Button size="sm" variant="ghost" disabled={busy} onClick={() => void doRestore(account)}>
                          <Upload className="size-3.5" />
                        </Button>
                      </TooltipTrigger>
                      <TooltipContent side="top">恢复对话状态</TooltipContent>
                    </Tooltip>
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <Button
                          size="sm"
                          variant="ghost"
                          disabled={busy || !account.hasSessionId}
                          onClick={() => void doExport(account)}
                        >
                          <FileDown className="size-3.5" />
                        </Button>
                      </TooltipTrigger>
                      <TooltipContent side="top">导出对话（markdown + json）</TooltipContent>
                    </Tooltip>
                    <Button size="sm" variant="outline" disabled={busy} onClick={() => void openEdit(account)}>
                      编辑
                    </Button>
                    <Button size="sm" variant="ghost" disabled={busy} onClick={() => void doDelete(account)}>
                      <Trash2 className="size-3.5 text-destructive" />
                    </Button>
                  </div>
                </div>
              ))}
            </div>
          )}
        </section>

        <Card>
          <CardHeader className="pb-3">
            <CardTitle className="flex items-center gap-2 text-base">
              <MessageSquare className="size-4" />
              关于对话数据
            </CardTitle>
          </CardHeader>
          <CardContent className="space-y-2 text-sm text-muted-foreground">
            <p>
              <strong className="text-foreground">对话正文存在豆包云端</strong>
              ，按账号归属。本地备份的是客户端状态（会话列表缓存、技能配置）；
              恢复并重新登录后，完整历史会从云端重新同步下来。
            </p>
            <Separator />
            <p>
              需要把对话带走时用「导出对话」：它直接调豆包官方接口拉取正文，
              输出 markdown 与 json 到数据目录的 <code className="font-mono">exports/</code> 下。
            </p>
          </CardContent>
        </Card>

        {/* 编辑弹窗 */}
        <Dialog open={editOpen} onOpenChange={setEditOpen}>
          <DialogContent className="sm:max-w-lg">
            <DialogHeader>
              <DialogTitle>{isNew ? "添加豆包账号" : "编辑豆包账号"}</DialogTitle>
              <DialogDescription>
                凭证等价于账号密码，请勿分享。留空表示清除对应凭证。
              </DialogDescription>
            </DialogHeader>
            <div className="space-y-3">
              <div className="space-y-1.5">
                <Label htmlFor="db-uid">账号标识</Label>
                <Input
                  id="db-uid"
                  value={editUid}
                  onChange={(e) => setEditUid(e.target.value)}
                  disabled={!isNew}
                  placeholder="豆包 user_id"
                  className="font-mono text-xs"
                />
              </div>
              <div className="grid grid-cols-2 gap-3">
                <div className="space-y-1.5">
                  <Label htmlFor="db-name">显示名</Label>
                  <Input
                    id="db-name"
                    value={editName}
                    onChange={(e) => setEditName(e.target.value)}
                    placeholder="例如：主号"
                  />
                </div>
                <div className="space-y-1.5">
                  <Label htmlFor="db-note">备注</Label>
                  <Input
                    id="db-note"
                    value={editNote}
                    onChange={(e) => setEditNote(e.target.value)}
                  />
                </div>
              </div>
              <Separator />
              <div className="flex items-center justify-between">
                <span className="text-sm font-medium">会话凭证</span>
                <Button size="sm" variant="outline" onClick={() => void fillFromCapture()}>
                  <Download className="size-3.5" />
                  从代理抓包填充
                </Button>
              </div>
              {!credPrefilled && (
                <Alert variant="destructive">
                  <AlertTriangle className="size-4" />
                  <AlertTitle>凭证未能读取</AlertTitle>
                  <AlertDescription>
                    已在下方清空。<strong className="text-foreground">空值提交会清除该账号的凭证</strong>
                    ，请先点「从代理抓包填充」或手动填入后再保存。
                  </AlertDescription>
                </Alert>
              )}
              <div className="space-y-1.5">
                <Label htmlFor="db-session">sessionid</Label>
                <Input
                  id="db-session"
                  value={editSession}
                  onChange={(e) => setEditSession(e.target.value)}
                  className="font-mono text-xs"
                />
              </div>
              <div className="space-y-1.5">
                <Label htmlFor="db-guard">sid_guard</Label>
                <Input
                  id="db-guard"
                  value={editGuard}
                  onChange={(e) => setEditGuard(e.target.value)}
                  className="font-mono text-xs"
                />
              </div>
              <div className="space-y-1.5">
                <Label htmlFor="db-ttwid">ttwid</Label>
                <Input
                  id="db-ttwid"
                  value={editTtwid}
                  onChange={(e) => setEditTtwid(e.target.value)}
                  className="font-mono text-xs"
                />
                <p className="text-xs text-muted-foreground">导出对话需要 ttwid。</p>
              </div>
            </div>
            <DialogFooter>
              <Button variant="outline" onClick={() => setEditOpen(false)}>
                取消
              </Button>
              <Button disabled={busy} onClick={() => void saveAccount()}>
                {busy && <Loader2 className="size-3.5 animate-spin" />}
                保存
              </Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>

        {/* 诊断弹窗 */}
        <Dialog open={diagOpen} onOpenChange={setDiagOpen}>
          <DialogContent className="sm:max-w-lg">
            <DialogHeader>
              <DialogTitle>会话与凭证诊断</DialogTitle>
              <DialogDescription>用于排查「为什么读不到凭证」。</DialogDescription>
            </DialogHeader>
            <pre className="max-h-80 overflow-auto rounded-lg border border-border/60 bg-muted/40 p-3 text-xs">
              {diag ? JSON.stringify(diag, null, 2) : "加载中…"}
            </pre>
            <DialogFooter>
              <Button variant="outline" onClick={() => setDiagOpen(false)}>
                关闭
              </Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>

        <QuotaDialog quota={quota} onClose={() => setQuota(null)} />
      </div>
    </TooltipProvider>
  );
}
