import { useCallback, useEffect, useRef, useState } from "react";
import { toast } from "sonner";
import {
  AlertTriangle,
  CalendarCheck,
  Coins,
  Fingerprint,
  Loader2,
  MonitorSmartphone,
  RefreshCw,
  RotateCcw,
  Save,
  Trash2,
  UserPlus,
} from "lucide-react";

import { TraeMark } from "@/components/product-marks";
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
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import * as api from "@/lib/api";
import { SENSITIVE_STRING_INPUT_PROPS } from "@/lib/sensitive-input";
import type {
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

/** JWT 状态的展示样式。 */
function jwtBadge(status: TraeAccountMeta["jwtStatus"], hours: number | null) {
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
  onDelete,
  onClearCooldown,
  onResetDevice,
  onSwitch,
  onSaveLogin,
  busy,
}: {
  account: TraeAccountMeta;
  onDelete: () => void;
  onClearCooldown: () => void;
  onResetDevice: () => void;
  onSwitch: () => void;
  onSaveLogin: () => void;
  busy: boolean;
}) {
  return (
    <div className="flex flex-wrap items-center gap-3 rounded-lg border border-border/60 bg-card px-4 py-3">
      <TraeMark size={34} variant="work" />
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <span className="truncate text-sm font-medium">{account.name}</span>
          {jwtBadge(account.jwtStatus, account.jwtExpHours)}
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
          {!account.hasRefreshToken && <span>无 refresh token（无法自动续期）</span>}
        </div>
      </div>
      <div className="flex shrink-0 flex-wrap items-center gap-1.5">
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
  const [loading, setLoading] = useState(true);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const res = await api.listSnapshots(appKind);
      setSnapshots(res.snapshots);
      setCurrentUserId(res.currentUserId);
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
            {snap.modifiedAt && (
              <span className="text-xs text-muted-foreground">
                {new Date(snap.modifiedAt * 1000).toLocaleString("zh-CN")}
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
  // 切换前记录的当前账号：作为防误覆盖守卫传给后端
  const expectedUidRef = useRef<string>("");

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
  }, [load]);

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

  const doCheckin = async () => {
    setBusy(true);
    try {
      const res = await api.traeCheckinRun();
      setCheckinResult(res);
      const parts = [`成功 ${res.ok}`];
      if (res.already) parts.push(`已签 ${res.already}`);
      if (res.skipped) parts.push(`跳过 ${res.skipped}`);
      if (res.failed) parts.push(`失败 ${res.failed}`);
      if (res.failed > 0) toast.warning(`签到完成：${parts.join(" · ")}`);
      else toast.success(`签到完成：${parts.join(" · ")}`);
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const doDiscover = async () => {
    setBusy(true);
    try {
      const res = await api.traeDiscoverAccounts();
      setDiscovered(res.accounts);
      if (res.accounts.length === 0) {
        toast.info("未在本机发现 Trae 登录痕迹。请先启动客户端并登录一次。");
      }
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
            <Button variant="outline" size="sm" disabled={busy} onClick={() => void doDiscover()}>
              <UserPlus className="size-3.5" />
              发现本机账号
            </Button>
            <Button size="sm" disabled={busy} onClick={() => setAddOpen(true)}>
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
                      按账号顺序执行，已签的会跳过；失败账号按错误类型落冷却，避免反复撞限流。
                    </CardDescription>
                  </div>
                  <Button size="sm" disabled={busy || accounts.length === 0} onClick={() => void doCheckin()}>
                    {busy ? (
                      <Loader2 className="size-3.5 animate-spin" />
                    ) : (
                      <CalendarCheck className="size-3.5" />
                    )}
                    一键签到
                  </Button>
                </CardHeader>
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
                      可以点「发现本机账号」从客户端读取，或粘贴 Cloud-IDE-JWT 手动添加。
                    </AlertDescription>
                  </Alert>
                ) : (
                  <div className="space-y-2">
                    {accounts.map((account) => (
                      <AccountRow
                        key={account.userId}
                        account={account}
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
                      uid 由客户端使用痕迹推导。标记为「无法确认」的候选**不会**入池 ——
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
                              // 本机发现只给出 uid，凭证仍需用户粘贴 JWT ——
                              // 客户端把 JWT 存在加密的 vscdb 里，离线取不出明文
                              setAddName(item.appLabel);
                              setAddOpen(true);
                              toast.info(
                                "请粘贴该账号的 Cloud-IDE-JWT（客户端登录态是加密存储的，无法离线读取）",
                              );
                            }}
                          >
                            添加
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
                  // JWT 是 Base64 大小写敏感的随机串：被浏览器自动大写/纠错后
                  // 解析出的账号 id 会变，而用户看不出是哪一位被改了。
                  {...SENSITIVE_STRING_INPUT_PROPS}
                />
              </div>
              <div className="space-y-1.5">
                <Label htmlFor="trae-name">备注名（可选）</Label>
                {/* 备注名是**人类语言**（「主号」「公司号」），刻意**不**套
                    SENSITIVE_STRING_INPUT_PROPS：关掉自动大写会让移动端写
                    中文/英文句子时更难用，而这里改错大小写用户一眼能看出来。 */}
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
                  // 同为大小写敏感的机器串，理由同上。
                  {...SENSITIVE_STRING_INPUT_PROPS}
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
      </div>
    </TooltipProvider>
  );
}
