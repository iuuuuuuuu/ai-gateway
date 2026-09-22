import { useCallback, useEffect, useRef, useState } from "react";
import { toast } from "sonner";
import {
  AlertTriangle,
  ArrowLeftRight,
  CalendarCheck,
  Coins,
  Download,
  Fingerprint,
  KeyRound,
  Loader2,
  MonitorSmartphone,
  Pencil,
  Play,
  RefreshCw,
  RotateCcw,
  Save,
  ShieldCheck,
  Trash2,
  UserPlus,
  Users,
  Zap,
} from "lucide-react";

import { AccountGroupsDialog, GroupSelect, groupColorClass } from "@/components/account-groups-dialog";
import { CheckinTrends } from "@/components/checkin-trends";
import { CreditCell, formatJwtHours, payIdentityClass } from "@/components/credit-cell";
import { TraeCreditsPanel } from "@/components/trae-credits-panel";
import { TraeMark } from "@/components/product-marks";
import { TraeOAuthDialog } from "@/components/trae-oauth-dialog";
import { TraeTransferDialog } from "@/components/trae-transfer-dialog";
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
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import * as api from "@/lib/api";
import type {
  AccountGroup,
  AppEnvStatus,
  SnapshotItem,
  TraeAccountMeta,
  TraeCheckinResult,
  TraeDiscoveredAccount,
} from "@/lib/api";
import { cn } from "@/lib/utils";

/** 两个 Trae 应用。两者共用同一账号库，只是登录态互相独立。 */
const TRAE_APPS = [
  { kind: "TraeWork", label: "Trae Work", variant: "work" as const },
  { kind: "Trae", label: "Trae", variant: "cn" as const },
];

/** 人类可读的文件体积（快照大小列用）。 */
function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

/** JWT 状态的展示样式。 */function jwtBadge(status: TraeAccountMeta["jwtStatus"], hours: number | null) {
  const label =
    status === "ok"
      ? "有效"
      : status === "warn"
        ? "即将过期"
        : status === "expired"
          ? "已过期"
          : "未知";
  const variant =
    status === "ok" ? "secondary" : status === "warn" ? "outline" : "destructive";
  const tip =
    hours == null
      ? "无法从 JWT 解析过期时间"
      : status === "ok"
        ? `剩余约 ${Math.floor(hours / 24)} 天`
        : status === "warn"
          ? `仅剩约 ${hours.toFixed(1)} 小时，请尽快重新登录`
          : "JWT 已过期，需要重新登录";
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Badge variant={variant as never} className="cursor-help">
          {label}
        </Badge>
      </TooltipTrigger>
      <TooltipContent side="top">{tip}</TooltipContent>
    </Tooltip>
  );
}

/** 冷却状态展示。 */
function cooldownBadge(cooldown: TraeAccountMeta["cooldown"]) {
  const until = cooldown?.until;
  if (!until || until <= Date.now() / 1000) return null;
  const type = cooldown?.type ?? "";
  // 永久失效（SessionDead）用一个大但有限的时间戳表示，单独识别以免显示成「几万天后」
  const permanent = until > 9_000_000_000;
  const remaining = permanent ? "" : `（剩 ${Math.ceil((until - Date.now() / 1000) / 60)} 分钟）`;
  const label = permanent
    ? "会话失效"
    : type === "PlanLimit"
      ? "套餐限制"
      : type === "SoftRate"
        ? "限流冷却"
        : "冷却中";
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Badge variant="outline" className="cursor-help gap-1 border-amber-500/40 text-amber-600 dark:text-amber-400">
          <AlertTriangle className="size-3" />
          {label}
          {remaining}
        </Badge>
      </TooltipTrigger>
      <TooltipContent side="top" className="max-w-xs">
        {cooldown?.reason || "该账号暂时不可用"}
        {permanent ? "；需重新登录该账号" : ""}
      </TooltipContent>
    </Tooltip>
  );
}

