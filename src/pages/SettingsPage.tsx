import { useCallback, useEffect, useState, type ReactElement, type ReactNode } from "react";
import { toast } from "sonner";
import { ArrowUpCircle, CircleCheck, ExternalLink, Loader2, RefreshCw, Save } from "lucide-react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import * as api from "@/lib/api";
import { getThemePreference, setThemePreference, type ThemePreference } from "@/lib/theme";
import type {
  AutoRotateConfig,
  CheckinConfig,
  CheckinLog,
  GatewayConfig,
  GatewayTaskName,
  GithubConfig,
  RotateLog,
  RotateStatus,
  UpdateInfo,
} from "@/lib/types";
import { GITHUB_RELEASE_URL, GITHUB_REPOSITORY_URL, openReleaseUrl } from "@/lib/update";
import { cn } from "@/lib/utils";
import { UpdateInstallDialog } from "@/components/update-install-dialog";
import { DemoAction } from "@/components/demo-action";
import { useAccountsStore } from "@/stores/accounts";

interface SettingsGroupProps {
  id: string;
  title: string;
  children: ReactNode;
}

function SettingsGroup({ id, title, children }: SettingsGroupProps) {
  return (
    <section className="min-w-0 space-y-2.5" aria-labelledby={id}>
      <div className="px-1">
        <h2 id={id} className="text-[13px] font-medium leading-5">
          {title}
        </h2>
      </div>
      <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">{children}</Card>
    </section>
  );
}

function SettingsRow({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <div
      className={cn(
        "mx-4 flex min-w-0 items-center justify-between gap-3 border-b border-border/50 px-0 py-2.5 sm:mx-5",
        className,
      )}
    >
      {children}
    </div>
  );
}

interface SettingsFieldRowProps {
  label: ReactNode;
  description?: ReactNode;
  htmlFor?: string;
  children: ReactNode;
  className?: string;
  operational?: boolean;
}

function SettingsFieldRow({
  label,
  description,
  htmlFor,
  children,
  className,
  operational = false,
}: SettingsFieldRowProps) {
  return (
    <SettingsRow className={cn("flex-col items-stretch gap-2 sm:flex-row sm:items-center", className)}>
      <div className="min-w-0 flex-1">
        {htmlFor ? (
          <Label htmlFor={htmlFor} className="text-[13px] leading-4">
            {label}
          </Label>
        ) : (
          <div className="text-[13px] font-medium leading-4">{label}</div>
        )}
        {description && (
          <p className="mt-0.5 text-xs leading-4 text-muted-foreground/75">{description}</p>
        )}
      </div>
      <div className="flex min-w-0 w-full shrink-0 justify-end sm:w-auto">
        {operational ? <DemoAction className="w-full sm:w-auto">{children as ReactElement}</DemoAction> : children}
      </div>
    </SettingsRow>
  );
}

function formatTime(ts: number): string {
  try {
    return new Date(ts).toLocaleString("zh-CN", {
      month: "2-digit",
      day: "2-digit",
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
    });
  } catch {
    return String(ts);
  }
}

function logLabel(result: string): { text: string; tone: "success" | "warning" | "error" } {
  switch (result) {
    case "success":
      return { text: "签到成功", tone: "success" };
    case "already":
      return { text: "已签到", tone: "warning" };
    default:
      return { text: "失败", tone: "error" };
  }
}

/** 自动签到配置 + 一键签到 + 日志。 */
function AutoCheckinCard() {
  const [cfg, setCfg] = useState<CheckinConfig | null>(null);
  const [logs, setLogs] = useState<CheckinLog[]>([]);
  // 日志标题里的天数取自「记录保留」设置，避免与真实清理口径不一致
  const [retentionDays, setRetentionDays] = useState<number | null>(null);
  const [saving, setSaving] = useState(false);
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<{ type: "ok" | "err"; text: string } | null>(null);

  useEffect(() => {
    void load();
  }, []);

  async function load() {
    try {
      const [c, l, r] = await Promise.all([
        api.getAutoCheckinConfig(),
        api.getCheckinLogs(),
        // 保留设置失败不应影响签到日志展示，故单独 catch
        api.getRecordRetention().catch(() => null),
      ]);
      setCfg(c);
      setLogs(l.logs);
      if (r) setRetentionDays(r.days);
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    }
  }

  async function save() {
    if (!cfg) return;
    setSaving(true);
    setMsg(null);
    try {
      const saved = await api.saveAutoCheckinConfig(cfg);
      setCfg(saved);
      setMsg({ type: "ok", text: "配置已保存" });
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setSaving(false);
    }
  }

  async function checkinAllNow() {
    setBusy(true);
    setMsg(null);
    try {
      const res = await api.checkinAll();
      if (res.status === "skipped" && res.reason === "already_running") {
        setMsg({ type: "err", text: "签到任务正在进行，请稍后再试" });
        return;
      }
      const ok = res.accounts.filter((a) => a.result === "success").length;
      const already = res.accounts.filter((a) => a.result === "already").length;
      const err = res.accounts.filter((a) => a.result === "error").length;
      const detail = res.accounts
        .filter((a) => a.result === "error")
        .map((a) => `${a.email}（${a.error}）`)
        .join("；");
      setMsg({
        type: err > 0 ? "err" : "ok",
        text: `签到完成：成功 ${ok}，已签 ${already}，失败 ${err}${detail ? `。${detail}` : ""}`,
      });
      void load();
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setBusy(false);
    }
  }

  function setNum(key: keyof CheckinConfig, value: string) {
    if (!cfg) return;
    setCfg({ ...cfg, [key]: Number(value) });
  }

  return (
    <SettingsGroup
      id="settings-auto-checkin"
      title="自动签到"
    >
      <CardContent className="space-y-0 p-0">
        {cfg ? (
          <>
            <SettingsFieldRow
              label="启用自动签到"
              description="启动时立即核验服务端状态，未签到账号会自动补签；仅覆盖国服账号"
              htmlFor="ac-enabled"
              operational
            >
              <Switch
                id="ac-enabled"
                checked={cfg.enabled}
                onCheckedChange={(v) => setCfg({ ...cfg, enabled: v })}
              />
            </SettingsFieldRow>

            <SettingsFieldRow
              label="保活阈值"
              description="天；0 表示每天无条件刷新"
              htmlFor="ac-keep"
              operational
            >
              <Input
                id="ac-keep"
                className="w-full sm:w-48"
                type="number"
                min={0}
                max={90}
                value={cfg.keepalive_days}
                onChange={(e) => setNum("keepalive_days", e.target.value)}
              />
            </SettingsFieldRow>
            <SettingsFieldRow label="惰性刷新" description="小时" htmlFor="ac-lazy" operational>
              <Input
                id="ac-lazy"
                className="w-full sm:w-48"
                type="number"
                min={1}
                max={72}
                value={cfg.lazy_refresh_hours}
                onChange={(e) => setNum("lazy_refresh_hours", e.target.value)}
              />
            </SettingsFieldRow>

            <div className="flex flex-wrap gap-2 border-b-0 border-border/60 px-4 py-3 sm:px-5">
              <DemoAction><Button size="sm" onClick={save} disabled={saving}>
                {saving ? <Loader2 className="animate-spin" /> : <Save />}保存配置
              </Button></DemoAction>
              <DemoAction><Button size="sm" variant="outline" onClick={checkinAllNow} disabled={busy}>
                {busy ? <Loader2 className="animate-spin" /> : <CircleCheck />}全部立即签到
              </Button></DemoAction>
            </div>
          </>
        ) : (
          <p className="px-4 py-3 text-sm text-muted-foreground sm:px-5">加载配置中…</p>
        )}

        {msg && (
          <Alert
            variant={msg.type === "err" ? "destructive" : "default"}
            className="!w-auto mx-4 my-4 sm:mx-5"
          >
            <AlertDescription>{msg.text}</AlertDescription>
          </Alert>
        )}

        <div className="px-4 py-3 sm:px-5">
          {/* 天数必须与「记录保留」设置一致：写死 30 天会在用户改成 60 天后骗人 */}
          <p className="mb-2 text-[13px] font-medium">
            签到日志{retentionDays ? `（最近 ${retentionDays} 天）` : ""}
          </p>
          {logs.length === 0 ? (
            <p className="py-3 text-center text-sm text-muted-foreground">暂无签到记录</p>
          ) : (
            <div className="max-h-64 overflow-y-auto pr-1">
              {[...logs].reverse().map((l, i) => {
                const tone = logLabel(l.result);
                return (
                  <div
                    key={i}
                    className="flex items-center justify-between border-b border-border/60 py-2 text-xs last:border-b-0"
                  >
                    <div className="min-w-0 flex-1 truncate">
                      <span className="font-medium">{l.email}</span>
                      {l.error && <span className="text-destructive">（{l.error}）</span>}
                    </div>
                    <div className="ml-2 flex shrink-0 items-center gap-2">
                      <span
                        className={
                          tone.tone === "error"
                            ? "text-destructive"
                            : tone.tone === "warning"
                              ? "text-amber-600"
                              : "text-emerald-600"
                        }
                      >
                        {tone.text}
                      </span>
                      <span className="text-muted-foreground">{formatTime(l.ts)}</span>
                    </div>
                  </div>
                );
              })}
            </div>
          )}
        </div>
      </CardContent>
    </SettingsGroup>
  );
}

