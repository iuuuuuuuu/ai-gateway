import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { toast } from "sonner";
import { CircleAlert, CircleCheck, Info, Loader2, RefreshCw, Sparkles } from "lucide-react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { DemoAction } from "@/components/demo-action";
import { Separator } from "@/components/ui/separator";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { QuotaWindowRow, formatInstant } from "@/components/cli-quota-panel";
import * as api from "@/lib/api";
import type { CliQuotaAccount, CliQuotaProvider, CliQuotaStatusItem } from "@/lib/types";
import { useVisibilityInterval } from "@/lib/use-visibility-interval";

/**
 * 本机 AI CLI 的登录额度查询页。
 *
 * 对照 EasyCLIProxyAPI 的 `QuotaPage`：按 provider 分组展示各账号的额度窗口。
 * 差别在凭证来源 —— 那边靠自带内核的 `/api-call` 代发请求，这边直接读本机 CLI
 * 已登录的凭证（`~/.codex/auth.json`、Windows 凭据管理器等）后原生调上游。
 *
 * 三条展示原则：
 *
 * 1. **未登录不是错误**：本机没登录某个 CLI 是常态，显示中性的「未登录」+
 *    去哪登录的指引，而不是红色失败。
 * 2. **未知不等于用尽**：`remainingPercent === null` 显示「—」。显示 0 会让
 *    用户以为额度耗尽而去干等重置。
 * 3. **首屏不打上游**：先用本地登录态渲染骨架（毫秒级），额度由缓存或用户点
 *    「查询」填充 —— 5 个 provider 就是 5 次网络往返。
 *
 * 单窗口的进度条与时间格式化与 [`CliQuotaPanel`] 共用（见 `cli-quota-panel.tsx`），
 * 避免「页面里 0% / 面板里 —」这种口径漂移。
 */

/** provider 展示名与一句话说明。 */
const PROVIDER_META: Record<CliQuotaProvider, { label: string; blurb: string }> = {
  claude: { label: "Claude", blurb: "Claude Code 的订阅额度（5 小时 / 7 天窗口）" },
  antigravity: { label: "Antigravity", blurb: "Google Antigravity 的 Cloud Code 额度" },
  codex: { label: "Codex", blurb: "ChatGPT 订阅的 Codex 额度（5 小时 / 每周窗口）" },
  xai: { label: "Grok", blurb: "xAI Grok 的账单周期用量" },
  kimi: { label: "Kimi", blurb: "Kimi Code 的用量窗口" },
};

/** 展示顺序：与后端 `CliProvider::ALL` 保持一致。 */
const PROVIDER_ORDER: CliQuotaProvider[] = ["claude", "antigravity", "codex", "xai", "kimi"];

/** 自动刷新间隔：额度变化不快，30 分钟足够，且避开上游限流。 */
const AUTO_REFRESH_MS = 30 * 60 * 1000;