function AccountRow({
  account,
  groups,
  checked,
  onToggleCheck,
  onDelete,
  onClearCooldown,
  onResetDevice,
  onSwitch,
  onSaveLogin,
  onEdit,
  onRefresh,
  onViewJwt,
  onMoveGroup,
  busy,
}: {
  account: TraeAccountMeta;
  groups: AccountGroup[];
  checked: boolean;
  onToggleCheck: () => void;
  onDelete: () => void;
  onClearCooldown: () => void;
  onResetDevice: () => void;
  onSwitch: () => void;
  onSaveLogin: () => void;
  onEdit: () => void;
  onRefresh: () => void;
  onViewJwt: () => void;
  onMoveGroup: (groupId: string | null) => void;
  busy: boolean;
}) {
  const group = groups.find((g) => g.id === account.group);
  return (
    <div className="flex flex-wrap items-center gap-3 rounded-lg border border-border/60 bg-card px-4 py-3">
      <Checkbox
        checked={checked}
        onCheckedChange={onToggleCheck}
        aria-label={`选择 ${account.name}`}
        className="shrink-0"
      />
      <TraeMark size={34} variant="work" />
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <span className="truncate text-sm font-medium">{account.name}</span>
          {jwtBadge(account.jwtStatus, account.jwtExpHours)}
          {group && (
            <Badge variant="outline" className={cn("text-[10px]", groupColorClass(group.color))}>
              {group.name}
            </Badge>
          )}
          {account.payIdentity && (
            <Badge variant="outline" className={cn("text-[10px]", payIdentityClass(account.payIdentity))}>
              {account.payIdentity}
            </Badge>
          )}
          {cooldownBadge(account.cooldown)}
          {account.refreshTokenInvalid && (
            <Badge variant="destructive" className="text-[10px]">
              refresh token 已失效
            </Badge>
          )}
        </div>
        <div className="mt-0.5 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-xs text-muted-foreground">
          <span className="font-mono">{account.userId}</span>
          <span className="inline-flex items-center gap-1">
            <Fingerprint className="size-3" />
            {account.deviceIdMasked}
          </span>
          <span>JWT 剩余 {formatJwtHours(account.jwtExpHours)}</span>
          <CreditCell
            userId={account.userId}
            total={account.creditsTotal}
            payIdentity={account.payIdentity}
          />
          {!account.hasRefreshToken && <span>无 refresh token（无法自动续期）</span>}
        </div>
      </div>
      <div className="flex shrink-0 flex-wrap items-center gap-1.5">
        <GroupSelect
          groups={groups}
          value={account.group ?? null}
          disabled={busy}
          onChange={onMoveGroup}
        />
        <Button size="sm" variant="outline" disabled={busy} onClick={onSwitch}>
          <MonitorSmartphone className="size-3.5" />
          切换
        </Button>
        <Button size="sm" variant="outline" disabled={busy} onClick={onSaveLogin}>
          <Save className="size-3.5" />
          保存登录态
        </Button>
        <Tooltip>
          <TooltipTrigger asChild>
            <Button
              size="sm"
              variant="ghost"
              disabled={busy || !account.canRefresh}
              onClick={onRefresh}
            >
              <Zap className="size-3.5" />
            </Button>
          </TooltipTrigger>
          <TooltipContent side="top">
            {account.canRefresh
              ? "刷新 JWT（ExchangeToken，剩余不足 48 小时才实际请求）"
              : "该账号没有可用的 refresh token"}
          </TooltipContent>
        </Tooltip>
        <Tooltip>
          <TooltipTrigger asChild>
            <Button size="sm" variant="ghost" disabled={busy} onClick={onEdit}>
              <Pencil className="size-3.5" />
            </Button>
          </TooltipTrigger>
          <TooltipContent side="top">编辑昵称 / 更换登录态</TooltipContent>
        </Tooltip>
        <Tooltip>
          <TooltipTrigger asChild>
            <Button size="sm" variant="ghost" disabled={busy} onClick={onViewJwt}>
              <KeyRound className="size-3.5" />
            </Button>
          </TooltipTrigger>
          <TooltipContent side="top">查看完整 JWT</TooltipContent>
        </Tooltip>
        <Tooltip>
          <TooltipTrigger asChild>
            <Button size="sm" variant="ghost" disabled={busy} onClick={onResetDevice}>
              <RotateCcw className="size-3.5" />
            </Button>
          </TooltipTrigger>
          <TooltipContent side="top">重置该账号的设备指纹</TooltipContent>
        </Tooltip>
        {account.cooldown?.until && account.cooldown.until > Date.now() / 1000 && (
          <Tooltip>
            <TooltipTrigger asChild>
              <Button size="sm" variant="ghost" disabled={busy} onClick={onClearCooldown}>
                <RefreshCw className="size-3.5" />
              </Button>
            </TooltipTrigger>
            <TooltipContent side="top">清除冷却</TooltipContent>
          </Tooltip>
        )}
        <Button size="sm" variant="ghost" disabled={busy} onClick={onDelete}>
          <Trash2 className="size-3.5 text-destructive" />
        </Button>
      </div>
    </div>
  );
}