/**
 * 自动养号任务：活跃上报 / 夜猫子 / 开学季 / 国际版 trial。
 *
 * 为什么单独一张卡而不塞进「自动签到」：这 4 个任务跑在**网关**里（不是宿主里），
 * 配置项落在 gateway_config.json 并转写进网关的 config.json；与宿主的自动签到
 * 是两条独立的链路。混在一起会让「改了不生效」变得无从排查。
 *
 * 为什么每个任务都写明前置条件：它们都会在条件不满足时静默跳过
 *（夜猫子限时段、开学季限活动期、活跃上报与开学季只跑国服、trial 只跑国际版）。
 * 不写清楚，用户点「立即执行」看不到任何变化，只会以为功能坏了。
 */
function AutoCareTasksCard() {
  const [cfg, setCfg] = useState<GatewayConfig | null>(null);
  const [saving, setSaving] = useState(false);
  /** 正在「立即执行」的任务名（用于按任务显示 loading）。 */
  const [running, setRunning] = useState<string | null>(null);
  const [msg, setMsg] = useState<{ type: "ok" | "err" | "warn"; text: string } | null>(null);

  const load = useCallback(async () => {
    try {
      const res = await api.getGatewayConfig();
      setCfg(res.config);
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  async function save() {
    if (!cfg) return;
    setSaving(true);
    setMsg(null);
    try {
      const res = await api.saveGatewayConfig({
        activity_hours: cfg.activity_hours,
        nightowl_hours: cfg.nightowl_hours,
        school_hours: cfg.school_hours,
        trial_hours: cfg.trial_hours,
        activity_enabled: cfg.activity_enabled,
        nightowl_enabled: cfg.nightowl_enabled,
        school_enabled: cfg.school_enabled,
        trial_enabled: cfg.trial_enabled,
        activity_report_count: cfg.activity_report_count,
      });
      setCfg(res.config);
      // 说清楚「还要重启」：网关只在启动时读一次 config.json，
      // 不提示的话用户会以为保存没生效，反复点保存。
      setMsg({ type: "ok", text: "配置已保存。重启网关后生效（可在「兼容网关」页重启）" });
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setSaving(false);
    }
  }

  async function runNow(task: GatewayTaskName) {
    setRunning(task);
    setMsg(null);
    try {
      const res = await api.runGatewayTask(task);
      if (!res.ok) {
        setMsg({ type: "err", text: res.error || "执行失败" });
      } else if (!res.ran) {
        // 被前置条件挡下是正常结果，用 warning 而非 error —— 否则用户会以为坏了
        setMsg({ type: "warn", text: res.message || "本次未执行（前置条件不满足）" });
      } else {
        setMsg({ type: "ok", text: `已触发一轮：${res.message || "执行完成"}` });
      }
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setRunning(null);
    }
  }

  /** 更新某个任务的时点列表（输入框是逗号分隔的小时）。 */
  function setHours(key: keyof GatewayConfig, value: string) {
    if (!cfg) return;
    const hours = value
      .split(/[,，\s]+/)
      .map((s) => Number(s.trim()))
      .filter((n) => Number.isInteger(n) && n >= 0 && n <= 23);
    setCfg({ ...cfg, [key]: hours });
  }

  /** 时点列表 → 输入框文本。 */
  function hoursText(hours?: number[]): string {
    return (hours ?? []).join(", ");
  }

  const tasks: {
    name: GatewayTaskName;
    label: string;
    enabledKey: keyof GatewayConfig;
    hoursKey: keyof GatewayConfig;
    description: ReactNode;
    /** 前置条件说明（界面必须写清楚，否则「立即执行」没反应像是坏了）。 */
    note: string;
    /** 额外的数值配置（只有活跃上报有）。 */
    extra?: ReactNode;
  }[] = [
    {
      name: "activity",
      label: "活跃上报",
      enabledKey: "activity_enabled",
      hoursKey: "activity_hours",
      description: "每个账号发送对话活跃事件，点亮连登天数并解锁领养猫猫的前置条件",
      note: "仅国服账号。签到只恢复余额，连登天数必须靠本任务点亮。",
      extra: cfg ? (
        <SettingsFieldRow
          label="每号每日条数"
          description="条；默认 3。单条偶发被服务端丢弃，多条提高点亮成功率"
          htmlFor="care-activity-count"
          operational
        >
          <Input
            id="care-activity-count"
            className="w-full sm:w-48"
            type="number"
            min={1}
            max={20}
            value={cfg.activity_report_count ?? 3}
            onChange={(e) =>
              setCfg({ ...cfg, activity_report_count: Number(e.target.value) })
            }
          />
        </SettingsFieldRow>
      ) : null,
    },
    {
      name: "nightowl",
      label: "夜猫子任务",
      enabledKey: "nightowl_enabled",
      hoursKey: "nightowl_hours",
      description: "在夜猫时段内补一次任务，点亮仅在夜间计入的成长任务",
      note: "只在 23:00–08:00（北京时间）内有效，时段外点击「立即执行」会被跳过并提示原因。仅国服账号。",
    },
    {
      name: "school",
      label: "开学季活动",
      enabledKey: "school_enabled",
      hoursKey: "school_hours",
      description: "领取活动里已达标的奖励",
      note: "限时活动。仅领取已达标的任务奖励，不伪造学生认证 / 邀请等动作；活动下线后自动跳过。仅国服账号。",
    },
    {
      name: "trial",
      label: "国际版 trial 加油包",
      enabledKey: "trial_enabled",
      hoursKey: "trial_hours",
      description: "为国际版账号领取 trial 加油包",
      note: "仅国际版账号（国服无此入口）。已领取过的账号会被幂等跳过，可每天重试。",
    },
  ];

  return (
    <SettingsGroup id="settings-care-tasks" title="自动养号任务">
      <CardContent className="space-y-0 p-0">
        <p className="border-b border-border/60 bg-muted/25 px-4 py-3 text-xs leading-5 text-muted-foreground sm:px-5">
          这 4 个任务由<b className="text-foreground">兼容网关</b>执行。改完配置需要重启网关才会生效；
          「立即执行」会立刻让网关跑一轮，便于验证配置是否正确。
        </p>

        {cfg ? (
          <>
            {tasks.map((task) => (
              <div key={task.name} className="border-b border-border/60">
                <SettingsFieldRow
                  label={task.label}
                  description={task.description}
                  htmlFor={`care-${task.name}-enabled`}
                  operational
                >
                  <Switch
                    id={`care-${task.name}-enabled`}
                    checked={cfg[task.enabledKey] !== false}
                    onCheckedChange={(v) => setCfg({ ...cfg, [task.enabledKey]: v })}
                  />
                </SettingsFieldRow>

                <SettingsFieldRow
                  label="执行时刻"
                  description="小时，可填多个用逗号分隔（0-23）；例如 9, 21"
                  htmlFor={`care-${task.name}-hours`}
                  operational
                >
                  <Input
                    id={`care-${task.name}-hours`}
                    className="w-full sm:w-48"
                    value={hoursText(cfg[task.hoursKey] as number[] | undefined)}
                    onChange={(e) => setHours(task.hoursKey, e.target.value)}
                  />
                </SettingsFieldRow>

                {/* 额外数值配置排在按钮之前：按钮行是这一组的收尾，
                    插在它后面会让「立即执行」看起来属于下一个配置项。 */}
                {task.extra}

                <div className="flex flex-wrap items-center gap-2 px-4 py-3 sm:px-5">
                  <DemoAction>
                    <Button
                      size="sm"
                      variant="outline"
                      disabled={running !== null}
                      onClick={() => void runNow(task.name)}
                    >
                      {running === task.name ? <Loader2 className="animate-spin" /> : <RefreshCw />}
                      立即执行
                    </Button>
                  </DemoAction>
                  <span className="text-xs leading-4 text-muted-foreground/75">{task.note}</span>
                </div>
              </div>
            ))}

            <div className="flex flex-wrap gap-2 px-4 py-3 sm:px-5">
              <DemoAction>
                <Button size="sm" onClick={save} disabled={saving}>
                  {saving ? <Loader2 className="animate-spin" /> : <Save />}保存配置
                </Button>
              </DemoAction>
            </div>
          </>
        ) : (
          <p className="px-4 py-3 text-sm text-muted-foreground sm:px-5">加载配置中…</p>
        )}

        {msg && (
          <Alert
            variant={msg.type === "err" ? "destructive" : "default"}
            className="!w-auto mx-4 my-4 sm:mx-5"
          >
            <AlertDescription>{msg.text}</AlertDescription>
          </Alert>
        )}
      </CardContent>
    </SettingsGroup>
  );
}

/** 自动轮换配置（CodeBuddy CLI）+ 手动检查 + 日志。 */
function AutoRotateCard() {
  const [cfg, setCfg] = useState<AutoRotateConfig | null>(null);
  const [status, setStatus] = useState<RotateStatus | null>(null);
  const [logs, setLogs] = useState<RotateLog[]>([]);
  const [saving, setSaving] = useState(false);
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<{ type: "ok" | "err"; text: string } | null>(null);

  useEffect(() => {
    void load();
  }, []);

  async function load() {
    try {
      const [c, s, l] = await Promise.all([
        api.getAutoRotateConfig(),
        api.getRotateStatus(),
        api.getRotateLogs(),
      ]);
      setCfg(c);
      setStatus(s);
      setLogs(l.logs);
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    }
  }

  async function save() {
    if (!cfg) return;
    setSaving(true);
    setMsg(null);
    try {
      const saved = await api.saveAutoRotateConfig(cfg);
      setCfg(saved);
      setMsg({ type: "ok", text: "配置已保存" });
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setSaving(false);
    }
  }

  async function runNow() {
    setBusy(true);
    setMsg(null);
    try {
      const res = await api.runRotate();
      setMsg({
        type: res.status === "error" ? "err" : "ok",
        text:
          res.status === "switched"
            ? `已切换到 ${res.to ?? "目标账号"}`
            : res.status === "disabled"
              ? "自动轮换未启用（请在下方开启后重试）"
              : (res.reason ?? `检查完成：${res.status}`),
      });
      void load();
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setBusy(false);
    }
  }

  function setNum(key: keyof AutoRotateConfig, value: string) {
    if (!cfg) return;
    setCfg({ ...cfg, [key]: Number(value) });
  }

  function actionLabel(action: string): { text: string; tone: "success" | "warning" | "error" } {
    switch (action) {
      case "switched":
        return { text: "已切换", tone: "success" };
      case "skipped":
        return { text: "未切换", tone: "warning" };
      case "disabled":
        return { text: "未启用", tone: "warning" };
      case "error":
        return { text: "出错", tone: "error" };
      default:
        return { text: action, tone: "warning" };
    }
  }

  return (
    <SettingsGroup
      id="settings-auto-rotate"
      title="CodeBuddy CLI 自动轮换"
    >
      <CardContent className="space-y-0 p-0">
        {status && (
          <div className="flex flex-wrap items-center gap-x-4 gap-y-1 border-b border-border/60 bg-muted/25 px-4 py-3 text-xs text-muted-foreground sm:px-5">
            <span>
              当前 CLI 账号：
              <b className="text-foreground">{status.activeAccountName ?? "未配置"}</b>
            </span>
            {status.lastCheckAt && <span>上次检查 {formatTime(status.lastCheckAt)}</span>}
            {status.lastSwitchAt && <span>上次切换 {formatTime(status.lastSwitchAt)}</span>}
            {!status.cliConfigured && (
              <span className="text-destructive">未接入 CodeBuddy CLI（请先到账号页安装 helper）</span>
            )}
          </div>
        )}

        {cfg ? (
          <>
            <SettingsFieldRow
              label="启用自动轮换"
              description="开启后按下方间隔自动检查并切换 CodeBuddy CLI 账号"
              htmlFor="ar-enabled"
              operational
            >
              <Switch
                id="ar-enabled"
                checked={cfg.enabled}
                onCheckedChange={(v) => setCfg({ ...cfg, enabled: v })}
              />
            </SettingsFieldRow>

            <SettingsFieldRow label="检查间隔" description="分钟" htmlFor="ar-interval" operational>
              <Input
                id="ar-interval"
                className="w-full sm:w-48"
                type="number"
                min={1}
                max={1440}
                value={cfg.check_interval_minutes}
                onChange={(e) => setNum("check_interval_minutes", e.target.value)}
              />
            </SettingsFieldRow>
            <SettingsFieldRow label="切换冷却" description="分钟" htmlFor="ar-cooldown" operational>
              <Input
                id="ar-cooldown"
                className="w-full sm:w-48"
                type="number"
                min={1}
                max={1440}
                value={cfg.cooldown_minutes}
                onChange={(e) => setNum("cooldown_minutes", e.target.value)}
              />
            </SettingsFieldRow>
            <SettingsFieldRow label="到期差异阈值" description="小时" htmlFor="ar-gap" operational>
              <Input
                id="ar-gap"
                className="w-full sm:w-48"
                type="number"
                min={0}
                max={720}
                value={cfg.min_gap_hours}
                onChange={(e) => setNum("min_gap_hours", e.target.value)}
              />
            </SettingsFieldRow>
            <SettingsFieldRow label="到期紧迫阈值" description="小时" htmlFor="ar-urgency" operational>
              <Input
                id="ar-urgency"
                className="w-full sm:w-48"
                type="number"
                min={0}
                max={720}
                value={cfg.min_urgency_hours}
                onChange={(e) => setNum("min_urgency_hours", e.target.value)}
              />
            </SettingsFieldRow>
            <SettingsFieldRow label="活跃保护" description="分钟" htmlFor="ar-guard" operational>
              <Input
                id="ar-guard"
                className="w-full sm:w-48"
                type="number"
                min={0}
                max={1440}
                value={cfg.active_guard_minutes}
                onChange={(e) => setNum("active_guard_minutes", e.target.value)}
              />
            </SettingsFieldRow>
            <SettingsFieldRow label="最小剩余积分" description="低于此值时不切换" htmlFor="ar-min" operational>
              <Input
                id="ar-min"
                className="w-full sm:w-48"
                type="number"
                min={0}
                value={cfg.min_remaining_credits}
                onChange={(e) => setNum("min_remaining_credits", e.target.value)}
              />
            </SettingsFieldRow>
            <p className="border-b border-border/60 px-4 py-3 text-[13px] leading-5 text-muted-foreground sm:px-5">
              切换时机：目标账号剩余到期时间少于「紧迫阈值」且比当前账号早超过「差异阈值」，且最近「活跃保护」分钟内 CLI 无对话、目标剩余积分不低于「最小剩余积分」。
            </p>

            <div className="flex flex-wrap gap-2 border-b-0 border-border/60 px-4 py-3 sm:px-5">
              <DemoAction><Button size="sm" onClick={save} disabled={saving}>
                {saving ? <Loader2 className="animate-spin" /> : <Save />}保存配置
              </Button></DemoAction>
              <DemoAction><Button size="sm" variant="outline" onClick={runNow} disabled={busy}>
                {busy ? <Loader2 className="animate-spin" /> : <RefreshCw />}立即检查一次
              </Button></DemoAction>
            </div>
          </>
        ) : (
          <p className="px-4 py-3 text-sm text-muted-foreground sm:px-5">加载配置中…</p>
        )}

        {msg && (
          <Alert
            variant={msg.type === "err" ? "destructive" : "default"}
            className="!w-auto mx-4 my-4 sm:mx-5"
          >
            <AlertDescription>{msg.text}</AlertDescription>
          </Alert>
        )}

        <div className="px-4 py-3 sm:px-5">
          <p className="mb-2 text-[13px] font-medium">轮换日志（最近 200 条）</p>
          {logs.length === 0 ? (
            <p className="py-3 text-center text-sm text-muted-foreground">暂无轮换记录</p>
          ) : (
            <div className="max-h-64 overflow-y-auto pr-1">
              {logs.map((l, i) => {
                const tone = actionLabel(l.action);
                return (
                  <div
                    key={i}
                    className="flex items-center justify-between border-b border-border/60 py-2 text-xs last:border-b-0"
                  >
                    <div className="min-w-0 flex-1 truncate">
                      {l.action === "switched" && l.from && l.to && (
                        <span className="font-medium">
                          {l.from.name ?? l.from.id} → {l.to.name ?? l.to.id}
                        </span>
                      )}
                      {l.reason && <span className="text-muted-foreground">（{l.reason}）</span>}
                    </div>
                    <div className="ml-2 flex shrink-0 items-center gap-2">
                      <span
                        className={
                          tone.tone === "error"
                            ? "text-destructive"
                            : tone.tone === "success"
                              ? "text-emerald-600"
                              : "text-amber-600"
                        }
                      >
                        {tone.text}
                      </span>
                      <span className="text-muted-foreground">{formatTime(l.ts)}</span>
                    </div>
                  </div>
                );
              })}
            </div>
          )}
        </div>
      </CardContent>
    </SettingsGroup>
  );
}

/** 权限检测卡片：确认本 App 是否有权写入 WorkBuddy 认证文件。 */
function PermissionCheckCard() {
  const authFile = useAuthFile();
  const [checking, setChecking] = useState(false);
  const [result, setResult] = useState<null | { ok: boolean; text: string }>(null);

  async function runCheck() {
    setChecking(true);
    setResult(null);
    try {
      const res = await api.checkAuthPermission();
      setResult({
        ok: res.ok,
        text: res.ok
          ? res.message ?? "认证目录可写，权限正常"
          : `${res.error}（${res.dir ?? ""}）`,
      });
    } catch (e) {
      setResult({ ok: false, text: api.asError(e) });
    } finally {
      setChecking(false);
    }
  }

  return (
    <SettingsGroup
      id="settings-permission"
      title="权限检测"
    >
      <CardContent className="space-y-0 p-0">
        <div className="break-all border-b border-border/60 bg-muted/25 px-4 py-3 font-mono text-[11px] leading-5 text-muted-foreground sm:px-5">
          {authFile || "认证文件路径未获取"}
        </div>
        <div className="flex flex-wrap gap-2 border-b-0 border-border/60 px-4 py-3 sm:px-5">
          <DemoAction><Button size="sm" onClick={runCheck} disabled={checking}>
            {checking ? "检测中…" : "检测权限"}
          </Button></DemoAction>
          <DemoAction><Button
            size="sm"
            variant="outline"
            onClick={() => void api.openPermissionSettings("all_files")}
          >
            打开完全磁盘访问
          </Button></DemoAction>
          <DemoAction><Button
            size="sm"
            variant="outline"
            onClick={() => void api.openPermissionSettings("app_management")}
          >
            打开 App 管理
          </Button></DemoAction>
          <DemoAction><Button size="sm" variant="outline" onClick={() => void api.revealAppInFinder()}>
            在 Finder 中显示
          </Button></DemoAction>
        </div>

        {result && (
          <Alert variant={result.ok ? "default" : "destructive"} className="!w-auto mx-4 my-4 sm:mx-5">
            <AlertDescription>{result.text}</AlertDescription>
          </Alert>
        )}
        {result && !result.ok && (
          <div className="mx-4 mb-4 border-l-2 border-destructive/50 bg-muted/30 px-3 py-2.5 text-xs text-muted-foreground sm:mx-5">
            <p className="mb-1 font-medium text-foreground">如何授权（拖拽方式）：</p>
            <ol className="list-decimal space-y-1 pl-4">
              <li>点上方「打开完全磁盘访问」</li>
              <li>再点「在 Finder 中显示」打开 ai-gateway 所在位置</li>
              <li>
                把 <b>ai-gateway.app</b> 从 Finder <b>直接拖进</b>完全磁盘访问的列表区域
                （即使没有提示框，拖入即生效），然后打开它的开关
              </li>
              <li>回到本页点「检测权限」，或直接重试切换</li>
            </ol>
          </div>
        )}
      </CardContent>
    </SettingsGroup>
  );
}

function useAuthFile(): string | undefined {
  return useAccountsStore((s) => s.status?.authFile);
}

/** 自动更新：检查公开 GitHub Releases 源 + 安装签名更新。 */
function UpdateCard() {
  const version = useAccountsStore((s) => s.status?.version);
  const [info, setInfo] = useState<UpdateInfo | null>(null);
  const [checking, setChecking] = useState(false);
  const [installOpen, setInstallOpen] = useState(false);
  const [msg, setMsg] = useState<{ type: "ok" | "err"; text: string } | null>(null);
  const [githubConfig, setGithubConfig] = useState<GithubConfig>({});
  const [proxyUrl, setProxyUrl] = useState("");
  const [proxySaving, setProxySaving] = useState(false);

  useEffect(() => {
    let cancelled = false;
    void api
      .getGithubConfig()
      .then((config) => {
        if (cancelled) return;
        setGithubConfig(config);
        setProxyUrl(config.proxy ?? "");
      })
      .catch((e) => {
        if (!cancelled) setMsg({ type: "err", text: api.asError(e) });
      });
    return () => {
      cancelled = true;
    };
  }, []);

  async function check() {
    setChecking(true);
    setMsg(null);
    try {
      const r = await api.checkUpdate(proxyUrl, true);
      setInfo(r);
      if (!r.ok) {
        setMsg({ type: "err", text: r.message || r.error || "检查失败" });
      }
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setChecking(false);
    }
  }

  async function saveProxy() {
    const value = proxyUrl.trim();
    if (value) {
      try {
        const parsed = new URL(value);
        if (!parsed.hostname || !["http:", "https:"].includes(parsed.protocol)) {
          throw new Error("unsupported proxy protocol");
        }
      } catch {
        setMsg({ type: "err", text: "代理地址格式不正确，请填写 HTTP/HTTPS 地址，例如 http://127.0.0.1:7897" });
        return;
      }
    }

    setProxySaving(true);
    setMsg(null);
    try {
      const saved = await api.saveGithubConfig({ ...githubConfig, proxy: value });
      setGithubConfig(saved);
      setProxyUrl(saved.proxy ?? "");
      setMsg({
        type: "ok",
        text: value
          ? "代理已保存（更新检查、网关与账号请求均会使用）"
          : "已关闭代理（更新检查与网关请求将直连）",
      });
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setProxySaving(false);
    }
  }

  return (
    <SettingsGroup
      id="settings-updates"
      title="自动更新"
    >
      <CardContent className="space-y-0 p-0">
        <div className="border-b border-border/60 px-4 py-3 text-sm sm:px-5">
          当前版本：<span className="font-mono">v{version || "?"}</span>
        </div>

        <div className="flex min-w-0 items-center justify-between gap-3 border-b border-border/60 bg-muted/25 px-4 py-3 text-sm sm:px-5">
          <div className="min-w-0 flex-1">
            <div className="font-medium">公开更新源</div>
            <div className="truncate text-xs text-muted-foreground">{GITHUB_REPOSITORY_URL}</div>
          </div>
          <DemoAction><Button
            variant="ghost"
            size="icon"
            title="打开 GitHub Release"
            onClick={() => void openReleaseUrl(GITHUB_RELEASE_URL)}
          >
            <ExternalLink />
          </Button></DemoAction>
        </div>

        <SettingsFieldRow
          label="网络代理地址"
          // 该代理的适用范围在「网关支持出站代理」之后扩大了：
          // 以前只服务 GitHub 更新，现在网关与账号相关的上游请求也复用它
          // （国际版 workbuddy.ai 在国内直连不通，必须走代理）。
          // 文案必须说实话，否则用户不会想到「国际版账号报错要来这里配」。
          description="GitHub 更新检查、安装包下载，以及网关与账号的上游请求（国际版账号在国内直连不通时尤其需要）。留空表示直连。"
          htmlFor="update-proxy"
          className="bg-muted/25"
          operational
        >
          <Input
            id="update-proxy"
            className="w-full sm:w-80"
            value={proxyUrl}
            onChange={(event) => setProxyUrl(event.target.value)}
            placeholder="例如 http://127.0.0.1:7897"
            spellCheck={false}
            autoComplete="off"
          />
        </SettingsFieldRow>

        <div className="flex flex-wrap gap-2 border-b border-border/60 bg-muted/25 px-4 py-3 sm:px-5">
          <DemoAction><Button size="sm" variant="outline" onClick={() => void saveProxy()} disabled={proxySaving}>
            {proxySaving ? <Loader2 className="animate-spin" /> : <Save />}
            保存代理
          </Button></DemoAction>
        </div>

        <div className="flex flex-wrap gap-2 border-b-0 border-border/60 px-4 py-3 sm:px-5">
          <DemoAction><Button size="sm" variant="outline" onClick={check} disabled={checking}>
            {checking ? <Loader2 className="animate-spin" /> : <RefreshCw />}
            检查更新
          </Button></DemoAction>
        </div>

        {info?.ok && (
          <Alert variant="default" className={cn("!w-auto mx-4 my-4 sm:mx-5", info.hasUpdate && "border-primary/35 bg-primary/[0.06]")}>
            {info.hasUpdate && <ArrowUpCircle className="text-primary" />}
            <AlertDescription className="space-y-2">
              <AlertTitle className={cn(info.hasUpdate && "text-primary")}>{info.hasUpdate ? "发现新版本" : "更新检查完成"}</AlertTitle>
              <div className="text-sm">
                {info.hasUpdate
                  ? `发现新版本 v${info.latest}（当前 v${info.current}）`
                  : `已是最新版本 v${info.current}`}
                {info.releaseName && <span className="text-muted-foreground"> · {info.releaseName}</span>}
              </div>
              {info.hasUpdate && (
                <DemoAction><Button size="sm" onClick={() => setInstallOpen(true)}>
                  <ArrowUpCircle />
                  立即升级
                </Button></DemoAction>
              )}
              {info.releaseUrl && (
                <DemoAction><Button
                  variant="link"
                  size="sm"
                  className="h-auto p-0"
                  onClick={() => void openReleaseUrl(info.releaseUrl)}
                >
                  打开 GitHub Release
                </Button></DemoAction>
              )}
            </AlertDescription>
          </Alert>
        )}
        {msg && (
          <Alert
            variant={msg.type === "err" ? "destructive" : "default"}
            className="!w-auto mx-4 my-4 sm:mx-5"
          >
            <AlertDescription>{msg.text}</AlertDescription>
          </Alert>
        )}
        <UpdateInstallDialog
          open={installOpen}
          onOpenChange={setInstallOpen}
          update={info}
        />
      </CardContent>
    </SettingsGroup>
  );
}

/** 开机自启（仅桌面端渲染）：开关直接反映系统自启注册状态，切换立即生效。 */
function StartupCard() {
  const [enabled, setEnabled] = useState<boolean | null>(null);
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<{ type: "ok" | "err"; text: string } | null>(null);

  useEffect(() => {
    let cancelled = false;
    void api
      .getLaunchAtLoginEnabled()
      .then((value) => {
        if (!cancelled) setEnabled(value);
      })
      .catch((e) => {
        if (!cancelled) setMsg({ type: "err", text: api.asError(e) });
      });
    return () => {
      cancelled = true;
    };
  }, []);

  async function onToggle(value: boolean) {
    if (busy || enabled === null) return;
    const previous = enabled;
    setBusy(true);
    setMsg(null);
    try {
      // 后端回读 OS 权威状态；即使与请求一致，也以回读值显示。
      const authoritative = await api.setLaunchAtLoginEnabled(value);
      setEnabled(authoritative);
      setMsg({ type: "ok", text: authoritative ? "已开启开机自启" : "已关闭开机自启" });
    } catch (e) {
      // 失败时恢复到最后一次确认的状态，并显示可读错误。
      setEnabled(previous);
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setBusy(false);
    }
  }

  return (
    <SettingsGroup
      id="settings-startup"
      title="启动设置"
    >
      <CardContent className="space-y-0 p-0">
        <SettingsFieldRow
          className="border-b-0"
          label="开机时静默启动到托盘"
          description="开关直接反映系统登录项状态；之后可从托盘「打开主界面」恢复"
          htmlFor="startup-silent"
          operational
        >
          <Switch
            id="startup-silent"
            checked={enabled ?? false}
            disabled={busy || enabled === null}
            onCheckedChange={(v) => void onToggle(v)}
            aria-label="开机时静默启动到托盘"
          />
        </SettingsFieldRow>

        {msg && (
          <Alert
            variant={msg.type === "err" ? "destructive" : "default"}
            className="!w-auto mx-4 my-4 sm:mx-5"
          >
            <AlertDescription>{msg.text}</AlertDescription>
          </Alert>
        )}
      </CardContent>
    </SettingsGroup>
  );
}

/** 外观：主题选择（持久化到 localStorage）。 */
function AppearanceCard() {
  const [theme, setTheme] = useState<ThemePreference>(getThemePreference);

  function onThemeChange(value: string) {
    if (value !== "system" && value !== "light" && value !== "dark") return;
    setThemePreference(value);
    setTheme(value);
  }

  return (
    <SettingsGroup
      id="settings-appearance"
      title="外观"
    >
      <CardContent className="space-y-0 p-0">
        <SettingsFieldRow
          className="border-b-0"
          label="主题"
          description="选择浅色、深色，或跟随系统外观自动切换"
          htmlFor="appearance-theme"
        >
          <Select value={theme} onValueChange={onThemeChange}>
            <SelectTrigger id="appearance-theme" size="sm" className="w-full sm:w-40" aria-label="主题">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="system">系统</SelectItem>
              <SelectItem value="light">浅色</SelectItem>
              <SelectItem value="dark">深色</SelectItem>
            </SelectContent>
          </Select>
        </SettingsFieldRow>
      </CardContent>
    </SettingsGroup>
  );
}

/**
 * 多应用环境配置：Trae Work / Trae / 豆包 的安装路径与豆包端点。
 *
 * 为什么需要手动指定路径：客户端安装位置五花八门（自定义盘符、绿色版），
 * exe 发现链的 6 级回退仍可能在部分机器上落空。此时让用户直接给出路径，
 * 比让他反复重装客户端现实得多。
 */
function AppEnvCard() {
  const APPS = [
    { kind: "TraeWork", label: "Trae Work", hint: "TRAE SOLO CN.exe" },
    { kind: "Trae", label: "Trae", hint: "Trae CN.exe" },
    { kind: "Doubao", label: "豆包", hint: "Doubao.exe" },
  ] as const;

  const [envs, setEnvs] = useState<Record<string, api.AppEnvStatus | null>>({});
  const [paths, setPaths] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    const results = await Promise.all(
      APPS.map(async (app) => {
        try {
          return [app.kind, await api.appEnvCheck(app.kind)] as const;
        } catch {
          return [app.kind, null] as const;
        }
      }),
    );
    const nextEnvs: Record<string, api.AppEnvStatus | null> = {};
    const nextPaths: Record<string, string> = {};
    for (const [kind, status] of results) {
      nextEnvs[kind] = status;
      nextPaths[kind] = status?.manualPath ?? "";
    }
    setEnvs(nextEnvs);
    setPaths(nextPaths);
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const savePath = async (kind: string) => {
    setBusy(true);
    try {
      await api.appSetManualPath(kind, paths[kind] ?? "");
      toast.success("已保存安装路径");
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <SettingsGroup id="settings-app-env" title="应用环境">
      <CardContent className="space-y-0 p-0">
        <p className="px-4 pt-4 text-xs text-muted-foreground sm:px-5">
          Trae Work / Trae / 豆包的安装路径。自动探测失败时可在此手动指定。
        </p>
        {APPS.map((app, index) => {
          const env = envs[app.kind];
          return (
            <div
              key={app.kind}
              className={cn(
                "space-y-2 px-4 py-4 sm:px-5",
                index < APPS.length - 1 && "border-b border-border/60",
              )}
            >
              <div className="flex flex-wrap items-center gap-2">
                <span className="text-sm font-medium">{app.label}</span>
                {env ? (
                  <>
                    <Badge variant={env.installed ? "secondary" : "outline"}>
                      {env.installed ? "已安装" : "未检测到"}
                    </Badge>
                    <Badge variant={env.running ? "secondary" : "outline"}>
                      {env.running ? "运行中" : "未运行"}
                    </Badge>
                    <span className="text-xs text-muted-foreground">
                      快照 {env.snapshotCount} 个
                    </span>
                  </>
                ) : (
                  <Badge variant="outline">检测失败</Badge>
                )}
              </div>
              {env?.exePath && (
                <p className="break-all font-mono text-xs text-muted-foreground">{env.exePath}</p>
              )}
              <div className="flex gap-2">
                <Input
                  value={paths[app.kind] ?? ""}
                  onChange={(e) =>
                    setPaths((prev) => ({ ...prev, [app.kind]: e.target.value }))
                  }
                  placeholder={`手动指定路径，例如 D:\\Programs\\${app.hint}`}
                  className="font-mono text-xs"
                  aria-label={`${app.label} 安装路径`}
                />
                <Button
                  size="sm"
                  variant="outline"
                  disabled={busy}
                  onClick={() => void savePath(app.kind)}
                >
                  保存
                </Button>
              </div>
            </div>
          );
        })}
      </CardContent>
    </SettingsGroup>
  );
}

/**
 * 本地代理：设备身份隔离与凭证抓取。
 *
 * 代理会**改写系统代理设置**，停止时还原为用户原有的值。因此界面必须把
 * 「当前是否在运行」「原有代理是什么」明确展示出来 —— 用户最怕的是
 * 「用了这个功能之后网断了，还不知道为什么」。
 */
function ProxyCard() {
  const [config, setConfig] = useState<api.ProxyConfigView | null>(null);
  const [running, setRunning] = useState(false);
  const [port, setPort] = useState("");
  const [domains, setDomains] = useState("");
  const [cert, setCert] = useState<Awaited<ReturnType<typeof api.proxyCertStatus>> | null>(null);
  const [logs, setLogs] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    try {
      const [cfg, status, certStatus] = await Promise.all([
        api.proxyConfig(),
        api.proxyStatus(),
        api.proxyCertStatus(),
      ]);
      setConfig(cfg);
      setRunning(status.running);
      setCert(certStatus);
      setPort((current) => current || String(cfg.port));
      setDomains((current) => current || cfg.domains);
    } catch (e) {
      // 代理状态读取失败不打扰用户（可能是首次运行、证书目录还没建）
      console.error(api.asError(e));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // 代理日志实时推送：只保留最近 200 行，避免长时间运行把内存吃满
  useEffect(() => {
    if (!api.isDesktop()) return;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void import("@tauri-apps/api/event").then(async ({ listen }) => {
      const stop = await listen<{ line: string }>("proxy-log", (event) => {
        if (disposed) return;
        setLogs((prev) => [...prev, event.payload.line].slice(-200));
      });
      if (disposed) stop();
      else unlisten = stop;
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  const start = async () => {
    setBusy(true);
    try {
      const parsed = Number(port);
      if (!Number.isInteger(parsed) || parsed < 1 || parsed > 65535) {
        throw new Error("端口必须是 1-65535 之间的整数");
      }
      await api.proxyStart(parsed, domains);
      toast.success(`代理已启动，系统代理已指向 127.0.0.1:${parsed}`);
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const stop = async () => {
    setBusy(true);
    try {
      await api.proxyStop();
      toast.success("代理已停止，系统代理已还原");
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const genCert = async () => {
    setBusy(true);
    try {
      const res = await api.proxyCertGenerate();
      toast.success(`CA 证书已就绪：${res.caCerPath}`);
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const captureLocal = async () => {
    setBusy(true);
    try {
      const res = await api.proxyCaptureLocal();
      if (res.ok) toast.success(res.message ?? "已从本机捕获凭证");
      else toast.warning(res.message ?? "未在本机找到可捕获的凭证");
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const existing = config?.existingSystemProxy;

  return (
    <SettingsGroup id="settings-proxy" title="本地代理">
      <CardContent className="space-y-0 p-0">
        <p className="px-4 pt-4 text-xs text-muted-foreground sm:px-5">
          拦截目标域名的请求，为每个账号注入独立设备标识，并自动抓取登录凭证。
          代理运行期间会接管系统代理设置，停止时还原。
        </p>

        <SettingsFieldRow
          label="运行状态"
          description={
            existing && existing[0]
              ? `检测到系统原有代理：${existing[1]}（停止时会还原为它）`
              : "当前未检测到系统代理，停止时会清空代理设置"
          }
        >
          <Badge variant={running ? "secondary" : "outline"}>{running ? "运行中" : "已停止"}</Badge>
        </SettingsFieldRow>

        <SettingsFieldRow label="监听端口" description="仅监听 127.0.0.1，不对局域网开放">
          <Input
            value={port}
            onChange={(e) => setPort(e.target.value)}
            disabled={running}
            className="w-full sm:w-32"
            aria-label="代理端口"
          />
        </SettingsFieldRow>

        <SettingsFieldRow
          label="拦截域名"
          description="逗号分隔，按后缀匹配；未命中的请求透明转发，不影响其他应用上网"
        >
          <Input
            value={domains}
            onChange={(e) => setDomains(e.target.value)}
            disabled={running}
            className="w-full font-mono text-xs sm:w-80"
            aria-label="拦截域名"
          />
        </SettingsFieldRow>

        <SettingsFieldRow
          label="CA 证书"
          description={
            cert?.caExists
              ? "已生成。需在系统中信任后 HTTPS 拦截才生效"
              : "尚未生成。首次启动代理时会自动创建"
          }
        >
          <div className="flex gap-2">
            <Button size="sm" variant="outline" disabled={busy} onClick={() => void genCert()}>
              生成
            </Button>
          </div>
        </SettingsFieldRow>

        {cert && !cert.caExists && (
          <Alert className="!w-auto mx-4 my-3 sm:mx-5">
            <AlertDescription className="text-xs">{cert.hint}</AlertDescription>
          </Alert>
        )}

        <div className="flex flex-wrap gap-2 px-4 py-4 sm:px-5">
          {running ? (
            <Button size="sm" variant="outline" disabled={busy} onClick={() => void stop()}>
              <Loader2 className={cn("size-3.5", busy && "animate-spin")} />
              停止代理
            </Button>
          ) : (
            <Button size="sm" disabled={busy} onClick={() => void start()}>
              <Loader2 className={cn("size-3.5", busy && "animate-spin")} />
              启动代理
            </Button>
          )}
          <Button size="sm" variant="outline" disabled={busy} onClick={() => void captureLocal()}>
            从本机捕获凭证
          </Button>
          <Button size="sm" variant="ghost" disabled={busy} onClick={() => void load()}>
            <RefreshCw className="size-3.5" />
            刷新状态
          </Button>
        </div>

        {logs.length > 0 && (
          <div className="px-4 pb-4 sm:px-5">
            <div className="mb-1 text-xs font-medium">代理日志</div>
            <pre className="max-h-40 overflow-auto rounded-lg border border-border/60 bg-muted/40 p-2 text-[11px] leading-5">
              {logs.join("\n")}
            </pre>
          </div>
        )}
      </CardContent>
    </SettingsGroup>
  );
}

/**
 * 计划任务：把签到 / 保活 / 额度巡检注册为 Windows 计划任务。
 *
 * 为什么需要系统级计划任务而不只靠应用内调度：应用内调度只在应用运行时有效。
 * 用户不会 24 小时开着这个工具，而「每天签到」必须每天都发生。
 */
function ScheduledTaskCard() {
  const [tasks, setTasks] = useState<api.TaskStatusItem[]>([]);
  const [times, setTimes] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    try {
      const res = await api.taskStatus();
      setTasks(res.tasks);
      setTimes((prev) => {
        const next = { ...prev };
        for (const task of res.tasks) {
          if (next[task.kind] === undefined) {
            // 已注册的沿用系统里的时间；未注册的给个合理默认
            next[task.kind] = task.registered && task.time ? task.time : defaultTimeFor(task.kind);
          }
        }
        return next;
      });
    } catch (e) {
      toast.error(api.asError(e));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const register = async (kind: string) => {
    setBusy(true);
    try {
      const res = await api.taskRegister(kind, times[kind] ?? "09:00");
      toast.success(res.message);
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const unregister = async (kind: string) => {
    setBusy(true);
    try {
      await api.taskUnregister(kind);
      toast.success("已删除计划任务");
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const runNow = async (kind: string) => {
    setBusy(true);
    try {
      const res = await api.taskRunNow(kind);
      if (res.ok) toast.success("任务执行完成");
      else toast.warning(`任务执行结束但返回非零（退出码 ${res.exitCode}），请查看日志`);
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <SettingsGroup id="settings-tasks" title="计划任务">
      <CardContent className="space-y-0 p-0">
        <p className="px-4 pt-4 text-xs text-muted-foreground sm:px-5">
          注册为 Windows 计划任务后，即使应用没在运行也会按时执行。
          应用内调度仍然生效，两者互补。
        </p>
        {tasks.map((task, index) => (
          <div
            key={task.kind}
            className={cn(
              "space-y-2 px-4 py-4 sm:px-5",
              index < tasks.length - 1 && "border-b border-border/60",
            )}
          >
            <div className="flex flex-wrap items-center gap-2">
              <span className="text-sm font-medium">{task.label}</span>
              <Badge variant={task.registered ? "secondary" : "outline"}>
                {task.registered ? `已注册 ${task.time || ""}`.trim() : "未注册"}
              </Badge>
            </div>
            {task.error && (
              <p className="text-xs text-destructive">{task.error}</p>
            )}
            <div className="flex flex-wrap items-center gap-2">
              <Input
                value={times[task.kind] ?? ""}
                onChange={(e) =>
                  setTimes((prev) => ({ ...prev, [task.kind]: e.target.value }))
                }
                placeholder="09:00"
                className="w-24"
                aria-label={`${task.label} 执行时间`}
              />
              <Button
                size="sm"
                variant="outline"
                disabled={busy}
                onClick={() => void register(task.kind)}
              >
                {task.registered ? "更新时间" : "注册"}
              </Button>
              {task.registered && (
                <Button
                  size="sm"
                  variant="ghost"
                  disabled={busy}
                  onClick={() => void unregister(task.kind)}
                >
                  删除
                </Button>
              )}
              <Button size="sm" variant="ghost" disabled={busy} onClick={() => void runNow(task.kind)}>
                立即执行
              </Button>
            </div>
          </div>
        ))}
        {tasks.length === 0 && (
          <p className="px-4 py-4 text-xs text-muted-foreground sm:px-5">
            正在读取计划任务状态…
          </p>
        )}
      </CardContent>
    </SettingsGroup>
  );
}

/** 各任务的默认执行时间。 */
function defaultTimeFor(kind: string): string {
  switch (kind) {
    case "trae_checkin":
      return "09:00";
    case "doubao_renew":
      return "09:00";
    case "doubao_quota":
      return "09:30";
    default:
      return "09:00";
  }
}

/** 设置页：自动签到配置 / 权限检测 / 更新配置。 */
export default function SettingsPage() {
  return (
    <div className="mx-auto min-w-0 w-full max-w-3xl px-4 py-6 sm:px-6 sm:py-8">
      <header className="mb-10 sm:mb-12">
        <h1 className="text-2xl font-semibold tracking-tight">设置</h1>
        <p className="mt-2 text-sm leading-6 text-muted-foreground">自动签到、权限检测与自动更新配置。</p>
      </header>

      <div className="min-w-0 space-y-12">
        <AppearanceCard />
        <AppEnvCard />
        <ProxyCard />
        <ScheduledTaskCard />
        <RecordRetentionCard />
        <PermissionCheckCard />
        <AutoCheckinCard />
        <AutoCareTasksCard />
        <AutoRotateCard />
        {api.isDesktop() || api.isDemoMode() ? <StartupCard /> : null}
        {api.isWebui() && !api.isDemoMode() ? null : <UpdateCard />}
      </div>
    </div>
  );
}

/**
 * 记录保留：签到日志 / 积分快照 / 任务记录保留多久。
 *
 * 为什么做成勾选而不是输入框：保留天数是粗粒度选择，
 * 常用档位就那么几个；手填既容易填错（0、负数、极大值），
 * 也要用户自己去想「填多少合适」。
 * 后端仍会做区间归一化（1..3650），越界值会被夹到合法范围并回显真实值。
 */
function RecordRetentionCard() {
  const [setting, setSetting] = useState<api.RecordRetentionSetting | null>(null);
  const [saving, setSaving] = useState(false);
  const [msg, setMsg] = useState<{ type: "ok" | "err"; text: string } | null>(null);

  useEffect(() => {
    let alive = true;
    void (async () => {
      try {
        const res = await api.getRecordRetention();
        if (alive) setSetting(res);
      } catch (e) {
        if (alive) setMsg({ type: "err", text: api.asError(e) });
      }
    })();
    return () => {
      alive = false;
    };
  }, []);

  async function choose(days: number) {
    if (saving || setting?.days === days) return;
    setSaving(true);
    setMsg(null);
    try {
      const res = await api.saveRecordRetention(days);
      // 用后端返回的实际生效值刷新，避免界面与真实行为不一致
      setSetting((prev) => (prev ? { ...prev, days: res.days } : prev));
      setMsg({ type: "ok", text: `已保存：保留 ${res.days} 天` });
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setSaving(false);
    }
  }

  return (
    <SettingsGroup id="settings-retention" title="记录保留">
      <SettingsFieldRow
        label="本地记录保留天数"
        description="签到日志、积分快照与任务记录都会按此天数清理；超出部分在下次写入时自动删除。"
      >
        {setting ? (
          <div className="flex flex-wrap items-center gap-1.5">
            {setting.presets.map((p) => (
              <Button
                key={p.days}
                type="button"
                size="sm"
                variant={setting.days === p.days ? "default" : "outline"}
                className="h-7 px-2.5 text-xs"
                disabled={saving}
                onClick={() => void choose(p.days)}
              >
                {p.label}
              </Button>
            ))}
          </div>
        ) : (
          <span className="text-xs text-muted-foreground">加载中…</span>
        )}
      </SettingsFieldRow>
      {setting && !setting.presets.some((p) => p.days === setting.days) && (
        <SettingsRow>
          <div className="text-xs text-muted-foreground">
            当前为自定义值：<span className="font-medium text-foreground">{setting.days}</span> 天
            （可选范围 {setting.minDays}–{setting.maxDays}）
          </div>
        </SettingsRow>
      )}
      {msg && (
        <SettingsRow>
          <div
            className={cn(
              "text-xs",
              msg.type === "ok" ? "text-emerald-600 dark:text-emerald-500" : "text-destructive",
            )}
          >
            {msg.text}
          </div>
        </SettingsRow>
      )}
    </SettingsGroup>
  );
}