/** 单个 provider 卡片。 */
function ProviderCard({
  provider,
  status,
  account,
  loading,
  onRefresh,
}: {
  provider: CliQuotaProvider;
  status: CliQuotaStatusItem | undefined;
  account: CliQuotaAccount | undefined;
  loading: boolean;
  onRefresh: () => void;
}) {
  const meta = PROVIDER_META[provider];
  const loggedIn = status?.loggedIn ?? account?.loggedIn ?? false;
  const windows = account?.windows ?? [];
  const hasError = Boolean(account?.error);

  return (
    <Card className="min-w-0">
      <CardHeader className="gap-2">
        <div className="flex items-start justify-between gap-3">
          <div className="min-w-0">
            <CardTitle className="flex items-center gap-2 text-base">
              {meta.label}
              {loggedIn ? (
                <Badge variant="success">已登录</Badge>
              ) : (
                <Badge variant="outline">未登录</Badge>
              )}
            </CardTitle>
            <CardDescription className="mt-1">{meta.blurb}</CardDescription>
          </div>
          <DemoAction>
            <Button
              type="button"
              variant="outline"
              size="sm"
              className="shrink-0"
              disabled={loading || !loggedIn}
              onClick={onRefresh}
            >
              {loading ? (
                <Loader2 className="size-3.5 animate-spin" />
              ) : (
                <RefreshCw className="size-3.5" />
              )}
              查询
            </Button>
          </DemoAction>
        </div>
        {account && (account.label || account.plan || account.source) && (
          <div className="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-muted-foreground">
            {account.label && <span className="font-medium text-foreground">{account.label}</span>}
            {account.plan && <Badge variant="secondary">{account.plan}</Badge>}
            {account.source && (
              <Tooltip>
                <TooltipTrigger asChild>
                  <span className="inline-flex min-w-0 items-center gap-1">
                    <Info className="size-3 shrink-0" />
                    <span className="truncate">凭证来源</span>
                  </span>
                </TooltipTrigger>
                <TooltipContent className="max-w-[420px] break-all">{account.source}</TooltipContent>
              </Tooltip>
            )}
          </div>
        )}
      </CardHeader>
      <CardContent className="space-y-3">
        {!loggedIn && (
          <p className="text-xs text-muted-foreground">
            {status?.loginHint ?? "在本机登录对应 CLI 后再查询"}
          </p>
        )}

        {loggedIn && loading && windows.length === 0 && (
          <div className="flex items-center gap-2 py-4 text-xs text-muted-foreground">
            <Loader2 className="size-3.5 animate-spin" />
            正在查询上游额度…
          </div>
        )}

        {hasError && (
          <Alert variant="destructive">
            <CircleAlert className="size-4" />
            <AlertTitle>查询失败</AlertTitle>
            <AlertDescription className="break-words">{account?.error}</AlertDescription>
          </Alert>
        )}

        {windows.length > 0 && (
          <div className="space-y-3">
            {windows.map((window, index) => (
              <QuotaWindowRow key={`${window.label}-${index}`} window={window} />
            ))}
          </div>
        )}

        {loggedIn && !loading && !hasError && windows.length === 0 && (
          <p className="text-xs text-muted-foreground">尚未查询。点击「查询」获取当前额度。</p>
        )}

        {account && (
          <div className="space-y-1.5 text-[11px] text-muted-foreground">
            {account.resetCredits && account.resetCredits.available !== null && (
              <div className="flex items-center gap-1.5">
                <Sparkles className="size-3 shrink-0" />
                <span>
                  手动重置次数：{account.resetCredits.available}
                  {account.resetCredits.applicable !== null &&
                    `（可用 ${account.resetCredits.applicable}）`}
                </span>
              </div>
            )}
            {account.subscriptionActiveUntil && (
              <div>订阅有效期至 {formatInstant(Date.parse(account.subscriptionActiveUntil))}</div>
            )}
            {account.fetchedAt > 0 && <div>查询于 {formatInstant(account.fetchedAt)}</div>}
          </div>
        )}
      </CardContent>
    </Card>
  );
}

