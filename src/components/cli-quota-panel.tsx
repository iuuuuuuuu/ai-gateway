import { useCallback, useEffect, useRef, useState } from "react";
import { Loader2, RefreshCw, Sparkles } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { DemoAction } from "@/components/demo-action";
import * as api from "@/lib/api";
import type { CliQuotaAccount, CliQuotaProvider, CliQuotaWindow } from "@/lib/types";
import { cn } from "@/lib/utils";

/**
 * 本机 AI CLI 额度面板（可嵌入）。
 *
 * 抽成独立组件的理由：额度既要在「额度查询」页总览，也要挂在各智能体客户端的
 * 详情里 —— 用户在「智能体管理 → Codex」看到的就是这个客户端，在那里问
 * 「这个客户端还有多少额度」最自然。两处共用同一份解析与展示逻辑，避免
 * 各写一套后口径漂移。
 *
 * 与 `CliQuotaPage` 的分工：页面负责多 provider 网格与批量查询，本组件负责
 * **单个 provider** 的自查自渲染。
 */

/** 智能体客户端 id → 额度 provider 的映射。
 *
 * 只有确实存在本机登录凭证、且语义对得上的才映射：
 * `claude-desktop` 虽然也用 Claude 账号，但凭证由桌面端另行管理，
 * 这里不硬凑（宁可少显示，也不要显示错的账号额度）。
 */
export const CLIENT_TO_PROVIDER: Record<string, CliQuotaProvider> = {
  "claude-code": "claude",
  codex: "codex",
  "grok-build": "xai",
  "kimi-code": "kimi",
};

