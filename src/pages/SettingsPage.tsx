import { useCallback, useEffect, useState } from "react";
import { toast } from "sonner";
import { ArrowUpCircle, ExternalLink, Loader2, RefreshCw, Save, ShieldCheck } from "lucide-react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { CardContent } from "@/components/ui/card";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import * as api from "@/lib/api";
import {
  DEFAULT_PROXY_SCOPE,
  PROXY_SCOPE_FIELDS,
  proxyScopeOf,
  proxyScopeSummary,
} from "@/lib/proxy-scope";
import { getThemePreference, setThemePreference, type ThemePreference } from "@/lib/theme";
import type { GithubConfig, ProxyScope, UpdateInfo } from "@/lib/types";
import { GITHUB_RELEASE_URL, GITHUB_REPOSITORY_URL, openReleaseUrl } from "@/lib/update";
import { cn } from "@/lib/utils";
import { UpdateInstallDialog } from "@/components/update-install-dialog";
import { DemoAction } from "@/components/demo-action";
import { LogTail, LogTailHeader, LOG_LEVEL_CLASS, logLevelOf } from "@/components/log-tail";
import { useAccountsStore } from "@/stores/accounts";

// 设置分组原件已抽到公共文件（2026-09-22）：
// 平台专属配置迁到各平台页后，那些页也要用同一套原件才不漂移。
// 见 `settings-primitives.tsx` 的说明。
import { SettingsFieldRow, SettingsGroup, SettingsRow } from "@/components/settings-primitives";

/**
 * 网络代理：**独立成一个配置区**（所有者诉求原话「代理单独开一个配置，
 * 三个选项 Github 国内版 国际版 加上描述」）。
 *
 * 为什么从「自动更新」卡片里搬出来，而不是原地加三个勾选框：
 *
 *  1. 代理的作用范围本轮已经**超出更新**（国际版/国服账号的上游请求都归它管），
 *     继续挂在「自动更新」下会让人以为它只影响更新 —— 那正是上一轮文案反复
 *     改措辞想解决的误解。
 *  2. 「自动更新」卡片在 **webui（浏览器打开宿主页面）下整块不渲染**
 *     （见页面底部的 `api.isWebui() ? null : <UpdateCard/>`），因为浏览器里
 *     不能安装桌面更新包。而代理配置是**纯宿主配置**，与能不能装更新无关 ——
 *     留在那张卡里等于「用浏览器打开时根本配不了代理」。
 *
 * 三格开关的语义差别很大，因此每格都必须带**描述**（所有者本次明确要求
 * 「加上描述」）：只说「国内版」用户无从判断该不该开。
 */
