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
  Play,
  Power,
  RefreshCw,
  Rocket,
  Stethoscope,
  Trash2,
  Upload,
  UserPlus,
} from "lucide-react";

import { DoubaoInsights } from "@/components/doubao-insights";
import { DoubaoMark } from "@/components/product-marks";
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
import { Switch } from "@/components/ui/switch";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import * as api from "@/lib/api";
import type { AppEnvStatus, DoubaoAccountView, DoubaoQuotaView } from "@/lib/api";
import { cn } from "@/lib/utils";

/** 人类可读的文件体积。 */
function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

/** 距今多少天（解析 `YYYY-MM-DD` 或 ISO 串；解析不出返回 null）。 */
function daysSince(iso: string): number | null {
  const parts = iso.slice(0, 10).split("-").map(Number);
  if (parts.length !== 3 || parts.some((n) => !Number.isFinite(n))) return null;
  return Math.floor((Date.now() - Date.UTC(parts[0], parts[1] - 1, parts[2])) / 86_400_000);
}

/**
 * 到期分层徽标。
 *
 * 后端已经算好 `expiryTier`/`daysLeft`，这里只负责呈现。分层而不是二值
 * （有效/失效）是为了让用户在「还能救」的时候收到提醒 —— 等到失效才发现
 * 就只能重新登录了。
 */
