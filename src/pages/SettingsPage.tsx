import { useCallback, useEffect, useState } from "react";
import { toast } from "sonner";
import { ArrowUpCircle, ExternalLink, Gauge, Loader2, PlugZap, RefreshCw, Save, ShieldCheck } from "lucide-react";

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
import { ProxyLogsPanel } from "@/components/proxy-logs-panel";
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
  /** 「测试连接」进行中。与 proxySaving 分开：测试是只读的，不该禁用保存。 */
  const [proxyTesting, setProxyTesting] = useState(false);
  /** 上一次连通性测试的结果，null = 还没测过。 */
  const [probeResult, setProbeResult] = useState<api.ProxyProbeResult | null>(null);
  /** 「节点测速」进行中。 */
  const [nodesProbing, setNodesProbing] = useState(false);
  /** 节点测速结果，null = 还没测过。 */
  const [nodesResult, setNodesResult] = useState<api.ProxyNodesResult | null>(null);

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

  /**
   * 测试**输入框里当前**的代理地址能否连通外网（只读，不改配置）。
   *
   * 为什么要测未保存的值：用户的自然顺序是"先试试行不行，行再保存"。
   * 强制先保存再测会让他多一次写入 —— 而且若地址是错的，
   * 那次写入还会短暂影响后续请求。
   */
  async function testProxy() {
    setProxyTesting(true);
    setProbeResult(null);
    try {
      setProbeResult(await api.proxyTestUpstream(proxyUrl.trim()));
    } catch (e) {
      // 测试本身失败也要如实显示 —— 静默什么都不做会让用户以为按钮坏了。
      setProbeResult({ ok: false, error: api.asError(e) });
    } finally {
      setProxyTesting(false);
    }
  }

  /**
   * 探测本机代理软件里的**所有节点**，按对 `workbuddy.ai` 的延迟排序。
   *
   * 只读：只调代理软件的控制接口查延迟，不改任何配置、不切换节点 ——
   * 「换哪个节点」由用户自己在代理软件里决定（自动化切换会超出本应用的职责，
   * 而且用户的代理软件可能正被其它程序使用）。
   */
  async function probeNodes() {
    setNodesProbing(true);
    setNodesResult(null);
    try {
      setNodesResult(await api.proxyProbeNodes());
    } catch (e) {
      // 失败要如实显示，且**带上可操作的原因**（后端已给出）——
      // 静默什么都不做会让用户以为按钮坏了。
      setNodesResult({ ok: false, error: api.asError(e) });
    } finally {
      setNodesProbing(false);
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
          {/* 「测试连接」按钮（2026-09-22 所有者要求：
              「设置里面的配置代理，也没有测试按钮 可以用来测试是否连通」）。

              ⚠ 它测的是**输入框里当前的地址**，不是已保存的值 ——
              用户想"先试试这个地址行不行，行再保存"，那是更自然的顺序。
              保存与否不影响测试结果。

              与「本地代理」那张卡的「检测」按钮的区别：
              那个查本地代理进程/端口/系统代理/证书四项；
              这个只回答一个问题：**通过它能不能访问外网**。 */}
          <Button
            size="sm"
            variant="outline"
            disabled={proxyTesting}
            onClick={() => void testProxy()}
          >
            {proxyTesting ? <Loader2 className="animate-spin" /> : <PlugZap />}
            测试连接
          </Button>
          {/* 「节点测速」按钮（2026-09-23 所有者要求：
              「探测本机配置的代理的速度那个更快更好」）。

              为什么需要：实测国际版带真实 token 首字节要 3.98~8.05s，
              而**同一代理打 api.z.ai 只要 0.34s** —— 即"代理不慢，
              是 workbuddy.ai 走的那个节点有问题"。这个按钮直接排序告诉
              用户该换成哪个节点。

              ⚠ 只在桌面端可用（要读本机代理软件的控制接口）。
              与「测试连接」的区别：那个回答"这台代理通不通"，
              这个回答"这台代理里**哪个节点最快**"。 */}
          {api.isDesktop() && (
            <Button
              size="sm"
              variant="outline"
              disabled={nodesProbing}
              onClick={() => void probeNodes()}
            >
              {nodesProbing ? <Loader2 className="animate-spin" /> : <Gauge />}
              节点测速
            </Button>
          )}
        </div>

        {/* 测试结果：逐项列出**本应用真正依赖的国外域名**，而不是只给一句"通/不通"。
            
            所有者要求「加上所用到的国外的域名,就比如 workbuddy.ai 这个」——
            列真实用到的域名有两个好处：
              · 某一项不通时直接指向该改什么（"workbuddy.ai 不通"⇒ 给国际版配代理）
              · 不会像 google 那样"通了也不代表本应用能用"（代理常按域名分流） */}
        {probeResult && (
          <div className="px-4 pb-4 sm:px-5">
            <Alert variant={probeResult.ok ? "default" : "destructive"} className="!w-auto">
              <PlugZap />
              <AlertTitle>
                {/* ⚠ 标题**必须**区分"全通"与"通了一部分"（2026-09-23 所有者截图现场）。
                    他截图时标题写「代理可以连通外网」，而 github.com 那行是
                    ✗ 连接被拒绝 —— 他那台机器的自动更新恰恰依赖 github，
                    界面等于在骗他。根因是后端曾按"任一通过即 true"汇总。 */}
                {probeResult.error
                  ? "代理测试未完成"
                  : probeResult.ok
                    ? "全部可用"
                    : probeResult.anyOk
                      ? "部分不可用"
                      : "代理不可用"}
              </AlertTitle>
              <AlertDescription>
                {probeResult.error ? (
                  <p className="mt-1 text-xs leading-5">{probeResult.error}</p>
                ) : (
                  <>
                    <p className="mt-1 text-xs leading-5 text-muted-foreground">
                      下列域名是本应用**实际会用到的**，每一项都对应一个功能。
                    </p>
                    {/* ⚠ 自动更新依赖 github，它单独坏了最容易被用户当场感知
                        （点检查更新就失败），故单独点出来，而不是埋在列表里。 */}
                    {probeResult.githubBlocked && (
                      <p className="mt-1 text-xs leading-5 font-medium text-destructive">
                        github.com 不通 ⇒ 软件内的「检查更新」与下载安装包会失败。
                      </p>
                    )}
                    <ul className="mt-2 space-y-1.5">
                      {(probeResult.results ?? []).map((r) => (
                        <li key={r.name} className="flex flex-wrap items-baseline gap-x-2 text-xs leading-5">
                          <span className={cn("shrink-0", r.ok ? "text-emerald-600" : "text-destructive")}>
                            {r.ok ? "✓" : "✗"}
                          </span>
                          <span className="shrink-0 font-medium">{r.name}</span>
                          {/* purpose 让用户知道"这一项影响什么功能" —— 
                              否则他不知道要不要管它（不用 Qoder 的人
                              看到 api3.qoder.sh 不通会白紧张）。 */}
                          {r.purpose && (
                            <span className="shrink-0 text-muted-foreground/70">（{r.purpose}）</span>
                          )}
                          <span className="min-w-0 flex-1 text-right text-muted-foreground">
                            {r.detail}
                          </span>
                        </li>
                      ))}
                    </ul>
                  </>
                )}
                {/* 全失败时给出"是网络还是代理"的结论 —— 这一条能省掉
                    用户一轮来回排查（改代理没用 vs 改代理有用）。 */}
                {probeResult.directNote && (
                  <p className="mt-2 text-xs leading-5 text-muted-foreground">
                    {probeResult.directNote}
                  </p>
                )}
              </AlertDescription>
            </Alert>
          </div>
        )}

        {/* 节点测速结果（2026-09-23 新增）。
            
            排序由后端完成（延迟升序，不通的在后），这里只负责展示 ——
            排序逻辑放后端是为了让它可被单元测试钉住。
            
            ⚠ 界面**不提供**"切换到最快节点"按钮：切节点会改变用户代理软件的
            全局状态（他可能正用着别的程序），那超出本应用的职责。
            这里只回答"哪个最快"，换不换由他自己决定。 */}
        {nodesResult && (
          <div className="px-4 pb-4 sm:px-5">
            <Alert variant={nodesResult.ok ? "default" : "destructive"} className="!w-auto">
              <Gauge />
              <AlertTitle>
                {nodesResult.ok
                  ? `本机代理共 ${nodesResult.total ?? 0} 个节点，${nodesResult.usable ?? 0} 个可用`
                  : "节点测速未完成"}
              </AlertTitle>
              <AlertDescription>
                {nodesResult.error ? (
                  <p className="mt-1 text-xs leading-5">{nodesResult.error}</p>
                ) : (
                  <>
                    <p className="mt-1 text-xs leading-5 text-muted-foreground">
                      按对 <span className="font-medium">{nodesResult.testUrl}</span> 的延迟排序
                      {nodesResult.endpoint ? `（经 ${nodesResult.endpoint}）` : ""}。
                      换节点请在代理软件里操作。
                    </p>
                    <ul className="mt-2 space-y-1">
                      {(nodesResult.nodes ?? []).map((n) => (
                        <li
                          key={n.name}
                          className="flex flex-wrap items-baseline gap-x-2 text-xs leading-5"
                        >
                          <span
                            className={cn(
                              "shrink-0 tabular-nums",
                              n.ok
                                ? n.delayMs !== null && n.delayMs < 200
                                  ? "font-medium text-emerald-600"
                                  : "text-foreground"
                                : "text-destructive",
                            )}
                          >
                            {n.ok ? `${n.delayMs}ms` : "不通"}
                          </span>
                          <span className="min-w-0 flex-1 truncate" title={n.name}>
                            {n.name}
                          </span>
                          {/* 「当前使用」要突出 —— 用户最想知道的是
                              "我现在用的这个排第几"。 */}
                          {n.current && (
                            <span className="shrink-0 rounded bg-primary/10 px-1 text-[10px] text-primary">
                              当前使用
                            </span>
                          )}
                        </li>
                      ))}
                    </ul>
                  </>
                )}
              </AlertDescription>
            </Alert>
          </div>
        )}

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
    // 流式布局：内容随窗口铺满（减去侧栏），只保留内边距。
    //
    // 留白的演进（按 2340px 屏、侧栏 220px 计算，每侧留白）：
    //   max-w-3xl (768px)  → 676px   ← 所有者反馈"设置页面两侧留白过多"
    //   去除上限（本版）    → 0（仅 px-8 内边距）
    // 内边距量级与「兼容网关」「WorkBuddy 账号」「Qoder」「ZCode」四页**逐字一致**
    // （`px-5 py-6 sm:px-8 sm:py-8`），切换页面时页头不会左右跳动。
    //
    // ⚠ 本页**不能**照抄那两页的"单列完全铺满"，理由是**内容形态不同**：
    //   那边是横向铺开的行式布局（状态块、账号池卡片、用量表），能自然拉伸；
    //   本页主体是**表单行**，而 `SettingsFieldRow` 是两端布局 ——
    //   标签 `flex-1` 在左、控件 `shrink-0` 在右。单列铺满 2056px 时，
    //   标签与它对应的输入框会被拉开近 2000px，比留白更难用：眼睛要在同一行上
    //   横跨整屏，才能把"这一行在改什么"与"改它的控件"对上。
    //
    //   另一种常见做法是"外层放开 + 给每个字段加 max-w-[640px]"，
    //   本页也**不可行**：那只收窄**控件**，收不窄**行**本身 —— 控件仍被
    //   `justify-between` 推到最右，中间照旧空出一大片。要收窄行就得改
    //   `settings-primitives.tsx`，而那个文件被本页 + 三个平台页共用，
    //   不在本次可改范围。
    //
    //   故取第三条路：**外层放开 + 卡片自适应多列**。横向空间用来多放几列，
    //   而不是把一行拉长 —— 每列都落在可读宽度内，两侧留白同时消失。
    <div className="w-full min-w-0 px-5 py-6 sm:px-8 sm:py-8">
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

      {/*
        卡片容器：**自适应多列**（auto-fill），而不是单列铺满。

        ⚠ 下界取 560px 的理由（算出来的，不是拍的）：
        本页最宽的两个表单控件是「网络代理地址」与「拦截域名」，都是 `sm:w-80`
        （320px）；而 `SettingsFieldRow` 是"标签在左 `flex-1`、控件在右 `shrink-0`"
        的两端布局 ⇒ 卡片窄于 ~520px 时标签只剩几十像素，描述会挤成竖条。

        按所有者那台 2340px 屏算：减 220px 侧栏、再减 `sm:px-8` 的 64px 内边距
        ⇒ 可用 **2056px**。`n*560 + (n-1)*24 ≤ 2056` 解得 **3 列、每列约 669px**。

        ⚠ 与改动前**逐项对比**（不写"基本一致"这种含糊话）：
        改动前 `max-w-3xl` = 768px **含** `sm:px-6`（两侧 24px）
        ⇒ 卡片 720px；`SettingsRow` 的 `sm:mx-5` 再两侧各 20px ⇒ 行内 680px；
        减去 320px 控件与 12px 间隙 ⇒ 标签可用 **348px**。
        改动后每列 669px ⇒ 行内 629px ⇒ 标签可用 **297px**。
        即标签区窄约 **15%**。本页最长的一句描述
        （「签到日志、积分快照与任务记录都会按此天数清理；超出部分在下次写入时
        自动删除。」38 字）在 `text-xs`(12px) 下由约 29 字/行变为约 25 字/行，
        即折 2 行 —— 仍可读，只是比原来紧一些。

        下界在 (496, 669] 区间内都会解出同样的 3 列，560 距下沿 64px、
        距上沿 109px，落在带宽中部，故窗口宽度略有出入时不会在临界点上跳列数。
        其他分辨率下的退化（已算过）：
          2340px 屏 → 可用 2056px → **3 列**，每列 669px
          1920px 屏 → 可用 1636px → **2 列**，每列 806px
          1600px 屏 → 可用 1316px → **2 列**，每列 646px
          ≤1427px  → **1 列**占满（与窄窗口的单列观感一致）

        `min(100%,560px)` 而不是裸 `560px`：窗口最小可到 720px
        （`tauri.conf.json` 的 `minWidth`），减侧栏与内边距只剩约 436px，
        裸 560px 会撑出横向滚动条；套一层 `min()` 后退化成"一列占满"。

        ⚠ 代价（如实记下）：`items-start` 让每张卡片保持自身高度，故同一行里
        一高一矮时，矮的那张下方会空出来 —— 本页卡片高度差异不小
        （`ProxyLogsPanel` 带表格与筛选器，`AppearanceCard` 只有一行）。

        ★★ 2026-10-01 改为 **CSS 多列（瀑布流）** —— 上面那个"代价"所有者不接受。

        所有者原话：「这个设置还是留白多,不好看」，并附了截图：
        `外观` 卡片只有约 45px 高，而同一行的 `应用环境` 有约 425px ——
        `items-start` 让前者**不拉伸**，于是它下面空了约 465px。
        这种高度差（45px ~ 570px）下，**任何**「按行对齐」的 grid 都必然留洞：
        `items-start` 留洞、`stretch` 则把 45px 的卡片拉成 570px（更难看）。

        `columns-*` 是唯一不留洞的做法：它把内容按**列**填充，
        每列内部自上而下紧密堆叠，短卡片下面立刻接下一张。
        代价是**阅读顺序变成"先填满第一列，再第二列"**（纵向优先），
        而 grid 是"从左到右再换行"（横向优先）。

        ⚠ 为什么这个顺序变化可接受：本页每张卡片自成一组、**彼此无先后依赖**
        （不像向导或步骤条）。所有者要的是"别留洞"，顺序换向是必要代价。

        ⚠ 两个必须一起加的类，缺一不可：
          · `break-inside-avoid` —— 否则一张卡片会被**从中间劈开**跨到下一列，
            这是多列布局最典型的坑（卡片带边框时尤其明显）；
          · `mb-*` 而不是 `gap-y-*` —— `gap` 在多列布局里只作用于**列间距**，
            纵向间距必须靠子元素自己的 margin。
      */}
      <div className="columns-1 gap-6 [column-fill:balance] sm:columns-2 xl:columns-3">
        <div className="mb-6 break-inside-avoid">
          <AppearanceCard />
        </div>
        <div className="mb-6 break-inside-avoid">
          <NetworkProxyCard />
        </div>
        <div className="mb-6 break-inside-avoid">
          <ProxyCard />
        </div>
        {/* 代理请求日志：紧跟在「本地代理」之后 —— 它是那个代理抓到的
            结构化请求记录（可筛选、可看详情），与上面卡片里的原始文本日志
            是同一件事的两个视角，分开太远用户找不到。

            ⚠ 多列布局带来的新问题：它是本页**唯一**没有用 `SettingsGroup`
            包的卡片（标题「抓包日志」在卡片**内部**，其它卡片的标题在卡片
            **外面**、由 `SettingsGroup` 渲染）。那个外置标题行高 30px
            （`leading-5` 的 20px + `space-y-2.5` 的 10px），于是并排时
            本卡片的上边缘会比同排邻居**高 30px**。

            单列时看不出来（上下堆叠没有可比的边缘），并排就很显眼。
            故补一行等高占位（`h-5` + `space-y-2.5` = 同样 30px）把它压齐；
            占位行 `aria-hidden`，不产生多余的标题语义。

            ⚠ 2026-10-01 改多列瀑布流后，这个 30px 占位**仍然必要**：
            它保证本卡片的**内容**与其它卡片的**卡片体**上边缘对齐
            （别的卡片标题在 `SettingsGroup` 里、占 30px，本卡片没有标题行）。 */}
        <div className="mb-6 break-inside-avoid">
          <div className="min-w-0 space-y-2.5">
            <div className="h-5" aria-hidden="true" />
            <ProxyLogsPanel />
          </div>
        </div>
        {/* 其余卡片统一包一层：`mb-6` 提供纵向间距、`break-inside-avoid`
            防止卡片被从中间劈开跨列（多列布局最典型的坑）。 */}
        <div className="mb-6 break-inside-avoid">
          <ScheduledTaskCard />
        </div>
        <div className="mb-6 break-inside-avoid">
          <RecordRetentionCard />
        </div>
        {api.isDesktop() || api.isDemoMode() ? (
          <div className="mb-6 break-inside-avoid">
            <StartupCard />
          </div>
        ) : null}
        {api.isWebui() && !api.isDemoMode() ? null : (
          <div className="mb-6 break-inside-avoid">
            <UpdateCard />
          </div>
        )}
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