function NetworkProxyCard() {
  const [msg, setMsg] = useState<{ type: "ok" | "err"; text: string } | null>(null);
  const [githubConfig, setGithubConfig] = useState<GithubConfig>({});
  const [proxyUrl, setProxyUrl] = useState("");
  // 三个开关的初值是**默认值**而不是全 false：老配置里没有 proxy_scope 字段，
  // 用全 false 初始化会让界面在加载完成前把「国际版」显示成关闭（而后端实际
  // 是开着的）—— 一帧的假象也足以让人误判成「我的开关被重置了」。
  const [proxyScope, setProxyScope] = useState<ProxyScope>(DEFAULT_PROXY_SCOPE);
  const [proxySaving, setProxySaving] = useState(false);

  useEffect(() => {
    let cancelled = false;
    void api
      .getGithubConfig()
      .then((config) => {
        if (cancelled) return;
        setGithubConfig(config);
        setProxyUrl(config.proxy ?? "");
        // 逐键兜底：老配置没有 proxy_scope，必须回落默认值（见 proxyScopeOf）。
        setProxyScope(proxyScopeOf(config));
      })
      .catch((e) => {
        if (!cancelled) setMsg({ type: "err", text: api.asError(e) });
      });
    return () => {
      cancelled = true;
    };
  }, []);

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
      // proxy_scope **必须一起提交**：后端保存接口写的是整份 github_config.json，
      // 漏传这个字段会让它按默认值补齐 —— 表现成「用户关掉国际版开关、一保存
      // 地址又自己开了」，而界面上看不出是谁改的。
      const saved = await api.saveGithubConfig({
        ...githubConfig,
        proxy: value,
        proxy_scope: proxyScope,
      });
      setGithubConfig(saved);
      setProxyUrl(saved.proxy ?? "");
      // 以**回读值**为准而不是提交值：后端可能有归一化（例如 trim），
      // 界面必须显示真正落盘的那一份，否则用户看到的是自己以为的结果。
      setProxyScope(proxyScopeOf(saved));
      setMsg({
        type: "ok",
        // 提示按**实际生效的开关**生成，而不是写死一句：用户关掉某一格之后
        // 仍看到「国际版会使用它」会以为开关没生效 —— 那是界面在撒谎。
        text: value
          ? proxyScopeSummary(proxyScopeOf(saved))
          : "未填写代理地址，全部直连（开关状态已保留）",
      });
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setProxySaving(false);
    }
  }

  return (
    <SettingsGroup id="settings-network-proxy" title="网络代理">
      <CardContent className="space-y-0 p-0">
        <SettingsFieldRow
          label="网络代理地址"
          // 该代理的适用范围在此前几轮里反复收窄过（先只服务 GitHub 更新、
          // 后来扩到国际版上游、再收窄成「仅国际版」）。本轮起范围不再写死在
          // 文案里，而是由下方**三个独立开关**控制 —— 因此这段描述只说
          // 「填一次、范围见下面开关」。复述出来的范围一旦与开关状态不符，
          // 就是界面在撒谎（而这正是前几轮反复改措辞的原因）。
          description="地址只填一次，下面的开关决定哪些范围使用它。留空表示全部直连。"
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

        {/*
          三个独立开关（Github / 国内版 / 国际版）。

          为什么用 Checkbox 而不是 Switch：所有者在本仓库明确要求过「都改为勾选
          而不是手写」（见 AGENTS.md 的 UI Component Policy），且三行并列时勾选框
          比拨动开关更容易一眼看出「哪些被选中」。
        */}
        <div className="border-b border-border/60 bg-muted/25">
          {PROXY_SCOPE_FIELDS.map((field) => (
            <label
              key={field.key}
              htmlFor={field.id}
              data-slot="proxy-scope-row"
              data-scope-key={field.key}
              className="flex cursor-pointer items-start gap-2.5 border-b border-border/40 px-4 py-2.5 last:border-b-0 sm:px-5"
            >
              <Checkbox
                id={field.id}
                className="mt-0.5"
                checked={proxyScope[field.key]}
                onCheckedChange={(checked) =>
                  // checked 可能是 "indeterminate"（Radix 的三态）：一律按
                  // 「非 true 即 false」处理，避免把中间态写进配置。
                  setProxyScope((prev) => ({ ...prev, [field.key]: checked === true }))
                }
                aria-label={`代理范围：${field.label}`}
              />
              <span className="min-w-0 flex-1">
                <span className="block text-[13px] font-medium leading-4">{field.label}</span>
                <span className="mt-0.5 block text-xs leading-4 text-muted-foreground/75">
                  {field.description}
                </span>
              </span>
            </label>
          ))}
        </div>

        <div className="flex flex-wrap gap-2 border-b-0 border-border/60 px-4 py-3 sm:px-5">
          <DemoAction><Button size="sm" variant="outline" onClick={() => void saveProxy()} disabled={proxySaving}>
            {proxySaving ? <Loader2 className="animate-spin" /> : <Save />}
            保存代理
          </Button></DemoAction>
        </div>

        {msg && (
          <Alert
            variant={msg.type === "err" ? "destructive" : "default"}
            className="!w-auto mx-4 mb-4 sm:mx-5"
          >
            <AlertDescription>{msg.text}</AlertDescription>
          </Alert>
        )}
      </CardContent>
    </SettingsGroup>
  );
}