function SnapshotList({
  appKind,
  appLabel,
  onChanged,
}: {
  appKind: string;
  appLabel: string;
  onChanged: () => void;
}) {
  const [snapshots, setSnapshots] = useState<SnapshotItem[]>([]);
  const [currentUserId, setCurrentUserId] = useState("");
  const [markerStale, setMarkerStale] = useState(false);
  const [loading, setLoading] = useState(true);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const res = await api.listSnapshots(appKind);
      setSnapshots(res.snapshots);
      setCurrentUserId(res.currentUserId);
      setMarkerStale(res.markerStale);
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setLoading(false);
    }
  }, [appKind]);

  useEffect(() => {
    void load();
  }, [load]);

  const remove = async (userId: string) => {
    try {
      await api.deleteSnapshot(appKind, userId);
      toast.success(`已删除 ${userId} 的登录态快照`);
      await load();
      onChanged();
    } catch (e) {
      toast.error(api.asError(e));
    }
  };

  if (loading) {
    return (
      <div className="space-y-2">
        <Skeleton className="h-12 w-full" />
        <Skeleton className="h-12 w-full" />
      </div>
    );
  }
  if (snapshots.length === 0) {
    return (
      <p className="rounded-lg border border-dashed border-border/60 px-4 py-6 text-center text-sm text-muted-foreground">
        还没有 {appLabel} 的登录态快照。在客户端登录账号后点「保存登录态」即可创建。
      </p>
    );
  }
  return (
    <div className="space-y-2">
      <p className="text-xs text-muted-foreground">
        当前登录账号：
        <span className="font-mono">{currentUserId || "未知"}</span>
        {currentUserId ? "（切换时会把现场登录态写回它）" : ""}
      </p>
      {markerStale && (
        <Alert>
          <AlertTriangle className="size-4" />
          <AlertTitle>「当前登录」可能已过期</AlertTitle>
          <AlertDescription>
            客户端里的登录态比这里的记录更新 —— 通常是直接在客户端里换过账号，
            没有走本应用的切换。此时下面的「当前登录」徽章可能标在错的账号上；
            点一次「保存登录态」或「切换」即可让记录追上现场。
          </AlertDescription>
        </Alert>
      )}
      {snapshots.map((snap) => (
        <div
          key={snap.userId}
          className="flex items-center gap-3 rounded-lg border border-border/60 px-4 py-2.5"
        >
          <div className="min-w-0 flex-1">
            <div className="flex items-center gap-2">
              <span className="truncate font-mono text-sm">{snap.userId}</span>
              {snap.isCurrent && (
                <Badge variant="secondary" className="text-[10px]">
                  当前登录
                </Badge>
              )}
              {!snap.hasMeta && (
                <Tooltip>
                  <TooltipTrigger asChild>
                    <Badge variant="outline" className="cursor-help text-[10px]">
                      旧格式
                    </Badge>
                  </TooltipTrigger>
                  <TooltipContent side="top">
                    该快照由旧版本创建，缺少版本元数据，恢复时不做版本校验
                  </TooltipContent>
                </Tooltip>
              )}
            </div>
            {(snap.modifiedAt || snap.sizeBytes > 0) && (
              <span className="flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
                {snap.modifiedAt && (
                  <span>{new Date(snap.modifiedAt * 1000).toLocaleString("zh-CN")}</span>
                )}
                {snap.sizeBytes > 0 && (
                  <span>
                    {formatBytes(snap.sizeBytes)} · {snap.fileCount} 个文件
                  </span>
                )}
              </span>
            )}
          </div>
          <Button size="sm" variant="ghost" onClick={() => void remove(snap.userId)}>
            <Trash2 className="size-3.5 text-destructive" />
          </Button>
        </div>
      ))}
    </div>
  );
}

/**
 * 汇总一次本机导入的结果并提示。
 *
 * 三个计数分开报是有意义的：「新增」是发现了新账号，「更新」只是凭证变新，
 * 「跳过」表示账号在本机但那份 JWT 比账号库里的更旧（防降级拦下了）——
 * 后两种情况下用户不该以为「什么都没发生」。
 */
function reportImport(summary: api.TraeImportSummary) {
  if (summary.error) {
    toast.warning(`未能读取本机登录态：${summary.error}`);
    return;
  }
  const parts: string[] = [];
  if (summary.appended) parts.push(`新增 ${summary.appended}`);
  if (summary.updated) parts.push(`更新 ${summary.updated}`);
  if (summary.skipped) parts.push(`跳过 ${summary.skipped}（本机凭证较旧）`);
  if (parts.length === 0) {
    toast.info("本机登录态与账号库一致，无需更新。");
    return;
  }
  toast.success(`已从本机导入：${parts.join(" · ")}`);
}