function expiryBadge(account: DoubaoAccountView) {
  const tier = account.expiryTier;
  if (!tier || tier === "unknown" || tier === "fresh") return null;
  const days = account.daysLeft;
  if (tier === "expired") {
    return (
      <Tooltip>
        <TooltipTrigger asChild>
          <Badge variant="destructive" className="cursor-help text-[10px]">
            凭证已过期
          </Badge>
        </TooltipTrigger>
        <TooltipContent side="top" className="max-w-xs">
          该账号的 sessionid 已过服务端到期时间，需要重新登录豆包。
          开启本地代理后访问豆包即可自动抓取新凭证。
        </TooltipContent>
      </Tooltip>
    );
  }
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Badge
          variant="outline"
          className="cursor-help border-amber-500/40 bg-amber-500/10 text-[10px] text-amber-700 dark:text-amber-300"
        >
          {typeof days === "number" && days > 0 ? `${days} 天后到期` : "即将到期"}
        </Badge>
      </TooltipTrigger>
      <TooltipContent side="top" className="max-w-xs">
        会话临近到期，建议跑一次保活触发服务端滑动续期。
      </TooltipContent>
    </Tooltip>
  );
}

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

  // 编辑弹窗三条回填路径（openEdit 的明文回填、fillFromCapture 的抓包回填）共用的
  // 请求序号。两条路径会真并发：打开弹窗时明文凭证还在路上，用户已经点了
  // 「从代理抓包填充」—— 先回来的那个会被后回来的整组覆盖（三个输入框是一起写的），
  // 用户看到「已填入抓包凭证」的成功 toast，框里却是另一个账号的凭证，
  // 点保存就把错误凭证写进该账号、原本正确的明文被覆盖丢失。
  // 后发起的写入权更高：每次发起前自增，回来时序号不匹配就丢弃。
  const editFillSeqRef = useRef(0);

  // 到期/未保活提醒只弹一次（否则每次轮询都会重新弹）
  const expiryWarnedRef = useRef("");
  const staleKeepaliveWarnedRef = useRef(false);
  // 运维健康史（趋势 + 健康卡）
  const [history, setHistory] = useState<api.DoubaoHistory | null>(null);
  // 删除确认（含「是否同时删除快照」选项）
  const [deleteTarget, setDeleteTarget] = useState<DoubaoAccountView | null>(null);
  const [deleteSnapshotToo, setDeleteSnapshotToo] = useState(true);
  // 豆包应用设置
  const [settings, setSettings] = useState<api.DoubaoAppSettings | null>(null);

  const load = useCallback(async () => {
    try {
      const [list, envStatus, history] = await Promise.all([
        api.doubaoListAccounts(),
        api.appEnvCheck("Doubao"),
        // 健康史读失败不该让整页报错：它只是概览区，账号列表才是主体
        api.doubaoHistory().catch(() => null),
      ]);
      setAccounts(list.accounts);
      setLastKeepaliveAt(list.lastKeepaliveAt);
      setEnv(envStatus);
      if (history) setHistory(history);
      // 设置单独读：读不到就用默认值渲染，不该拦住账号列表
      api.doubaoGetSettings().then(setSettings).catch(() => setSettings(null));
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // 到期提醒：进页面时对临期/过期/长期未保活各提示一次。
  // 用 ref 记录已提示过的 key，避免每次轮询都重新弹一遍。
  useEffect(() => {
    if (loading || accounts.length === 0) return;
    const soon = accounts.filter((a) => a.expiryTier === "soon");
    const expired = accounts.filter(
      (a) => a.expiryTier === "expired" || a.sessionState === "expired",
    );
    const key = `${expired.length}-${soon.length}`;
    if (expiryWarnedRef.current === key) return;
    expiryWarnedRef.current = key;

    if (expired.length > 0) {
      toast.error(
        `${expired.length} 个账号的会话已到期，需要重新登录豆包并抓取新凭证`,
        { duration: 8000 },
      );
    }
    if (soon.length > 0) {
      const minDays = Math.min(
        ...soon.map((a) => (typeof a.daysLeft === "number" ? a.daysLeft : 7)),
      );
      toast.warning(
        `${soon.length} 个账号的会话将在 ${minDays} 天内到期，建议先跑一次保活`,
        { duration: 8000 },
      );
    }
  }, [accounts, loading]);

  // 超过 25 天未保活单独提醒：会话约 30 天到期，这是最后的补救窗口
  useEffect(() => {
    if (loading || !lastKeepaliveAt || staleKeepaliveWarnedRef.current) return;
    const days = daysSince(lastKeepaliveAt);
    if (days !== null && days > 25) {
      staleKeepaliveWarnedRef.current = true;
      toast.warning(`已 ${days} 天没有保活，豆包会话约 30 天到期，建议现在跑一次`, {
        duration: 10000,
      });
    }
  }, [lastKeepaliveAt, loading]);

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
    const seq = ++editFillSeqRef.current;
    try {
      // 列表里的凭证是掩码，编辑需要明文回填
      const cred = await api.doubaoGetCredential(account.userId);
      // 期间用户可能已经点了「从代理抓包填充」，别覆盖它填进去的值。
      if (seq !== editFillSeqRef.current) return;
      setEditSession(cred.sessionId ?? "");
      setEditGuard(cred.sidGuard ?? "");
      setEditTtwid(cred.ttwid ?? "");
      setCredPrefilled(true);
    } catch (e) {
      if (seq !== editFillSeqRef.current) return;
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
    const seq = ++editFillSeqRef.current;
    try {
      const captured = await api.doubaoCapturedCredential();
      if (seq !== editFillSeqRef.current) return;
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
      if (seq !== editFillSeqRef.current) return;
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

  const doDelete = async (account: DoubaoAccountView, deleteSnapshot: boolean) => {
    setBusy(true);
    try {
      const res = await api.doubaoDeleteAccount(account.userId, deleteSnapshot);
      toast.success(
        res.snapshotRemoved ? "已删除账号及其登录态快照" : "已删除账号",
      );
      setDeleteTarget(null);
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  /**
   * 写入一项豆包设置。
   *
   * 乐观更新 + 失败回滚：开关必须在点击瞬间就动，否则用户会以为没点到而连点。
   */
  const saveSetting = async <K extends keyof api.DoubaoAppSettings>(
    key: K,
    value: api.DoubaoAppSettings[K],
  ) => {
    const previous = settings;
    setSettings((s) => (s ? { ...s, [key]: value } : s));
    try {
      setSettings(await api.doubaoSetSetting(key, value));
      toast.success("设置已保存");
    } catch (e) {
      setSettings(previous);
      toast.error(api.asError(e));
    }
  };

  const doOpenAsAccount = async (account: DoubaoAccountView) => {    setBusy(true);
    try {
      const res = await api.doubaoOpenAsAccount(account.userId);
      if (res.ok) toast.success(res.message || `已以 ${account.name ?? account.userId} 打开豆包`);
      else toast.error(res.error || "打开失败");
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
      // 带上自动回退：HTTP 路径全失败时后端会拉起客户端保活，
      // 这比让用户自己想到「那试试保活」有用得多
      const res = await api.doubaoRenew(false, true);
      const parts = [`成功 ${res.ok}`];
      if (res.expired) parts.push(`失效 ${res.expired}`);
      if (res.skipped) parts.push(`跳过 ${res.skipped}`);
      if (res.errors) parts.push(`错误 ${res.errors}`);
      if (res.fallbackUsed) {
        // 回退成功也算「救回来了」，但要让用户知道走的不是常规路径
        toast.info(res.fallbackMessage ?? "HTTP 续期失败，已回退到客户端保活", {
          duration: 8000,
        });
      } else if (res.expired > 0 || res.errors > 0) {
        toast.warning(`探活完成：${parts.join(" · ")}`);
      } else {
        toast.success(`探活完成：${parts.join(" · ")}`);
      }
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const doLaunch = async () => {
    setBusy(true);
    try {
      // 零副作用：不切账号、不改快照，只是打开客户端
      const res = await api.appLaunch("doubao");
      toast.success(res.message || "已启动豆包客户端");
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
            <Button variant="outline" size="sm" disabled={busy} onClick={() => void doLaunch()}>
              <Play className="size-3.5" />
              打开客户端
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

        <Card>
          <CardHeader className="pb-3">
            <CardTitle className="text-base">快照与端点</CardTitle>
            <CardDescription>
              控制切换账号时保存哪些数据。快照在切换前保存、切换时恢复。
            </CardDescription>
          </CardHeader>
          <CardContent className="space-y-3">
            <label className="flex items-start gap-3">
              <Switch
                checked={settings?.doubao_snapshot_include_idb ?? false}
                disabled={busy || settings === null}
                onCheckedChange={(v) => void saveSetting("doubao_snapshot_include_idb", v)}
                className="mt-0.5"
              />
              <div className="space-y-0.5 text-sm">
                <div>快照包含 IndexedDB</div>
                <p className="text-xs text-muted-foreground">
                  豆包的会话与草稿主要存在 IndexedDB 里。开启后快照更完整、切换后
                  页面状态几乎无损，但体积会大一个量级、保存与恢复都明显变慢。
                  只在意登录态时保持关闭即可。
                </p>
              </div>
            </label>
            <Separator />
            <div className="space-y-1 text-xs text-muted-foreground">
              <div className="flex items-center gap-2">
                <span>数据目录</span>
                <span className="font-mono">{env?.dataDir ?? "（未检测）"}</span>
              </div>
              <p>
                客户端安装路径在设置页配置；此处展示的是实际读取到的用户数据目录，
                账号快照即保存在该目录下的备份区。
              </p>
            </div>
          </CardContent>
        </Card>

        {/* 运维概览：额度趋势 + 健康卡 + 最近记录 */}
        {history && (
          <DoubaoInsights
            trend={history.trend}
            health={history.health}
            events={history.events}
            lastKeepaliveAt={history.health.lastKeepaliveAt ?? lastKeepaliveAt}
          />
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
                        {account.orphanSnapshot && (
                          <span className="ml-1 text-xs font-normal text-muted-foreground">
                            （仅剩快照）
                          </span>
                        )}
                      </span>
                      {sessionBadge(account)}
                      {expiryBadge(account)}
                      {quotaBadge(account)}
                      {account.hasSnapshot && typeof account.sizeBytes === "number" && (
                        <Tooltip>
                          <TooltipTrigger asChild>
                            <Badge variant="outline" className="cursor-help text-[10px]">
                              {formatSize(account.sizeBytes)} · {account.fileCount ?? 0} 文件
                            </Badge>
                          </TooltipTrigger>
                          <TooltipContent side="top">
                            登录态快照
                            {account.lastModified
                              ? ` · 最后修改 ${new Date(account.lastModified * 1000).toLocaleString()}`
                              : ""}
                          </TooltipContent>
                        </Tooltip>
                      )}
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
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <Button
                          size="sm"
                          variant="outline"
                          disabled={busy || !account.hasSnapshot}
                          onClick={() => void doOpenAsAccount(account)}
                        >
                          <Rocket className="size-3.5" />
                          打开
                        </Button>
                      </TooltipTrigger>
                      <TooltipContent side="top">
                        {account.hasSnapshot
                          ? "以该账号打开豆包（恢复快照后拉起客户端）"
                          : "该账号还没有登录态快照，请先保存当前登录态"}
                      </TooltipContent>
                    </Tooltip>
                    <Button size="sm" variant="outline" disabled={busy} onClick={() => void openEdit(account)}>
                      编辑
                    </Button>
                    <Button
                      size="sm"
                      variant="ghost"
                      disabled={busy}
                      onClick={() => {
                        setDeleteSnapshotToo(true);
                        setDeleteTarget(account);
                      }}
                    >
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

        {/*
          删除确认用 Dialog 而不是 window.confirm：
          这里有一个必须让用户看到的选项（是否连快照一起删），
          原生 confirm 只能展示一行纯文本、无法承载复选框。
        */}
        <Dialog open={deleteTarget !== null} onOpenChange={(o) => !o && setDeleteTarget(null)}>
          <DialogContent className="max-w-md">
            <DialogHeader>
              <DialogTitle>
                删除账号 {deleteTarget?.name ?? deleteTarget?.userId}？
              </DialogTitle>
              <DialogDescription>
                删除后该账号的凭证会从账号库移除，此操作无法撤销。
              </DialogDescription>
            </DialogHeader>
            <div className="space-y-3">
              <label className="flex items-start gap-3 rounded-lg border border-border/60 px-3 py-2.5">
                <Checkbox
                  checked={deleteSnapshotToo}
                  disabled={!deleteTarget?.hasSnapshot}
                  onCheckedChange={(v) => setDeleteSnapshotToo(v === true)}
                  className="mt-0.5"
                />
                <div className="space-y-0.5 text-sm">
                  <div>
                    同时删除登录态快照
                    {deleteTarget?.hasSnapshot && typeof deleteTarget.sizeBytes === "number"
                      ? `（${formatSize(deleteTarget.sizeBytes)}）`
                      : ""}
                  </div>
                  <p className="text-xs text-muted-foreground">
                    {deleteTarget?.hasSnapshot
                      ? "不勾选则保留快照，之后仍可「以该账号打开」。"
                      : "该账号没有快照，此项不可选。"}
                  </p>
                </div>
              </label>
            </div>
            <DialogFooter>
              <Button variant="outline" onClick={() => setDeleteTarget(null)}>
                取消
              </Button>
              <Button
                variant="destructive"
                disabled={busy}
                onClick={() => deleteTarget && void doDelete(deleteTarget, deleteSnapshotToo)}
              >
                {busy && <Loader2 className="size-3.5 animate-spin" />}
                删除
              </Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>
      </div>
    </TooltipProvider>
  );
}