export default function CliQuotaPage() {
  const [statuses, setStatuses] = useState<CliQuotaStatusItem[]>([]);
  const [accounts, setAccounts] = useState<Record<string, CliQuotaAccount>>({});
  const [loadingProviders, setLoadingProviders] = useState<Set<CliQuotaProvider>>(new Set());
  const [bulkLoading, setBulkLoading] = useState(false);
  const [statusError, setStatusError] = useState("");

  // 卸载后不再 setState：查询是秒级网络操作，用户很可能中途切页。
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  const setProviderLoading = useCallback((provider: CliQuotaProvider, loading: boolean) => {
    setLoadingProviders((current) => {
      const next = new Set(current);
      if (loading) next.add(provider);
      else next.delete(provider);
      return next;
    });
  }, []);

  /** 单个 provider 强制重查。 */
  const refreshOne = useCallback(
    async (provider: CliQuotaProvider) => {
      setProviderLoading(provider, true);
      try {
        const result = await api.refreshCliQuota(provider);
        if (!mounted.current) return;
        if (result.ok && result.account) {
          const account = result.account;
          setAccounts((current) => ({ ...current, [account.id]: account }));
        } else {
          toast.error(result.error || `${PROVIDER_META[provider].label} 查询失败`);
        }
      } catch (error) {
        if (mounted.current) {
          toast.error(`${PROVIDER_META[provider].label} 查询失败：${api.asError(error)}`);
        }
      } finally {
        if (mounted.current) setProviderLoading(provider, false);
      }
    },
    [setProviderLoading],
  );

  /** 全部重查。 */
  const refreshAll = useCallback(async () => {
    setBulkLoading(true);
    try {
      const result = await api.getCliQuotas(true);
      if (!mounted.current) return;
      setAccounts(Object.fromEntries(result.accounts.map((item) => [item.id, item])));
      const failed = result.accounts.filter((item) => item.loggedIn && item.error).length;
      if (failed > 0) toast.warning(`查询完成，其中 ${failed} 个账号未能取到额度`);
      else toast.success("额度已更新");
    } catch (error) {
      if (mounted.current) toast.error(`查询失败：${api.asError(error)}`);
    } finally {
      if (mounted.current) setBulkLoading(false);
    }
  }, []);

  // 首屏：先读登录态（本地、毫秒级）+ 已有的缓存额度，不打上游。
  useEffect(() => {
    let disposed = false;
    (async () => {
      try {
        const [status, cached] = await Promise.all([
          api.getCliQuotaStatus(),
          api.getCliQuotas(false).catch(() => ({ accounts: [] as CliQuotaAccount[] })),
        ]);
        if (disposed) return;
        setStatuses(status.providers);
        setAccounts(Object.fromEntries(cached.accounts.map((item) => [item.id, item])));
      } catch (error) {
        if (!disposed) setStatusError(api.asError(error));
      }
    })();
    return () => {
      disposed = true;
    };
  }, []);

  // 可见时才跑定时器：窗口收进托盘后组件不卸载，裸 setInterval 会一直打上游。
  useVisibilityInterval(() => void refreshAll(), AUTO_REFRESH_MS, {
    immediate: false,
    enabled: statuses.some((item) => item.loggedIn),
  });

  const statusByProvider = useMemo(() => {
    const map: Partial<Record<CliQuotaProvider, CliQuotaStatusItem>> = {};
    statuses.forEach((item) => {
      map[item.provider] = item;
    });
    return map;
  }, [statuses]);

  /** 每个 provider 取它自己的账号记录（同一 provider 只会有一条）。 */
  const accountByProvider = useMemo(() => {
    const map: Partial<Record<CliQuotaProvider, CliQuotaAccount>> = {};
    Object.values(accounts).forEach((account) => {
      map[account.provider] = account;
    });
    return map;
  }, [accounts]);

  const loggedInCount = statuses.filter((item) => item.loggedIn).length;

  return (
    <div className="mx-auto w-full max-w-[1180px] min-w-0 px-4 py-6 sm:px-8 sm:py-9">
      <header className="mb-6 flex flex-wrap items-start justify-between gap-4">
        <div className="min-w-0">
          <h1 className="text-[28px] font-semibold tracking-tight">额度查询</h1>
          <p className="mt-1 text-sm text-muted-foreground">
            查询本机已登录的 AI CLI 账号剩余额度。凭证只在本机读取、直接请求上游，不经过任何第三方服务。
          </p>
        </div>
        <DemoAction>
          <Button
            type="button"
            variant="outline"
            size="sm"
            disabled={bulkLoading || loggedInCount === 0}
            onClick={() => void refreshAll()}
          >
            {bulkLoading ? (
              <Loader2 className="size-3.5 animate-spin" />
            ) : (
              <RefreshCw className="size-3.5" />
            )}
            全部查询
          </Button>
        </DemoAction>
      </header>

      {statusError && (
        <Alert variant="destructive" className="mb-4">
          <CircleAlert className="size-4" />
          <AlertTitle>无法读取本机登录态</AlertTitle>
          <AlertDescription>{statusError}</AlertDescription>
        </Alert>
      )}

      {!statusError && loggedInCount === 0 && (
        <Alert className="mb-4">
          <Info className="size-4" />
          <AlertTitle>未检测到任何已登录的 CLI</AlertTitle>
          <AlertDescription>
            本页查询的是「本机已登录」的 CLI 账号额度。请先在终端登录至少一个（如
            `codex login`、`claude`、`grok login`），再回到本页查询。
          </AlertDescription>
        </Alert>
      )}

      {!statusError && loggedInCount > 0 && (
        <div className="mb-4 flex items-center gap-2 text-xs text-muted-foreground">
          <CircleCheck className="size-3.5" />
          已检测到 {loggedInCount} 个已登录的 CLI，共 {PROVIDER_ORDER.length} 个支持的 provider。
        </div>
      )}

      <Separator className="mb-6" />

      <div className="grid min-w-0 grid-cols-1 gap-4 lg:grid-cols-2">
        {PROVIDER_ORDER.map((provider) => (
          <ProviderCard
            key={provider}
            provider={provider}
            status={statusByProvider[provider]}
            account={accountByProvider[provider]}
            loading={bulkLoading || loadingProviders.has(provider)}
            onRefresh={() => void refreshOne(provider)}
          />
        ))}
      </div>
    </div>
  );
}