export default function TraePage() {
  const [activeApp, setActiveApp] = useState(TRAE_APPS[0]);
  const [accounts, setAccounts] = useState<TraeAccountMeta[]>([]);
  const [env, setEnv] = useState<AppEnvStatus | null>(null);
  const [discovered, setDiscovered] = useState<TraeDiscoveredAccount[]>([]);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [checkinResult, setCheckinResult] = useState<TraeCheckinResult | null>(null);
  const [addOpen, setAddOpen] = useState(false);
  const [addJwt, setAddJwt] = useState("");
  const [addName, setAddName] = useState("");
  const [addRefresh, setAddRefresh] = useState("");
  // ---- 分组 ----
  const [groups, setGroups] = useState<AccountGroup[]>([]);
  const [groupsOpen, setGroupsOpen] = useState(false);
  // ---- OAuth 登录 ----
  const [oauthOpen, setOauthOpen] = useState(false);
  // ---- 导出 / 导入 ----
  const [transferOpen, setTransferOpen] = useState(false);
  // ---- 签到范围选择 ----
  const [selected, setSelected] = useState<string[]>([]);
  /** 签到实时进度文案（重试轮会等 30/90 秒，没有进度用户会以为卡死）。 */
  const [checkinProgress, setCheckinProgress] = useState("");
  /** 签到跑完后自增，触发趋势面板重新拉取。 */
  const [checkinTrendKey, setCheckinTrendKey] = useState(0);
  // ---- 编辑 ----
  const [editing, setEditing] = useState<TraeAccountMeta | null>(null);
  const [editName, setEditName] = useState("");
  const [editJwt, setEditJwt] = useState("");
  const [originalJwt, setOriginalJwt] = useState("");
  const [editJwtInfo, setEditJwtInfo] = useState<api.TraeJwtInfo | null>(null);
  // ---- JWT 查看 ----
  const [jwtView, setJwtView] = useState<{
    account: TraeAccountMeta;
    jwt: string | null;
    loading: boolean;
  } | null>(null);
  // 切换前记录的当前账号：作为防误覆盖守卫传给后端
  const expectedUidRef = useRef<string>("");

  const loadGroups = useCallback(async () => {
    try {
      const res = await api.groupsList("Trae");
      setGroups(res.groups);
    } catch {
      // 分组读不到不该让整页报错：账号列表本身仍然可用
      setGroups([]);
    }
  }, []);

  const load = useCallback(async () => {
    try {
      const [list, envStatus] = await Promise.all([
        api.traeListAccounts(),
        api.appEnvCheck(activeApp.kind),
      ]);
      setAccounts(list.accounts);
      setEnv(envStatus);
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setLoading(false);
    }
  }, [activeApp.kind]);

  useEffect(() => {
    setLoading(true);
    void load();
    void loadGroups();
  }, [load, loadGroups]);

  /**
   * 签到进度：桌面端走 Tauri 事件，webui 走 HTTP 轮询。
   *
   * 重试轮会等 30 + 90 秒，没有进度反馈用户会以为界面卡死而强退 ——
   * 强退会让正在跑的签到中断，白等一场。
   */
  useEffect(() => {
    if (api.isWebui()) {
      const timer = window.setInterval(() => {
        void api
          .switchProgress()
          .then((p) => {
            if (p.progress) setCheckinProgress(p.progress);
          })
          .catch(() => {});
      }, 1000);
      return () => window.clearInterval(timer);
    }
    let unlisten: (() => void) | undefined;
    void import("@tauri-apps/api/event").then(({ listen }) =>
      listen<{ stage: string; data: string }>("checkin-progress", (e) => {
        const { stage, data } = e.payload;
        // account 行是「序号|uid|状态|说明」的打包格式，取说明部分展示
        if (stage === "checkin:account") {
          const parts = data.split("|");
          setCheckinProgress(parts[3] ?? data);
        } else {
          setCheckinProgress(data);
        }
      }).then((fn) => {
        unlisten = fn;
      }),
    );
    return () => {
      unlisten?.();
    };
  }, []);

  // 刷新当前登录账号（守卫依据），页面切换应用时也要重新读
  useEffect(() => {
    let disposed = false;
    api
      .currentAccount(activeApp.kind)
      .then((res) => {
        if (!disposed) expectedUidRef.current = res.userId;
      })
      .catch(() => {
        // 读不到不算错误：守卫会退化为「跳过写回来源槽」，不影响切换本身
      });
    return () => {
      disposed = true;
    };
  }, [activeApp.kind, accounts.length]);

  const runAction = async (
    action: api.SwitchActionName,
    userId?: string,
    successMessage?: string,
  ) => {
    setBusy(true);
    try {
      const res = await api.switchAction({
        action,
        targetApp: activeApp.kind,
        userId: userId ?? null,
        expectedCurrentUid: expectedUidRef.current || null,
      });
      toast.success(successMessage ?? res.message);
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const doCheckin = async (userIds?: string[]) => {
    setBusy(true);
    setCheckinProgress("");
    try {
      // 选中若干账号时只签这些；一个都不选等同于全签（与后端语义一致）
      const res = await api.traeCheckinRun(
        userIds && userIds.length > 0 ? userIds : undefined,
      );
      setCheckinResult(res);
      const parts = [`成功 ${res.ok}`];
      if (res.already) parts.push(`已签 ${res.already}`);
      if (res.skipped) parts.push(`跳过 ${res.skipped}`);
      if (res.failed) parts.push(`失败 ${res.failed}`);
      if (res.failed > 0) toast.warning(`签到完成：${parts.join(" · ")}`);
      else toast.success(`签到完成：${parts.join(" · ")}`);
      await load();
      // 趋势面板要重新拉一次，否则刚跑完的签到不会出现在图上
      setCheckinTrendKey((k) => k + 1);
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const doLaunch = async () => {
    setBusy(true);
    try {
      // 零副作用：不动任何快照槽，只是把客户端开起来
      const res = await api.appLaunch(activeApp.kind);
      toast.success(res.message || "已启动客户端");
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const doDiscover = async () => {
    setBusy(true);
    try {
      // 发现即导入：JWT 就在客户端本地 Cookies 里，没必要让用户手贴一遍
      const res = await api.traeDiscoverAndImport();
      setDiscovered(res.accounts);
      if (res.accounts.length === 0) {
        toast.info("未在本机发现 Trae 登录痕迹。请先启动客户端并登录一次。");
      } else {
        reportImport(res.import);
      }
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const doImportLocal = async () => {
    setBusy(true);
    try {
      const res = await api.traeImportLocal();
      reportImport(res.import);
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const doAdd = async () => {
    if (!addJwt.trim()) {
      toast.error("请粘贴 JWT");
      return;
    }
    setBusy(true);
    try {
      const res = await api.traeAddAccount({
        jwt: addJwt.trim(),
        name: addName.trim() || undefined,
        refreshToken: addRefresh.trim() || undefined,
      });
      toast.success(`已保存账号 ${res.account.name}`);
      setAddOpen(false);
      setAddJwt("");
      setAddName("");
      setAddRefresh("");
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const doDelete = async (account: TraeAccountMeta) => {
    if (!window.confirm(`确认删除账号 ${account.name}（${account.userId}）？`)) return;
    setBusy(true);
    try {
      await api.traeDeleteAccount(account.userId);
      toast.success("已删除");
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const doRefreshJwt = async (account: TraeAccountMeta) => {
    setBusy(true);
    try {
      const res = await api.traeRefreshAccount(account.userId);
      // 「暂无需刷新」是正常结果而不是错误，用 info 而不是 success，
      // 免得用户以为刚做了一次有实际效果的刷新
      if (res.message.includes("跳过")) toast.info(res.message);
      else toast.success(res.message);
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const doMoveGroup = async (account: TraeAccountMeta, groupId: string | null) => {
    try {
      await api.groupMove("Trae", account.userId, groupId);
      await load();
      await loadGroups();
    } catch (e) {
      toast.error(api.asError(e));
    }
  };

  /**
   * 打开编辑弹窗。
   *
   * 先预填昵称并立即展示，再异步拉完整 JWT 回填 —— 用户不必等网络往返
   * 就能看到弹窗，而 JWT 输入框随后自己填上。
   */
  const doOpenEdit = async (account: TraeAccountMeta) => {
    setEditing(account);
    setEditName(account.name);
    setEditJwt("");
    setOriginalJwt("");
    setEditJwtInfo(null);
    try {
      const res = await api.traeAccountJwt(account.userId);
      setEditJwt(res.jwt);
      setOriginalJwt(res.jwt);
    } catch {
      // 读不到 JWT 不影响改昵称，留空即可
    }
  };

  const doSaveEdit = async () => {    if (!editing) return;
    const name = editName.trim();
    if (!name) {
      toast.error("昵称不能为空");
      return;
    }
    setBusy(true);
    try {
      await api.traeUpdateAccount({
        userId: editing.userId,
        name,
        // 只在用户真的改了输入框时才提交 JWT，避免把回填的原文再写一遍
        jwt: editJwt.trim() && editJwt.trim() !== originalJwt ? editJwt.trim() : undefined,
      });
      toast.success("账号已更新");
      setEditing(null);
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const doViewJwt = async (account: TraeAccountMeta) => {
    setJwtView({ account, jwt: null, loading: true });
    try {
      const res = await api.traeAccountJwt(account.userId);
      setJwtView({ account, jwt: res.jwt, loading: false });
    } catch (e) {
      setJwtView(null);
      toast.error(api.asError(e));
    }
  };

  const doClearAllCooldowns = async () => {
    setBusy(true);
    try {
      const res = await api.traeClearAllCooldowns();
      toast.success(res.cleared > 0 ? `已清除 ${res.cleared} 条冷却记录` : "当前没有冷却中的账号");
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  // 实时预览粘贴的 JWT（编辑弹窗）——防抖 200ms，避免每敲一个字符打一次后端
  useEffect(() => {
    const text = editJwt.trim();
    if (!text || text === originalJwt) {
      setEditJwtInfo(null);
      return;
    }
    const timer = window.setTimeout(() => {
      api
        .traeJwtParse(text)
        .then(setEditJwtInfo)
        // 预览失败不算错误：用户可能还没粘完
        .catch(() => setEditJwtInfo(null));
    }, 200);
    return () => window.clearTimeout(timer);
  }, [editJwt, originalJwt]);

  const doResetDevice = async (account: TraeAccountMeta) => {
    setBusy(true);
    try {
      const res = await api.traeDeviceInfo(account.userId, true);
      toast.success(`已为 ${account.name} 重新派生设备指纹（${res.deviceId}）`);
      await load();
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
              <TraeMark size={26} variant={activeApp.variant} />
              Trae 账号管理
            </h1>
            <p className="mt-1 text-sm text-muted-foreground">
              Trae Work 与 Trae 共用同一份账号库，但登录态快照互相独立。
            </p>
          </div>
          <div className="flex items-center gap-2">
            <Button variant="outline" size="sm" disabled={busy} onClick={() => void doImportLocal()}>
              <Download className="size-3.5" />
              从本机登录态导入
            </Button>
            <Button variant="outline" size="sm" disabled={busy} onClick={() => void doDiscover()}>
              <UserPlus className="size-3.5" />
              发现本机账号
            </Button>
            <Button variant="outline" size="sm" onClick={() => setGroupsOpen(true)}>
              <Users className="size-3.5" />
              分组管理
            </Button>
            <Button variant="outline" size="sm" disabled={busy} onClick={() => void doLaunch()}>
              <Play className="size-3.5" />
              打开客户端
            </Button>
            <Button variant="outline" size="sm" onClick={() => setTransferOpen(true)}>
              <ArrowLeftRight className="size-3.5" />
              导出/导入
            </Button>
            <Button variant="outline" size="sm" disabled={busy} onClick={() => void doClearAllCooldowns()}>
              <RefreshCw className="size-3.5" />
              清除全部冷却
            </Button>
            <Button size="sm" disabled={busy} onClick={() => setOauthOpen(true)}>
              <ShieldCheck className="size-3.5" />
              OAuth 登录
            </Button>
            <Button size="sm" variant="outline" disabled={busy} onClick={() => setAddOpen(true)}>
              <UserPlus className="size-3.5" />
              粘贴 JWT 添加
            </Button>
          </div>
        </header>

        <Tabs
          value={activeApp.kind}
          onValueChange={(value) => {
            const next = TRAE_APPS.find((app) => app.kind === value);
            if (next) setActiveApp(next);
          }}
        >
          <TabsList>
            {TRAE_APPS.map((app) => (
              <TabsTrigger key={app.kind} value={app.kind} className="gap-2">
                <TraeMark size={16} variant={app.variant} />
                {app.label}
              </TabsTrigger>
            ))}
          </TabsList>

          {TRAE_APPS.map((app) => (
            <TabsContent key={app.kind} value={app.kind} className="space-y-5">
              {/* 环境状态 */}
              {env && (
                <Card>
                  <CardHeader className="pb-3">
                    <CardTitle className="text-base">{app.label} 环境</CardTitle>
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
                    {/* 版本与配置来源：改了 conf 不生效时，这两项是唯一的排查入口 */}
                    {env.version ? (
                      <Badge variant="outline">
                        版本 {env.version}
                        {env.versionSource ? ` · ${env.versionSource}` : ""}
                      </Badge>
                    ) : (
                      <Badge variant="outline">版本未知</Badge>
                    )}
                    <Badge variant={env.dataDirExists ? "outline" : "destructive"}>
                      {env.dataDirExists ? "数据目录存在" : "数据目录不存在"}
                    </Badge>
                    <span className="font-mono text-muted-foreground">{env.dataDir}</span>
                  </CardContent>
                </Card>
              )}

              {/* 签到 */}
              <Card>
                <CardHeader className="flex-row items-center justify-between space-y-0 pb-3">
                  <div>
                    <CardTitle className="text-base">每日签到</CardTitle>
                    <CardDescription>
                      按账号顺序执行，已签的会跳过；失败账号会先按错误类型判定冷却，
                      瞬时故障每 30 秒 / 90 秒各重试一次。
                    </CardDescription>
                  </div>
                  <div className="flex items-center gap-1.5">
                    <Button
                      size="sm"
                      variant="outline"
                      disabled={busy}
                      onClick={() => setSelected(selected.length === accounts.length ? [] : accounts.map((a) => a.userId))}
                    >
                      {selected.length === accounts.length && accounts.length > 0
                        ? "取消全选"
                        : "全选"}
                    </Button>
                    <Button
                      size="sm"
                      disabled={busy || accounts.length === 0}
                      onClick={() => void doCheckin(selected)}
                    >
                      {busy ? (
                        <Loader2 className="size-3.5 animate-spin" />
                      ) : (
                        <CalendarCheck className="size-3.5" />
                      )}
                      {selected.length > 0 ? `签到选中 ${selected.length} 个` : "一键签到"}
                    </Button>
                  </div>
                </CardHeader>
                {busy && checkinProgress && (
                  <CardContent className="pt-0">
                    <div className="flex items-center gap-2 rounded-md border border-border/60 bg-muted/40 px-3 py-2 text-xs text-muted-foreground">
                      <Loader2 className="size-3.5 shrink-0 animate-spin" />
                      <span className="truncate">{checkinProgress}</span>
                    </div>
                  </CardContent>
                )}
                <CardContent className="space-y-3 pt-0">
                  <Separator />
                  <CheckinTrends refreshKey={checkinTrendKey} />
                </CardContent>
                {checkinResult && (
                  <CardContent className="space-y-2 pt-0">
                    <Separator />
                    <div className="flex flex-wrap gap-2 text-xs">
                      <Badge variant="secondary">成功 {checkinResult.ok}</Badge>
                      <Badge variant="outline">已签 {checkinResult.already}</Badge>
                      {checkinResult.skipped > 0 && (
                        <Badge variant="outline">跳过 {checkinResult.skipped}</Badge>
                      )}
                      {checkinResult.failed > 0 && (
                        <Badge variant="destructive">失败 {checkinResult.failed}</Badge>
                      )}
                    </div>
                    <div className="space-y-1">
                      {checkinResult.results
                        .filter((r) => r.status !== "success" && r.status !== "already")
                        .map((r) => (
                          <div
                            key={r.userId}
                            className="flex items-start gap-2 rounded border border-border/60 px-3 py-1.5 text-xs"
                          >
                            <span className="shrink-0 font-medium">{r.name}</span>
                            <span className="text-muted-foreground">{r.message}</span>
                          </div>
                        ))}
                    </div>
                  </CardContent>
                )}
              </Card>

              {/* 账号列表 */}
              <section className="space-y-3">
                <div className="flex items-center justify-between">
                  <h2 className="text-sm font-medium">
                    账号（{accounts.length}）
                  </h2>
                  <Button variant="ghost" size="sm" disabled={busy} onClick={() => void load()}>
                    <RefreshCw className={cn("size-3.5", busy && "animate-spin")} />
                    刷新
                  </Button>
                </div>

                {loading ? (
                  <div className="space-y-2">
                    <Skeleton className="h-16 w-full" />
                    <Skeleton className="h-16 w-full" />
                  </div>
                ) : accounts.length === 0 ? (
                  <Alert>
                    <Coins className="size-4" />
                    <AlertTitle>还没有 Trae 账号</AlertTitle>
                    <AlertDescription>
                      点「从本机登录态导入」即可自动读取客户端里已登录的账号
                      （JWT 就在本机 Cookies 中，无需手贴）；也可以「发现本机账号」或手动粘贴。
                    </AlertDescription>
                  </Alert>
                ) : (
                  <div className="space-y-2">
                    {accounts.map((account) => (
                      <AccountRow
                        key={account.userId}
                        account={account}
                        groups={groups}
                        checked={selected.includes(account.userId)}
                        onToggleCheck={() =>
                          setSelected((prev) =>
                            prev.includes(account.userId)
                              ? prev.filter((x) => x !== account.userId)
                              : [...prev, account.userId],
                          )
                        }
                        busy={busy}
                        onSwitch={() => void runAction("Switch", account.userId)}
                        onSaveLogin={() =>
                          void runAction(
                            "SaveCurrentLogin",
                            account.userId,
                            `已把当前登录态保存到 ${account.name}`,
                          )
                        }
                        onResetDevice={() => void doResetDevice(account)}
                        onRefresh={() => void doRefreshJwt(account)}
                        onMoveGroup={(gid) => void doMoveGroup(account, gid)}
                        onEdit={() => void doOpenEdit(account)}
                        onViewJwt={() => void doViewJwt(account)}
                        onClearCooldown={async () => {
                          await api.traeClearCooldown(account.userId);
                          toast.success("已清除冷却");
                          await load();
                        }}
                        onDelete={() => void doDelete(account)}
                      />
                    ))}
                  </div>
                )}
              </section>

              {/* 快照 */}
              <Card>
                <CardHeader className="pb-3">
                  <CardTitle className="text-base">登录态快照</CardTitle>
                  <CardDescription>
                    每个账号一份独立快照。切换时先备份现场、再恢复目标账号的快照，
                    并保留一代备份可回退。
                  </CardDescription>
                </CardHeader>
                <CardContent>
                  <SnapshotList
                    appKind={app.kind}
                    appLabel={app.label}
                    onChanged={() => void load()}
                  />
                </CardContent>
              </Card>

              {/* 本机发现结果 */}
              {discovered.length > 0 && (
                <Card>
                  <CardHeader className="pb-3">
                    <CardTitle className="text-base">本机发现</CardTitle>
                    <CardDescription>
                      uid 由客户端使用痕迹推导，「发现」时会顺带把本机登录态里的 JWT
                      自动导入账号库。标记为「无法确认」的候选
                      <strong className="text-foreground">不会</strong>入池 ——
                      它的 uid 属于账户中心编号体系，与账号库不是同一套编号。
                    </CardDescription>
                  </CardHeader>
                  <CardContent className="space-y-2">
                    {discovered.map((item, index) => (
                      <div
                        key={`${item.userId || item.dcUid}-${index}`}
                        className="flex flex-wrap items-center gap-3 rounded-lg border border-border/60 px-4 py-2.5"
                      >
                        <div className="min-w-0 flex-1">
                          <div className="flex items-center gap-2">
                            <span className="truncate font-mono text-sm">
                              {item.uidConfident ? item.userId : "无法确认账号"}
                            </span>
                            <Badge variant="outline" className="text-[10px]">
                              {item.appLabel}
                            </Badge>
                            {item.inPool && (
                              <Badge variant="secondary" className="text-[10px]">
                                已在账号库
                              </Badge>
                            )}
                            {item.payIdentity && (
                              <Badge variant="outline" className="text-[10px]">
                                {item.payIdentity}
                              </Badge>
                            )}
                          </div>
                          {item.dcUid && (
                            <span className="text-xs text-muted-foreground">
                              账户中心编号 {item.dcUid}
                              {item.uidConfident ? "" : "（不能直接入库）"}
                            </span>
                          )}
                        </div>
                        {item.uidConfident && !item.inPool && (
                          <Button
                            size="sm"
                            variant="outline"
                            disabled={busy}
                            onClick={async () => {
                              // 正常情况下「发现」已自动导入该账号的 JWT；
                              // 走到这里说明本机 Cookies 里没有它的凭证
                              // （例如只在另一个应用登录过、或客户端已清缓存）。
                              // 保留手动入口，让用户能补 JWT / rename / 填 refresh token。
                              setAddName(item.appLabel);
                              setAddOpen(true);
                              toast.info(
                                "本机登录态里没找到该账号的凭证，请手动粘贴 Cloud-IDE-JWT 补充",
                              );
                            }}
                          >
                            手动补充
                          </Button>
                        )}
                      </div>
                    ))}
                  </CardContent>
                </Card>
              )}
            </TabsContent>
          ))}
        </Tabs>

        {/*
          积分趋势放在 Tabs **之外**：账号库是 Trae Work 与 Trae 共用的，
          积分也属于账号而不是某个客户端 —— 放进某个 Tab 里会让人以为
          切换客户端能看到不同的积分。
        */}
        <section className="space-y-3">
          <div>
            <h2 className="flex items-center gap-2 text-sm font-medium">
              <Coins className="size-4" />
              积分趋势
            </h2>
            <p className="mt-1 text-xs text-muted-foreground">
              数据来自本地积分快照，两个客户端共用同一份统计。
            </p>
          </div>
          <TraeCreditsPanel />
        </section>

        <Dialog open={addOpen} onOpenChange={setAddOpen}>
          <DialogContent className="sm:max-w-lg">
            <DialogHeader>
              <DialogTitle>添加 Trae 账号</DialogTitle>
              <DialogDescription>
                粘贴 Cloud-IDE-JWT（可带 `Cloud-IDE-JWT ` 前缀，也可只贴 token 本体）。
                账号 id 会从 JWT 里自动解析。
              </DialogDescription>
            </DialogHeader>
            <div className="space-y-3">
              <div className="space-y-1.5">
                <Label htmlFor="trae-jwt">JWT</Label>
                <Input
                  id="trae-jwt"
                  value={addJwt}
                  onChange={(e) => setAddJwt(e.target.value)}
                  placeholder="Cloud-IDE-JWT eyJ..."
                  className="font-mono text-xs"
                />
              </div>
              <div className="space-y-1.5">
                <Label htmlFor="trae-name">备注名（可选）</Label>
                <Input
                  id="trae-name"
                  value={addName}
                  onChange={(e) => setAddName(e.target.value)}
                  placeholder="例如：主号"
                />
              </div>
              <div className="space-y-1.5">
                <Label htmlFor="trae-refresh">refresh token（可选）</Label>
                <Input
                  id="trae-refresh"
                  value={addRefresh}
                  onChange={(e) => setAddRefresh(e.target.value)}
                  placeholder="填写后可自动续期"
                  className="font-mono text-xs"
                />
              </div>
            </div>
            <DialogFooter>
              <Button variant="outline" onClick={() => setAddOpen(false)}>
                取消
              </Button>
              <Button disabled={busy} onClick={() => void doAdd()}>
                {busy && <Loader2 className="size-3.5 animate-spin" />}
                保存
              </Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>

        {/* 分组管理 */}
        <AccountGroupsDialog
          open={groupsOpen}
          onOpenChange={setGroupsOpen}
          app="Trae"
          onChanged={() => {
            void load();
            void loadGroups();
          }}
        />

        {/* OAuth 授权码登录 */}
        <TraeOAuthDialog
          open={oauthOpen}
          onOpenChange={setOauthOpen}
          onSuccess={() => void load()}
        />

        {/* 导出 / 导入 */}
        <TraeTransferDialog
          open={transferOpen}
          onOpenChange={setTransferOpen}
          accounts={accounts}
          onImported={() => void load()}
        />

        {/* 编辑账号 */}
        <Dialog open={editing !== null} onOpenChange={(o) => !o && setEditing(null)}>
          <DialogContent className="max-w-lg">
            <DialogHeader>
              <DialogTitle>编辑账号</DialogTitle>
              <DialogDescription>
                只改昵称不会触碰登录态。替换 JWT 时必须仍是同一个账号 ——
                换了账号请删除后重新添加，以免把两个账号的记录混在一起。
              </DialogDescription>
            </DialogHeader>
            <div className="space-y-3">
              <div className="space-y-1.5">
                <Label htmlFor="edit-name">昵称</Label>
                <Input
                  id="edit-name"
                  value={editName}
                  maxLength={64}
                  onChange={(e) => setEditName(e.target.value)}
                />
              </div>
              <div className="space-y-1.5">
                <Label htmlFor="edit-jwt">Cloud-IDE-JWT</Label>
                <Input
                  id="edit-jwt"
                  value={editJwt}
                  onChange={(e) => setEditJwt(e.target.value)}
                  placeholder="Cloud-IDE-JWT eyJ..."
                  className="font-mono text-xs"
                />
                {editJwtInfo && !editJwtInfo.valid && (
                  <p className="text-xs text-destructive">
                    解析不出账号 id，请确认粘贴的是完整的 Cloud-IDE-JWT。
                  </p>
                )}
                {editJwtInfo?.valid && (
                  <p className="text-xs text-muted-foreground">
                    账号 <span className="font-mono">{editJwtInfo.userId}</span> · 剩余{" "}
                    {formatJwtHours(editJwtInfo.expHours)}
                  </p>
                )}
              </div>
              <p className="text-xs text-muted-foreground">
                账号 id：<span className="font-mono">{editing?.userId}</span>
              </p>
            </div>
            <DialogFooter>
              <Button variant="outline" onClick={() => setEditing(null)}>
                取消
              </Button>
              <Button disabled={busy || !editName.trim()} onClick={() => void doSaveEdit()}>
                {busy && <Loader2 className="size-3.5 animate-spin" />}
                保存
              </Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>

        {/* 查看完整 JWT */}
        <Dialog open={jwtView !== null} onOpenChange={(o) => !o && setJwtView(null)}>
          <DialogContent className="max-w-lg">
            <DialogHeader>
              <DialogTitle>完整 JWT</DialogTitle>
              <DialogDescription>
                {jwtView?.account.name}（{jwtView?.account.userId}）。
                这是可直接用于 API 调用的凭证，请勿分享给他人。
              </DialogDescription>
            </DialogHeader>
            {jwtView?.loading ? (
              <div className="flex items-center gap-2 py-4 text-sm text-muted-foreground">
                <Loader2 className="size-4 animate-spin" />
                正在读取…
              </div>
            ) : (
              <div className="space-y-3">
                <textarea
                  readOnly
                  value={jwtView?.jwt ?? ""}
                  rows={5}
                  className="w-full resize-none rounded-md border border-border bg-muted/40 p-2 font-mono text-[11px] leading-relaxed"
                  onFocus={(e) => e.currentTarget.select()}
                />
                <div className="flex items-center gap-2">
                  <Button
                    size="sm"
                    variant="outline"
                    onClick={async () => {
                      try {
                        await navigator.clipboard.writeText(jwtView?.jwt ?? "");
                        toast.success("已复制到剪贴板");
                      } catch {
                        toast.error("复制失败，请手动选中复制");
                      }
                    }}
                  >
                    复制
                  </Button>
                  <span className="text-xs text-muted-foreground">
                    列表里始终只显示脱敏值，明文仅在此处按需展示。
                  </span>
                </div>
              </div>
            )}
            <DialogFooter>
              <Button variant="outline" onClick={() => setJwtView(null)}>
                关闭
              </Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>
      </div>
    </TooltipProvider>
  );
}