/** 自动更新：检查公开 GitHub Releases 源 + 安装签名更新。 */
function UpdateCard() {
  const version = useAccountsStore((s) => s.status?.version);
  const [info, setInfo] = useState<UpdateInfo | null>(null);
  const [checking, setChecking] = useState(false);
  const [installOpen, setInstallOpen] = useState(false);
  const [msg, setMsg] = useState<{ type: "ok" | "err"; text: string } | null>(null);
  // 「检查更新」按钮用的代理地址。**只读**：编辑入口在「网络代理」卡片里
  //（那块配置在 webui 下也要能改，而本卡片在 webui 下整块不渲染）。
  const [proxyUrl, setProxyUrl] = useState("");

  useEffect(() => {
    let cancelled = false;
    void api
      .getGithubConfig()
      .then((config) => {
        if (cancelled) return;
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
      // 传 null 而不是空串，让后端按「用户没填」处理；是否真的走代理由
      // Github 那个开关在后端决定（关掉时手动检查也必须直连）。
      const r = await api.checkUpdate(proxyUrl.trim() || undefined, true);
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

        {/*
          代理地址与三个开关**已搬到「网络代理」卡片**（见 NetworkProxyCard）。
          留在这里的重复控件会让两处状态各存一份 —— 在其中一处改完保存，
          另一处仍显示旧值，用户无法判断哪个是真的。
          本卡片只保留「用当前配置的代理检查一次更新」这个动作。
        */}

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
  /** 代理日志「自动滚动」开关，缺省开（实时推送的 tail，最需要跟随最新）。 */
  const [autoScroll, setAutoScroll] = useState(true);
  /** 「检测」进行中。与 `busy` 分开：检测是只读的，不该禁用启动/停止按钮。 */
  const [checking, setChecking] = useState(false);
  /** 上一次检测结果（含逐项明细），null = 还没检测过。 */
  const [checkResult, setCheckResult] = useState<{
    ok: boolean;
    checks: { name: string; ok: boolean; detail: string }[];
    checkedAt: number;
  } | null>(null);

  /** 跑一次代理自检（只读，不改任何状态）。 */
  const runCheck = useCallback(async () => {
    setChecking(true);
    try {
      const res = await api.proxyCheck();
      setCheckResult(res);
    } catch (e) {
      // 检测本身失败也要如实显示 —— 静默什么都不做会让用户以为按钮坏了
      setCheckResult({
        ok: false,
        checks: [{ name: "检测失败", ok: false, detail: api.asError(e) }],
        checkedAt: Date.now(),
      });
    } finally {
      setChecking(false);
    }
  }, []);

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
          {/* 「检测」按钮（2026-09-22 所有者要求：
              「本地代理,配置之后再加上检测按钮」）。

              与「运行中」徽章的区别：徽章只说进程在不在跑，
              而"能不能用"还要看端口真在监听、系统代理真指向它、
              CA 证书已生成 —— 这三件任何一件不成立，用户都会觉得"代理坏了"。 */}
          <Button size="sm" variant="outline" disabled={busy} onClick={() => void runCheck()}>
            <ShieldCheck className={cn("size-3.5", checking && "animate-pulse")} />
            检测
          </Button>
          <Button size="sm" variant="ghost" disabled={busy} onClick={() => void load()}>
            <RefreshCw className="size-3.5" />
            刷新状态
          </Button>
        </div>

        {/* 检测结果（2026-09-22 新增）：逐项列出，而不是只给一句"正常/异常" ——
            用户需要知道**具体哪一项**没过，才能自己动手修
            （比如"系统代理被别的软件改了"和"证书没生成"的处置完全不同）。 */}
        {checkResult && (
          <div className="px-4 pb-4 sm:px-5">
            <Alert variant={checkResult.ok ? "default" : "destructive"}>
              <ShieldCheck />
              <AlertTitle>
                {checkResult.ok ? "代理工作正常" : "代理有问题"}
              </AlertTitle>
              <AlertDescription>
                <ul className="mt-2 space-y-1">
                  {checkResult.checks.map((c) => (
                    <li key={c.name} className="flex gap-2 text-xs leading-5">
                      <span className={cn("shrink-0", c.ok ? "text-emerald-600" : "text-destructive")}>
                        {c.ok ? "✓" : "✗"}
                      </span>
                      <span className="shrink-0 font-medium">{c.name}</span>
                      <span className="min-w-0 flex-1 text-muted-foreground">{c.detail}</span>
                    </li>
                  ))}
                </ul>
              </AlertDescription>
            </Alert>
          </div>
        )}

        {logs.length > 0 && (
          <div className="px-4 pb-4 sm:px-5">
            <LogTailHeader
              title="代理日志"
              autoScroll={autoScroll}
              onAutoScrollChange={setAutoScroll}
              switchId="proxy-log-autoscroll"
            />
            {/*
              改成逐行渲染（原来是整块 `<pre>{logs.join("\n")}</pre>`）：
              只有切开才能**按行着色** —— 整块文本没法给某几行上色。
              用 `whitespace-pre-wrap break-all` 保留原来的空白排版：
              代理日志常有缩进与对齐，用默认 `white-space: normal` 会把缩进吃掉。
            */}
            <LogTail
              revision={`${logs.length}:${logs[logs.length - 1] ?? ""}`}
              autoScroll={autoScroll}
              // 代理日志是**追加**的（`[...prev, line]`），最新一行在最下面
              // → 跟随即滚到底（与参考实现 `box.scrollTop = box.scrollHeight` 同向）。
              anchor="end"
              size="sm"
              className="rounded-lg border border-border/60 bg-muted/40 p-2 font-mono text-[11px] leading-5"
              label="代理日志"
            >
              {logs.map((line, i) => (
                <div key={i} className={cn("whitespace-pre-wrap break-all", LOG_LEVEL_CLASS[logLevelOf(line)])}>
                  {line}
                </div>
              ))}
            </LogTail>
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
        {/* ⚠ 文案不要再列"自动签到/权限检测" —— 那些是**平台专属配置**，
            2026-09-22 已迁到各平台自己的页面（WorkBuddy 账号页 / Qoder / ZCode）。
            这里只描述**与平台无关**的通用设置，否则用户会按提示来设置页找、
            却找不到。 */}
        <p className="mt-2 text-sm leading-6 text-muted-foreground">
          外观、代理、更新与计划任务等通用配置。各平台的配置在对应平台页面。
        </p>
      </header>

      <div className="min-w-0 space-y-12">
        <AppearanceCard />
        <AppEnvCard />
        <NetworkProxyCard />
        <ProxyCard />
        <ScheduledTaskCard />
        <RecordRetentionCard />
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