/** 把毫秒时间戳格式化成「MM-DD HH:mm」。 */
export function formatInstant(ms: number | null): string {
  if (ms === null || !Number.isFinite(ms)) return "";
  return new Date(ms).toLocaleString("zh-CN", {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

/**
 * 重置时刻的人话描述：绝对时间 + 相对剩余量。
 *
 * 只给绝对时间要用户自己算「还有多久」；只给相对量则跨天后无法定位。
 */
export function formatReset(resetAtMs: number | null): string {
  if (resetAtMs === null || !Number.isFinite(resetAtMs)) return "";
  const absolute = formatInstant(resetAtMs);
  const delta = resetAtMs - Date.now();
  if (delta <= 0) return `${absolute} · 已到重置时间`;
  const minutes = Math.max(1, Math.ceil(delta / 60000));
  const relative =
    minutes >= 1440
      ? `${Math.ceil(minutes / 1440)} 天后`
      : minutes >= 60
        ? `${Math.ceil(minutes / 60)} 小时后`
        : `${minutes} 分钟后`;
  return `${absolute} · ${relative}`;
}

/** 剩余比例 → 进度条与文字色调。 */
function toneFor(percent: number | null): { bar: string; text: string } {
  if (percent === null) return { bar: "bg-muted-foreground/40", text: "text-muted-foreground" };
  if (percent <= 10) return { bar: "bg-destructive", text: "text-destructive" };
  if (percent <= 30) return { bar: "bg-[var(--chart-4)]", text: "text-[var(--chart-4)]" };
  return { bar: "bg-primary", text: "text-foreground" };
}

/** 单个额度窗口的进度行。 */
export function QuotaWindowRow({ window }: { window: CliQuotaWindow }) {
  const percent =
    window.remainingPercent === null
      ? null
      : Math.max(0, Math.min(100, window.remainingPercent));
  const tone = toneFor(percent);
  const reset = formatReset(window.resetAtMs);

  return (
    <div className="space-y-1.5">
      <div className="flex items-baseline justify-between gap-3 text-xs">
        <span className="min-w-0 truncate font-medium">{window.label}</span>
        {/* 未知必须显示「—」：显示 0 会让用户以为额度耗尽而去干等重置。 */}
        <span className={cn("shrink-0 tabular-nums", tone.text)}>
          {percent === null ? "—" : `${Math.round(percent)}%`}
        </span>
      </div>
      <div
        className="h-1.5 w-full overflow-hidden rounded-full bg-muted"
        role={percent === null ? undefined : "progressbar"}
        aria-label={percent === null ? undefined : `${window.label} 剩余额度`}
        aria-valuemin={percent === null ? undefined : 0}
        aria-valuemax={percent === null ? undefined : 100}
        aria-valuenow={percent ?? undefined}
      >
        {percent === null ? null : (
          <div
            className={cn("h-full rounded-full transition-all", tone.bar)}
            style={{ width: `${percent}%` }}
          />
        )}
      </div>
      {(reset || window.detail) && (
        <div className="flex flex-wrap items-center gap-x-2 gap-y-0.5 text-[11px] text-muted-foreground">
          {window.detail && <span>{window.detail}</span>}
          {window.detail && reset && <span aria-hidden="true">·</span>}
          {reset && <span>{reset}</span>}
        </div>
      )}
    </div>
  );
}

/**
 * 单 provider 的额度面板：自查自渲染（嵌入用）。
 *
 * `autoLoad = false`（默认）时不自动打上游 —— 智能体详情页在列表里会被逐个
 * 渲染，自动查询会让打开页面就打出 12 次网络请求。由用户点「查询」触发。
 */
export function CliQuotaPanel({
  provider,
  autoLoad = false,
  className,
}: {
  provider: CliQuotaProvider;
  autoLoad?: boolean;
  className?: string;
}) {
  const [account, setAccount] = useState<CliQuotaAccount | null>(null);
  const [loading, setLoading] = useState(false);
  // 卸载后不再 setState：查询是秒级网络操作，用户很可能中途切走。
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  const load = useCallback(
    async (refresh: boolean) => {
      setLoading(true);
      try {
        if (refresh) {
          const result = await api.refreshCliQuota(provider);
          if (!mounted.current) return;
          if (result.ok && result.account) setAccount(result.account);
          else if (result.error) {
            setAccount((current) => ({
              ...(current ?? emptyAccount(provider)),
              error: result.error ?? "查询失败",
              loggedIn: false,
            }));
          }
        } else {
          // 走整体查询（含磁盘缓存），再挑出本 provider 的记录。
          const all = await api.getCliQuotas(false);
          if (!mounted.current) return;
          const found = all.accounts.find((item) => item.provider === provider);
          if (found) setAccount(found);
        }
      } catch (error) {
        if (mounted.current) {
          setAccount((current) => ({
            ...(current ?? emptyAccount(provider)),
            error: api.asError(error),
          }));
        }
      } finally {
        if (mounted.current) setLoading(false);
      }
    },
    [provider],
  );

  useEffect(() => {
    if (autoLoad) void load(false);
  }, [autoLoad, load]);

  const windows = account?.windows ?? [];
  const loggedIn = account?.loggedIn ?? false;

  return (
    <section className={cn("space-y-3", className)} aria-busy={loading}>
      <div className="flex items-center justify-between gap-3">
        <div className="flex min-w-0 items-center gap-2">
          <span className="text-xs font-semibold">账号额度</span>
          {account && (
            <Badge variant={loggedIn ? "success" : "outline"}>
              {loggedIn ? "已登录" : "未登录"}
            </Badge>
          )}
          {account?.plan && <Badge variant="secondary">{account.plan}</Badge>}
        </div>
        <DemoAction>
          <Button
            type="button"
            variant="ghost"
            size="sm"
            className="shrink-0"
            disabled={loading}
            onClick={() => void load(true)}
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

      {account?.error && (
        <p className="text-xs text-destructive" role="status">
          {account.error}
        </p>
      )}

      {windows.length > 0 && (
        <div className="space-y-3">
          {windows.map((window, index) => (
            <QuotaWindowRow key={`${window.label}-${index}`} window={window} />
          ))}
        </div>
      )}

      {!account && !loading && (
        <p className="text-xs text-muted-foreground">
          点击「查询」获取本机登录账号的剩余额度。
        </p>
      )}

      {account && !account.error && windows.length === 0 && !loading && (
        <p className="text-xs text-muted-foreground">未查询到额度窗口。</p>
      )}

      {account?.resetCredits && account.resetCredits.available !== null && (
        <div className="flex items-center gap-1.5 text-[11px] text-muted-foreground">
          <Sparkles className="size-3 shrink-0" />
          <span>手动重置次数：{account.resetCredits.available}</span>
        </div>
      )}

      {account && account.fetchedAt > 0 && (
        <p className="text-[11px] text-muted-foreground">查询于 {formatInstant(account.fetchedAt)}</p>
      )}
    </section>
  );
}

/** 空账号占位（仅用于承载错误信息，不含 token）。 */
function emptyAccount(provider: CliQuotaProvider): CliQuotaAccount {
  return {
    id: `${provider}:unknown`,
    provider,
    providerLabel: provider,
    label: provider,
    loggedIn: false,
    source: "",
    plan: null,
    windows: [],
    error: null,
    fetchedAt: 0,
    resetCredits: null,
    subscriptionActiveUntil: null,
  };
}
