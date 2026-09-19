import { useCallback, useEffect, useMemo, useState } from "react";
import { Link } from "react-router-dom";
import {
  AlertTriangle,
  Bot,
  Check,
  CheckCircle2,
  ChevronDown,
  ChevronUp,
  Copy,
  Cpu,
  History,
  Laptop,
  Loader2,
  Network,
  Plus,
  RefreshCw,
  RotateCcw,
  Wand2,
  Zap,
} from "lucide-react";
import { toast } from "sonner";

import claudeIcon from "@/assets/agent-icons/claude.svg";
import codexIcon from "@/assets/agent-icons/codex.svg";
import deepseekIcon from "@/assets/agent-icons/deepseek.svg";
import grokIcon from "@/assets/agent-icons/grok.svg";
import hermesIcon from "@/assets/agent-icons/hermes.png";
import kimiIcon from "@/assets/agent-icons/kimi-light.svg";
import minimaxIcon from "@/assets/agent-icons/minimax-code.svg";
import openclawIcon from "@/assets/agent-icons/openclaw.svg";
import opencodeIcon from "@/assets/agent-icons/opencode.svg";
import piIcon from "@/assets/agent-icons/pi-logo-on-light.svg";
import zcodeIcon from "@/assets/agent-icons/zcode.png";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import { ModelCapabilityRow } from "@/components/model-capability-chip";
import * as api from "@/lib/api";
import { capabilityViewOf, summarizeCapabilities } from "@/lib/model-capability";
import { accentOf } from "@/lib/client-accent";
import type {
  AgentBackupItem,
  AgentClientTarget,
  AgentDetectionResult,
  GatewayModelItem,
  GatewayStatus,
} from "@/lib/types";
import { cn } from "@/lib/utils";

/** 客户端图标映射 */
const CLIENT_ICONS: Record<string, string> = {
  "claude-code": claudeIcon,
  "claude-desktop": claudeIcon,
  codex: codexIcon,
  dsh: deepseekIcon,
  opencode: opencodeIcon,
  pi: piIcon,
  "grok-build": grokIcon,
  zcode: zcodeIcon,
  "kimi-code": kimiIcon,
  openclaw: openclawIcon,
  hermes: hermesIcon,
  "minimax-code": minimaxIcon,
};

/** Claude 客户端的模型槽位上限：Sonnet / Opus / Haiku / Fable。 */
const CLAUDE_SLOT_LIMIT = 4;

/** 使用 Anthropic Messages 协议、只支持固定槽位的客户端。 */
const CLAUDE_PROTOCOL_TARGETS = new Set(["claude-code", "claude-desktop"]);

/** 客户端协议与说明 */
interface ClientMeta {
  protocol: string;
  protocolBadge: string;
  description: string;
}

const CLIENT_METAS: Record<string, ClientMeta> = {
  "claude-code": {
    protocol: "Anthropic Messages",
    protocolBadge: "POST /v1/messages",
    description: "Anthropic 官方终端助手。四模型槽位（Sonnet/Opus/Haiku/Fable）直写真实模型名，网关零转译转发。",
  },
  "claude-desktop": {
    protocol: "Anthropic Messages (3P)",
    protocolBadge: "3P Gateway",
    description: "Claude 桌面客户端第三方网关模式。自动配置 Sonnet/Opus/Haiku/Fable 四槽位与鉴权凭据。",
  },
  codex: {
    protocol: "OpenAI Responses",
    protocolBadge: "POST /v1/responses",
    description: "Codex CLI 终端助手。完美兼容 0.146+ 版本强制要求的 Responses API 协议。",
  },
  dsh: {
    protocol: "OpenAI Chat",
    protocolBadge: "POST /v1/chat/completions",
    description: "DeepSeek Harness 架构工具。通过标准 OpenAI Chat 协议直连网关。",
  },
  opencode: {
    protocol: "OpenAI Chat",
    protocolBadge: "POST /v1/chat/completions",
    description: "开源 AI 编程终端与桌面环境。自动写入 provider.workbuddy 兼容配置并支持多模型。",
  },
  pi: {
    protocol: "OpenAI Chat",
    protocolBadge: "POST /v1/chat/completions",
    description: "极简终端编程助手。通过 providers.workbuddy 协议提供方无缝挂载多模型接入。",
  },
  "grok-build": {
    protocol: "OpenAI Responses",
    protocolBadge: "POST /v1/responses",
    description: "Grok 命令行研发工具。自动设置 default 模型及每个选定模型的 [model.\"...\"] 配置表。",
  },
  zcode: {
    protocol: "OpenAI Chat",
    protocolBadge: "POST /v1/chat/completions",
    description: "ZCode 编程助理。自动写入 v2 规范 provider 架构配置与全套多选模型参数。",
  },
  "kimi-code": {
    protocol: "OpenAI Chat",
    protocolBadge: "POST /v1/chat/completions",
    description: "Kimi Code 终端助手。写入 default_model 与每个选中模型的 [models.\"...\"] 协议表。",
  },
  openclaw: {
    protocol: "OpenAI Chat",
    protocolBadge: "POST /v1/chat/completions",
    description: "OpenClaw 多智能体框架。自动写入 models.providers 并激活默认主模型。",
  },
  hermes: {
    protocol: "OpenAI Chat",
    protocolBadge: "POST /v1/chat/completions",
    description: "Hermes Agent 终端助手。自动在 custom_providers 列表挂载所有选定模型。",
  },
  "minimax-code": {
    protocol: "Anthropic Messages",
    protocolBadge: "POST /v1/messages",
    description:
      "MiniMax Code 桌面客户端。在 provider 块注册本网关（Anthropic 协议适配器），并把 defaultModel 指向它；官方 MiniMax 账号配置原样保留。",
  },
};

type FilterTab = "all" | "installed" | "configured" | "uninstalled";

function StatCard({
  label,
  value,
  subtext,
  tone = "default",
  icon: Icon,
}: {
  label: string;
  value: React.ReactNode;
  subtext?: string;
  tone?: "default" | "ok" | "warn" | "accent";
  icon?: React.ComponentType<{ className?: string }>;
}) {
  return (
    <Card className="flex items-center justify-between rounded-xl border border-border/60 bg-card p-3.5 shadow-none sm:p-4">
      <div className="min-w-0 space-y-0.5">
        <div className="text-[11.5px] font-medium text-muted-foreground">{label}</div>
        <div
          className={cn(
            "text-xl font-bold tracking-tight tabular-nums sm:text-2xl",
            tone === "ok" && "text-emerald-600 dark:text-emerald-400",
            tone === "warn" && "text-amber-600 dark:text-amber-400",
            tone === "accent" && "text-primary",
            tone === "default" && "text-foreground",
          )}
        >
          {value}
        </div>
        {subtext && <div className="text-[10.5px] text-muted-foreground">{subtext}</div>}
      </div>
      {Icon && (
        <div className="flex size-9 shrink-0 items-center justify-center rounded-xl bg-muted/40 text-muted-foreground">
          <Icon className="size-4.5" />
        </div>
      )}
    </Card>
  );
}

export default function AgentsPage() {
  const [loading, setLoading] = useState(false);
  const [detection, setDetection] = useState<AgentDetectionResult | null>(null);
  const [gatewayStatus, setGatewayStatus] = useState<GatewayStatus | null>(null);
  const [gatewayModels, setGatewayModels] = useState<GatewayModelItem[]>([]);
  const [loadingModels, setLoadingModels] = useState(false);

  // 全局模型多选（从网关动态拉取）
  const [selectedGlobalModels, setSelectedGlobalModels] = useState<string[]>([]);

  // 每个客户端单独定制的模型覆盖
  const [customTargetModels, setCustomTargetModels] = useState<Record<string, string[]>>({});
  const [expandedTargetModels, setExpandedTargetModels] = useState<Record<string, boolean>>({});

  // 用户手动添加模型
  const [customInputModel, setCustomInputModel] = useState("");

  const [operatingTarget, setOperatingTarget] = useState<string | null>(null);
  const [batchUpdating, setBatchUpdating] = useState(false);
  const [filterTab, setFilterTab] = useState<FilterTab>("all");

  // 备份管理对话框
  const [backupDialogOpen, setBackupDialogOpen] = useState(false);
  const [activeBackupTarget, setActiveBackupTarget] = useState<AgentClientTarget | null>(null);
  const [backups, setBackups] = useState<AgentBackupItem[]>([]);
  const [loadingBackups, setLoadingBackups] = useState(false);
  const [restoringBackupId, setRestoringBackupId] = useState<string | null>(null);

  // 读取客户端检测与网关信息
  const fetchData = useCallback(async (quiet = false) => {
    if (!quiet) setLoading(true);
    try {
      const [detRes, statusRes] = await Promise.all([
        api.detectAgentClients(),
        api.getGatewayStatus().catch(() => null),
      ]);
      setDetection(detRes);
      if (statusRes) setGatewayStatus(statusRes);
    } catch (err) {
      if (!quiet) toast.error("读取客户端状态失败: " + String(err));
    } finally {
      if (!quiet) setLoading(false);
    }
  }, []);

  // 从网关动态拉取模型列表（无静态预设，纯上游拉取 + 权威去重）
  const fetchModels = useCallback(async () => {
    setLoadingModels(true);
    try {
      const list = await api.getGatewayModels();
      if (list && list.length > 0) {
        // 全局按 ID 去重，消除重复项，统一呈现
        const uniqueList = Array.from(new Map(list.map((m) => [m.id, m])).values());
        setGatewayModels(uniqueList);
        setSelectedGlobalModels((prev) => {
          const valid = prev.filter((id) => uniqueList.some((m) => m.id === id));
          return valid.length > 0 ? valid : [uniqueList[0].id];
        });
      } else {
        setGatewayModels([]);
      }
    } catch (err) {
      console.warn("拉取网关模型失败:", err);
      setGatewayModels([]);
    } finally {
      setLoadingModels(false);
    }
  }, []);

  useEffect(() => {
    void fetchData();
    void fetchModels();
  }, [fetchData, fetchModels]);

  const copyToClipboard = async (text: string, label: string) => {
    try {
      await navigator.clipboard.writeText(text);
      toast.success(`已复制 ${label}`);
    } catch {
      toast.error("复制失败，请手动选择复制");
    }
  };

  // 添加自定义模型
  const handleAddCustomModel = (e: React.FormEvent) => {
    e.preventDefault();
    const id = customInputModel.trim();
    if (!id) return;
    if (gatewayModels.some((m) => m.id === id)) {
      toast.info("该模型已在列表中");
    } else {
      setGatewayModels((prev) => [
        { id, name: id, owned_by: "custom" },
        ...prev,
      ]);
    }
    if (!selectedGlobalModels.includes(id)) {
      setSelectedGlobalModels((prev) => [id, ...prev]);
    }
    setCustomInputModel("");
    toast.success(`已添加并选中模型: ${id}`);
  };

  // 快速选择模型列表
  const selectModels = (list: string[]) => {
    setSelectedGlobalModels(list);
  };

  // 切换全局模型多选状态
  const toggleGlobalModel = (modelId: string) => {
    setSelectedGlobalModels((prev) => {
      if (prev.includes(modelId)) {
        if (prev.length === 1) {
          toast.warning("至少需保留一个目标模型");
          return prev;
        }
        return prev.filter((id) => id !== modelId);
      } else {
        return [...prev, modelId];
      }
    });
  };

  // 获取当前 target 分配的模型列表
  const getModelsForTarget = (targetId: string): string[] => {
    return customTargetModels[targetId] ?? selectedGlobalModels;
  };

  // 切换当前 target 的模型多选
  const toggleTargetModel = (targetId: string, modelId: string) => {
    const current = getModelsForTarget(targetId);
    let next: string[];
    if (current.includes(modelId)) {
      if (current.length === 1) {
        toast.warning("至少需保留一个目标模型");
        return;
      }
      next = current.filter((id) => id !== modelId);
    } else {
      next = [...current, modelId];
    }
    setCustomTargetModels((prev) => ({ ...prev, [targetId]: next }));
  };

  // 单个接入 / 更新配置
  const handleImport = async (target: AgentClientTarget) => {
    const hasKey = Boolean(gatewayStatus?.config?.api_key || detection?.hasApiKey);
    if (!hasKey) {
      toast.error("请先在「兼容网关」页面配置 API Key，客户端必须携带 API Key 鉴权");
      return;
    }

    const models = getModelsForTarget(target.id);
    if (models.length === 0) {
      toast.error("请至少选择一个目标模型");
      return;
    }

    setOperatingTarget(target.id);
    try {
      const outcome = await api.importAgentClient(target.id, models);
      toast.success(`已成功更新 ${target.label}`, {
        description: `已写入 ${outcome.models?.length ?? models.length} 个模型配置（默认主模型: ${models[0]}），已自动安全备份`,
      });
      await fetchData(true);
    } catch (err) {
      toast.error(`接入 ${target.label} 失败: ` + String(err));
    } finally {
      setOperatingTarget(null);
    }
  };

  // 一键更新所有已安装客户端
  const handleBatchUpdate = async () => {
    const targets = detection?.targets.filter((t) => t.installed) ?? [];
    if (targets.length === 0) {
      toast.error("未检测到已安装的客户端");
      return;
    }
    const hasKey = Boolean(gatewayStatus?.config?.api_key || detection?.hasApiKey);
    if (!hasKey) {
      toast.error("请先在网关设置中配置 API Key，客户端必须携带 API Key 鉴权");
      return;
    }

    setBatchUpdating(true);
    let successCount = 0;
    let failCount = 0;

    for (const target of targets) {
      const models = getModelsForTarget(target.id);
      try {
        await api.importAgentClient(target.id, models);
        successCount++;
      } catch {
        failCount++;
      }
    }

    setBatchUpdating(false);
    await fetchData(true);

    if (failCount === 0) {
      toast.success(`一键更新完成！已成功将 ${successCount} 个客户端同步更新为当前最新网关配置与多模型组合`);
    } else {
      toast.info(`更新完毕：${successCount} 个成功，${failCount} 个失败`);
    }
  };

  // 打开备份弹窗
  const handleOpenBackups = async (target: AgentClientTarget) => {
    setActiveBackupTarget(target);
    setBackupDialogOpen(true);
    setLoadingBackups(true);
    try {
      const res = await api.listAgentBackups(target.id);
      setBackups(res.backups || []);
    } catch (err) {
      toast.error("读取备份记录失败: " + String(err));
      setBackups([]);
    } finally {
      setLoadingBackups(false);
    }
  };

  // 恢复备份
  const handleRestore = async (backupId?: string) => {
    if (!activeBackupTarget) return;
    setRestoringBackupId(backupId ?? "latest");
    try {
      const res = await api.restoreAgentClient(activeBackupTarget.id, backupId);
      toast.success(`已成功恢复 ${activeBackupTarget.label} 的配置`, {
        description: `已回滚 ${res.restored} 个配置文件至备份状态`,
      });
      setBackupDialogOpen(false);
      await fetchData(true);
    } catch (err) {
      toast.error("恢复配置失败: " + String(err));
    } finally {
      setRestoringBackupId(null);
    }
  };

  const hasApiKey = Boolean(gatewayStatus?.config?.api_key || detection?.hasApiKey);
  const targets = detection?.targets ?? [];

  const installedCount = useMemo(() => targets.filter((t) => t.installed).length, [targets]);
  const configuredCount = useMemo(() => targets.filter((t) => t.configured).length, [targets]);

  const filteredTargets = useMemo(() => {
    switch (filterTab) {
      case "installed":
        return targets.filter((t) => t.installed);
      case "configured":
        return targets.filter((t) => t.configured);
      case "uninstalled":
        return targets.filter((t) => !t.installed);
      default:
        return targets;
    }
  }, [targets, filterTab]);

  /**
   * 每个模型的能力视图（上下文 / 视觉 / 思考档位 / 区域）。
   *
   * 只算一次并缓存：这三项在渲染模型列表与客户端卡片上的模型标签时都要用，
   * 而 `capabilityViewOf` 内部要按多种拼写嗅探字段，逐处重算既浪费也让
   * 「同一个模型在两处显示不一致」成为可能。
   */
  const capabilityViews = useMemo(
    () => new Map(gatewayModels.map((m) => [m.id, capabilityViewOf(m)])),
    [gatewayModels],
  );

  /**
   * 能力真值覆盖率。
   *
   * 用途：单个模型显示「—」时，用户无法判断这是**那个模型**没声明，还是网关
   * 整体拉不到真值。给出计数后两种情形一眼可分 —— 前者是模型自身情况，
   * 后者要去查网关/上游（此时「全选」这类操作的风险也更高）。
   */
  const capabilitySummary = useMemo(
    () => summarizeCapabilities(gatewayModels),
    [gatewayModels],
  );

  /** 客户端卡片上模型 chip 的悬浮说明（只讲视觉与区域，见 ModelCapabilityRow 的注释）。 */
  const capabilityHintOf = useCallback(
    (modelId: string): string | undefined => {
      const view = capabilityViews.get(modelId);
      if (!view) return undefined;
      const parts = [
        // 「未知」时补一句「未声明」：只显示「—」的话，用户不知道它是
        // 「拿不到」还是「这个模型就是这么写的」。
        view.vision.unknown ? `视觉未声明（${view.vision.text}）` : view.vision.text,
      ];
      if (view.region) parts.push(view.region.label);
      return parts.join(" · ");
    },
    [capabilityViews],
  );

  /**
   * 一个模型来自哪些平台（供窄 chip 的悬浮说明用）。
   *
   * 需求：「可以加一个渠道，是来自于哪个平台，如果重叠，就显示多个平台」。
   *
   * 大卡片上直接显示 chip（见 data-slot="agent-model-channels"），
   * 这里只是给**窄 chip** 用的文字版 —— 那里放不下标签，
   * 但悬浮时用户仍要知道这个模型来自哪。
   *
   * 未声明（旧网关没这个字段）时返回 undefined，而不是"未知平台"——
   * 后者会让用户以为网关知道但没告诉我们。
   */
  const channelSummaryOf = useCallback(
    (m: GatewayModelItem): string | undefined => {
      if (!m.channels || m.channels.length === 0) return undefined;
      return `来自 ${m.channels.map((c) => c.label).join(" / ")}`;
    },
    [],
  );

  return (
    <div className="mx-auto max-w-6xl space-y-5 p-4 sm:p-6">
      {/* 顶部主横幅与一键更新操作栏 */}
      <div className="flex flex-col gap-3 rounded-2xl border border-border/70 bg-gradient-to-br from-card via-card to-muted/20 p-5 shadow-xs sm:flex-row sm:items-center sm:justify-between">
        <div className="space-y-1">
          <div className="flex items-center gap-2.5">
            <div className="flex size-9 items-center justify-center rounded-xl bg-primary/10 text-primary">
              <Bot className="size-5" />
            </div>
            <div>
              <h1 className="text-lg font-semibold tracking-tight">智能体工作台</h1>
              <p className="text-xs text-muted-foreground">
                管理与接入本机已安装的 AI 编码助手，自动配置网关端点、多选模型与鉴权凭据。
              </p>
            </div>
          </div>
        </div>

        <div className="flex flex-wrap items-center gap-2.5">
          <Button
            size="sm"
            className="h-8 gap-1.5 px-3.5 text-xs font-medium shadow-xs"
            disabled={batchUpdating || !hasApiKey || installedCount === 0}
            onClick={() => void handleBatchUpdate()}
          >
            {batchUpdating ? (
              <Loader2 className="size-3.5 animate-spin" />
            ) : (
              <Zap className="size-3.5 text-amber-300 fill-amber-300" />
            )}
            一键更新所有已安装智能体
          </Button>

          <Button
            variant="outline"
            size="sm"
            className="h-8 gap-1.5 text-xs text-muted-foreground"
            onClick={() => {
              void fetchData();
              void fetchModels();
            }}
            disabled={loading || batchUpdating}
          >
            <RefreshCw className={cn("size-3.5", loading && "animate-spin")} />
            刷新状态
          </Button>
        </div>
      </div>

      {/* 4 维核心运行态数据卡 */}
      <div className="grid grid-cols-2 gap-3 sm:grid-cols-4 sm:gap-4">
        <StatCard
          label="已接入客户端"
          value={`${configuredCount} / ${targets.length}`}
          subtext="已绑定网关服务"
          tone={configuredCount > 0 ? "ok" : "default"}
          icon={CheckCircle2}
        />
        <StatCard
          label="本机已安装"
          value={`${installedCount} / ${targets.length}`}
          subtext="检测到客户端环境"
          tone="accent"
          icon={Laptop}
        />
        <StatCard
          label="上游可用模型"
          value={loadingModels ? "..." : gatewayModels.length}
          subtext="来自网关实时同步"
          tone="default"
          icon={Cpu}
        />
        <StatCard
          label="网关服务状态"
          value={gatewayStatus?.running ? "在线" : "未启动"}
          subtext={gatewayStatus?.running ? `端口 :${gatewayStatus?.port || 7863}` : "点击可前往启动"}
          tone={gatewayStatus?.running ? "ok" : "warn"}
          icon={Network}
        />
      </div>

      {/* 缺少 API Key 时的提示 */}
      {!hasApiKey && (
        <Alert variant="destructive" className="border-amber-500/50 bg-amber-500/10 text-amber-900 dark:text-amber-200">
          <AlertTriangle className="size-4 text-amber-600 dark:text-amber-400" />
          <AlertTitle className="text-xs font-semibold">网关尚未配置 API Key</AlertTitle>
          <AlertDescription className="text-xs">
            Claude Code、Codex、OpenCode 等客户端请求必须携带 Authorization 凭据才能通过网关鉴权。请先在「兼容网关」页面填写并保存 API Key。
          </AlertDescription>
        </Alert>
      )}

      {/* 多模型分发中心（从网关动态获取） */}
      <Card className="rounded-xl border border-border/70 p-4 shadow-none">
        <div className="space-y-3">
          <div className="flex flex-wrap items-center justify-between gap-2 border-b border-border/50 pb-2.5">
            <div>
              <div className="flex items-center gap-2">
                <Cpu className="size-4 text-primary" />
                <span className="text-[13px] font-semibold">分发模型配置（从网关动态获取）</span>
                <Badge variant="secondary" className="font-mono text-[10px]">
                  {loadingModels ? "正在拉取网关模型..." : `已获取 ${gatewayModels.length} 个上游模型`}
                </Badge>
              </div>
              <p className="mt-0.5 text-xs text-muted-foreground">
                选中的模型将批量注入各智能体配置文件；排在第 1 位的模型自动作为默认主模型。
              </p>
              {/*
                能力覆盖率说明。
                为什么必须给「已知 / 总数」而不是不写：单个模型显示「—」时用户分不清
                是**那个模型**没声明，还是网关整体拿不到真值（后者要去查网关与上游）。
                只统计**视觉**：上下文与思考档位已按所有者要求从模型卡片撤下，
                统计它们会让用户去找一个界面上并不存在的标记。
              */}
              {gatewayModels.length > 0 && (
                <p className="mt-0.5 text-[11px] text-muted-foreground" data-slot="model-capability-summary">
                  视觉能力真值覆盖：{capabilitySummary.visionKnown}/{capabilitySummary.total}
                  {capabilitySummary.singleRegion > 0
                    ? ` · ${capabilitySummary.singleRegion} 个模型仅单区可用`
                    : ""}
                  ；显示「—」表示上游未声明（并非不支持）。
                </p>
              )}
            </div>

            {/* 快捷选择预设 */}
            {gatewayModels.length > 0 && (
              <div className="flex items-center gap-1.5 text-xs">
                <Button
                  variant="ghost"
                  size="sm"
                  className="h-6 px-2 text-[11px]"
                  onClick={() => {
                    selectModels(gatewayModels.map((m) => m.id));
                  }}
                >
                  全选 ({gatewayModels.length})
                </Button>
                <Button
                  variant="ghost"
                  size="sm"
                  className="h-6 px-2 text-[11px] text-muted-foreground"
                  onClick={() => {
                    selectModels([]);
                  }}
                >
                  清空
                </Button>
              </div>
            )}
          </div>

          {/* 模型多选列表或空状态提示 */}
          {gatewayModels.length === 0 ? (
            <div className="rounded-xl border border-dashed border-border/70 bg-muted/15 p-6 text-center space-y-2.5">
              <Cpu className="mx-auto size-7 text-muted-foreground/60" />
              <div className="text-sm font-medium">尚未从网关获取到上游模型</div>
              <p className="text-xs text-muted-foreground max-w-md mx-auto">
                网关尚未启动或正在同步。请前往「兼容网关」页面启动网关服务，网关启动后将自动从官方上游拉取您账号下的全部真实可用模型。
              </p>
              <div className="flex items-center justify-center gap-2 pt-1">
                <Button size="sm" variant="default" asChild className="h-7 text-xs">
                  <Link to="/gateway">前往「兼容网关」启动网关 →</Link>
                </Button>
                <Button size="sm" variant="outline" onClick={() => void fetchModels()} disabled={loadingModels} className="h-7 text-xs">
                  <RefreshCw className={cn("mr-1 size-3", loadingModels && "animate-spin")} />
                  重新检查
                </Button>
              </div>
            </div>
          ) : (
            /*
              模型列表：每个模型一张小卡，卡内展示**三项能力**。
              为什么从「一行一个 chip」改成网格卡：需求是「看到模型所支持的上下文是多少、
              是否是视觉模型、思考强度支持哪些」。三行挤在原来那种扁 chip 里会截断
              档位列表（而所有者明确要求信息不能为了清爽而丢），所以每张卡给足两行。
              列表高度也跟着放大（max-h-56 → max-h-96）：卡片变高后原来的高度
              只能装下 2 行，「模型一多就看不到下面」这个既有反馈会立刻复现。
            */
            <div className="grid grid-cols-1 gap-2 pt-1 max-h-96 overflow-y-auto p-0.5 sm:grid-cols-2 lg:grid-cols-3">
              {gatewayModels.map((m, index) => {
                const selected = selectedGlobalModels.includes(m.id);
                const isPrimary = selectedGlobalModels[0] === m.id;
                // 每张卡都需要能力视图；map 里查表而不是现算，保证与
                // capabilitySummary 的口径完全一致（同一份 view）。
                const view = capabilityViews.get(m.id) ?? capabilityViewOf(m);

                return (
                  <button
                    key={m.id}
                    type="button"
                    data-slot="agent-model-card"
                    data-model={m.id}
                    data-selected={selected ? "true" : "false"}
                    data-primary={isPrimary ? "true" : "false"}
                    onClick={() => toggleGlobalModel(m.id)}
                    className={cn(
                      "flex flex-col gap-1.5 rounded-lg border px-2.5 py-2 text-left text-xs transition-all",
                      selected
                        ? "border-primary/50 bg-primary/10 text-foreground shadow-2xs"
                        : "border-border/60 bg-muted/20 text-muted-foreground hover:bg-muted/40 hover:text-foreground",
                    )}
                  >
                    <div className="flex items-start gap-1.5">
                      <div
                        className={cn(
                          "mt-0.5 flex size-3.5 shrink-0 items-center justify-center rounded-sm border text-[9px]",
                          selected ? "border-primary bg-primary text-primary-foreground" : "border-muted-foreground/50",
                        )}
                      >
                        {selected && <Check className="size-2.5 stroke-[3]" />}
                      </div>

                      <span
                        className={cn("min-w-0 flex-1 break-all font-mono", selected && "font-medium")}
                        title={m.id}
                      >
                        {m.id}
                      </span>

                      {/* 序号：与卡片下方的「默认主模型」一起说明「第 1 位」是怎么算的 */}
                      <span className="shrink-0 text-[9px] tabular-nums text-muted-foreground/70">
                        #{index + 1}
                      </span>
                    </div>

                    {isPrimary && (
                      <Badge variant="default" className="h-4 w-fit px-1 text-[9px] font-normal">
                        默认主模型
                      </Badge>
                    )}

                    {/* 能力展示：**只保留视觉**。
                        上下文与思考档位已按所有者要求撤下（2026-09-18）：
                          · 上下文窗口由上游 /v3/config 的 maxInputTokens 给出，
                            但它对「该选哪个模型」几乎没有决策价值，占位却最宽；
                          · 思考档位的「可选范围」上游并无逐模型清单
                            （实测 supportedEfforts 不是硬范围：hy3 声明 [low,high]
                            却接受 medium/max/off），在我们给出可靠范围之前，
                            显示它只会误导 —— 宁可不显示。
                        相关字段仍由网关下发（/v1/models 的 supported_efforts 等），
                        需要时可随时恢复展示。 */}
                    <ModelCapabilityRow vision={view.vision} region={view.region} />

                    {/* 渠道：这个模型来自哪个平台。
                        所有者的需求：「可以加一个渠道，是来自于哪个平台，
                        如果重叠，就显示多个平台」。

                        只在下发时展示（旧网关没有这个字段 = 未声明）。
                        重叠时每个平台一个 chip —— 那正是需求要的表达。 */}
                    {m.channels && m.channels.length > 0 && (
                      <div
                        className="flex flex-wrap items-center gap-1"
                        data-slot="agent-model-channels"
                      >
                        {m.channels.map((ch) => (
                          <span
                            key={ch.product}
                            className="rounded border border-border/60 bg-muted/50 px-1 py-px text-[9px] leading-4 text-muted-foreground"
                            title={
                              ch.regions && ch.regions.length > 0
                                ? `${ch.label}（${ch.regions.join(" / ")}）`
                                : ch.label
                            }
                          >
                            {ch.label}
                          </span>
                        ))}
                      </div>
                    )}
                  </button>
                );
              })}
            </div>
          )}

          {/* 手动添加自定义模型 */}
          <form onSubmit={handleAddCustomModel} className="flex items-center gap-2 pt-2 border-t border-border/40">
            <Input
              type="text"
              placeholder="手动补充指定模型名称（若上游刚上线新模型）"
              value={customInputModel}
              onChange={(e) => setCustomInputModel(e.target.value)}
              className="h-8 max-w-sm text-xs font-mono"
            />
            <Button type="submit" size="sm" variant="secondary" className="h-8 text-xs">
              <Plus className="mr-1 size-3.5" />
              添加模型
            </Button>
          </form>
        </div>
      </Card>

      {/* 智能体客户端列表 */}
      <div className="space-y-3">
        {/* 筛选标签栏 */}
        <div className="flex flex-wrap items-center justify-between gap-2 border-b border-border/50 pb-2 text-xs">
          <div className="flex items-center gap-1.5">
            <button
              type="button"
              onClick={() => setFilterTab("all")}
              className={cn(
                "rounded-md px-2.5 py-1 transition-colors",
                filterTab === "all"
                  ? "bg-secondary font-medium text-foreground"
                  : "text-muted-foreground hover:text-foreground",
              )}
            >
              全部客户端 ({targets.length})
            </button>
            <button
              type="button"
              onClick={() => setFilterTab("installed")}
              className={cn(
                "rounded-md px-2.5 py-1 transition-colors",
                filterTab === "installed"
                  ? "bg-secondary font-medium text-foreground"
                  : "text-muted-foreground hover:text-foreground",
              )}
            >
              已检测到安装 ({installedCount})
            </button>
            <button
              type="button"
              onClick={() => setFilterTab("configured")}
              className={cn(
                "rounded-md px-2.5 py-1 transition-colors",
                filterTab === "configured"
                  ? "bg-secondary font-medium text-foreground"
                  : "text-muted-foreground hover:text-foreground",
              )}
            >
              已接入网关 ({configuredCount})
            </button>
            <button
              type="button"
              onClick={() => setFilterTab("uninstalled")}
              className={cn(
                "rounded-md px-2.5 py-1 transition-colors",
                filterTab === "uninstalled"
                  ? "bg-secondary font-medium text-foreground"
                  : "text-muted-foreground hover:text-foreground",
              )}
            >
              未安装 ({targets.length - installedCount})
            </button>
          </div>

          <div className="text-[11px] text-muted-foreground">
            支持 12 类智能体的一键写入、模型多选与历史无损回滚
          </div>
        </div>

        {/* 客户端卡片网格 */}
        <div className="grid grid-cols-1 gap-3.5 sm:grid-cols-2 lg:grid-cols-3">
          {filteredTargets.map((target) => {
            const meta = CLIENT_METAS[target.id] ?? {
              protocol: "通用兼容",
              protocolBadge: "API",
              description: "AI 客户端集成支持",
            };
            const iconSrc = CLIENT_ICONS[target.id] || claudeIcon;
            const isOperating = operatingTarget === target.id || batchUpdating;
            const assignedModels = getModelsForTarget(target.id);
            const isExpanded = Boolean(expandedTargetModels[target.id]);
            // 每个客户端一个色相（所有者的要求：「不同的客户端要采用不同的颜色
            // 加以区分」）。此前 12 张卡片外观完全相同，扫一眼分不出谁是谁。
            //
            // ⚠ 色相只表达"这是哪个客户端"，**不承载"是否已接入"** ——
            // 后者由边框/背景表达（既有做法）。两个维度正交，
            // 否则用户无法同时看出"这是 Codex"与"它已接入"。
            const accent = accentOf(target.id);

            return (
              <Card
                key={target.id}
                data-slot="agent-client-card"
                data-client={target.id}
                className={cn(
                  "flex flex-col justify-between rounded-xl border p-4 transition-all",
                  // 色相：左侧一道色条 + 图标底色，让同类客户端一眼可辨
                  accent.border,
                  accent.bg,
                  "hover:brightness-[1.02]",
                  // 已接入：**只加粗边框与阴影**，不改色相
                  target.configured && "ring-1 ring-emerald-500/40",
                )}
              >
                <div className="space-y-3">
                  {/* 头部：专属图标 + 标题 + 状态 Badge */}
                  <div className="flex items-start justify-between gap-2.5">
                    <div className="flex items-center gap-2.5 min-w-0">
                      {/* 图标外面套一个本客户端色的圆角底 —— 这是"颜色区分"
                          最直观的落点：即使图标本身很相似（都是深色 logo），
                          底色也不同。 */}
                      <span
                        className={cn(
                          "flex size-9 shrink-0 items-center justify-center rounded-lg",
                          accent.chip,
                        )}
                      >
                        <img
                          src={iconSrc}
                          alt=""
                          className="size-6 shrink-0 object-contain drop-shadow-2xs"
                        />
                      </span>
                      <div className="min-w-0">
                        <div className="flex items-center gap-1.5">
                          <span className="truncate text-[13.5px] font-semibold text-foreground">
                            {target.label}
                          </span>
                        </div>
                        <div className={cn("truncate text-[10.5px]", accent.text)}>
                          {meta.protocol}
                        </div>
                      </div>
                    </div>

                    <div className="flex shrink-0 items-center gap-1">
                      {target.configured ? (
                        <Badge className="border-emerald-500/30 bg-emerald-50 text-[10px] text-emerald-700 dark:bg-emerald-950/40 dark:text-emerald-300">
                          <CheckCircle2 className="mr-1 size-3" />
                          已接入
                        </Badge>
                      ) : target.installed ? (
                        <Badge variant="secondary" className="text-[10px]">
                          {target.version ? target.version : "已安装"}
                        </Badge>
                      ) : (
                        <Badge variant="outline" className="text-[10px] text-muted-foreground">
                          未安装
                        </Badge>
                      )}
                    </div>
                  </div>

                  {/* 简述与说明 */}
                  <p className="line-clamp-2 text-[11px] leading-relaxed text-muted-foreground">
                    {meta.description}
                  </p>

                  {/* 配置文件路径展示 */}
                  <div className="flex items-center justify-between gap-1.5 rounded-md border border-border/50 bg-muted/30 px-2 py-1 font-mono text-[10.5px]">
                    <span className="truncate text-muted-foreground" title={target.configPath}>
                      {target.configPath}
                    </span>
                    <button
                      type="button"
                      onClick={() => void copyToClipboard(target.configPath, "配置路径")}
                      className="shrink-0 text-muted-foreground transition hover:text-foreground"
                      title="复制文件路径"
                    >
                      <Copy className="size-3" />
                    </button>
                  </div>

                  {/* 多选模型状态栏 */}
                  <div className="rounded-lg border border-border/40 bg-muted/15 p-2 space-y-1.5">
                    <div className="flex items-center justify-between text-[11px]">
                      <span className="text-muted-foreground">
                        配置模型 ({assignedModels.length} 个)
                      </span>
                      <button
                        type="button"
                        onClick={() =>
                          setExpandedTargetModels((prev) => ({
                            ...prev,
                            [target.id]: !prev[target.id],
                          }))
                        }
                        className="flex items-center gap-0.5 text-[10px] text-primary hover:underline"
                      >
                        {isExpanded ? (
                          <>收起 <ChevronUp className="size-3" /></>
                        ) : (
                          <>自定义多选 <ChevronDown className="size-3" /></>
                        )}
                      </button>
                    </div>

                    {/* 默认摘要 */}
                    {!isExpanded && (
                      <div className="flex flex-wrap gap-1 text-[10px]">
                        {assignedModels.map((m, i) => (
                          /*
                            模型 chip 挂 `title` 给出该模型的能力摘要。
                            为什么这里也补一层：用户在**这个客户端卡片**上决定选哪些模型，
                            而能力卡在页面上方的分发区 —— 让他为了确认「这个模型收不收图」
                            来回滚动，等于把刚加的信息又藏回去了。
                            用原生 `title` 而不是 Tooltip：chip 已经嵌在 Card 里、
                            同一卡片下方还有 TooltipProvider，再包一层会让
                            Provider 嵌套且每个 chip 都多一个 portal；这里只要
                            「悬浮能看到」这个最低保证，不引入新交互。
                          */
                          <span
                            key={m}
                            data-slot="agent-assigned-model"
                            data-model={m}
                            title={capabilityHintOf(m)}
                            className={cn(
                              "rounded px-1.5 py-0.5 font-mono",
                              i === 0
                                ? "bg-primary/10 text-primary font-medium"
                                : "bg-muted text-muted-foreground",
                            )}
                          >
                            {m}{i === 0 ? " (主)" : ""}
                          </span>
                        ))}
                      </div>
                    )}

                    {/* 展开自定义模型选择 */}
                    {isExpanded && (
                      <div className="flex flex-wrap gap-1.5 pt-1">
                        {gatewayModels.map((m) => {
                          const isChecked = assignedModels.includes(m.id);
                          return (
                            <button
                              key={m.id}
                              type="button"
                              data-slot="agent-target-model-toggle"
                              data-model={m.id}
                              title={
                                // 渠道也进 title：这里 chip 太窄放不下平台标签，
                                // 但悬浮时用户仍要知道这个模型来自哪。
                                [capabilityHintOf(m.id), channelSummaryOf(m)]
                                  .filter(Boolean)
                                  .join(" · ") || undefined
                              }
                              onClick={() => toggleTargetModel(target.id, m.id)}
                              className={cn(
                                "rounded px-1.5 py-0.5 font-mono text-[10px] border transition-colors",
                                isChecked
                                  ? "border-primary bg-primary/15 text-foreground font-medium"
                                  : "border-border/60 bg-background text-muted-foreground hover:border-border",
                              )}
                            >
                              {isChecked ? "✓ " : ""}{m.id}
                            </button>
                          );
                        })}
                      </div>
                    )}

                    {/* Claude 系客户端的槽位上限提示 */}
                    {CLAUDE_PROTOCOL_TARGETS.has(target.id) &&
                      assignedModels.length > CLAUDE_SLOT_LIMIT && (
                        <div className="text-[10px] leading-relaxed text-amber-600 dark:text-amber-400">
                          Claude 客户端仅支持 Sonnet / Opus / Haiku / Fable 四个模型槽位，
                          一键接入时仅前 {CLAUDE_SLOT_LIMIT} 个模型生效
                        </div>
                      )}
                  </div>
                </div>

                {/* 卡片底部操作栏 */}
                <div className="mt-3.5 flex items-center justify-between gap-2 border-t border-border/40 pt-2.5">
                  <Button
                    variant="ghost"
                    size="sm"
                    className="h-7 px-2 text-[11px] text-muted-foreground"
                    onClick={() => void handleOpenBackups(target)}
                  >
                    <History className="mr-1 size-3" />
                    备份历史
                  </Button>

                  <TooltipProvider>
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <span>
                          <Button
                            size="sm"
                            className={cn(
                              "h-7 px-3 text-xs font-medium",
                              target.configured && "border border-emerald-500/30 bg-emerald-50 text-emerald-800 hover:bg-emerald-100 dark:bg-emerald-950/40 dark:text-emerald-200 dark:hover:bg-emerald-900/50",
                            )}
                            variant={target.configured ? "outline" : "default"}
                            onClick={() => void handleImport(target)}
                            disabled={isOperating || !hasApiKey}
                          >
                            {isOperating ? (
                              <Loader2 className="mr-1 size-3 animate-spin" />
                            ) : target.configured ? (
                              <RotateCcw className="mr-1 size-3 text-emerald-600 dark:text-emerald-400" />
                            ) : (
                              <Wand2 className="mr-1 size-3" />
                            )}
                            {target.configured ? "更新配置" : "一键接入"}
                          </Button>
                        </span>
                      </TooltipTrigger>
                      {!hasApiKey ? (
                        <TooltipContent className="text-xs">
                          请先在「兼容网关」设置 API Key
                        </TooltipContent>
                      ) : null}
                    </Tooltip>
                  </TooltipProvider>
                </div>
              </Card>
            );
          })}
        </div>
      </div>

      {/* 备份历史与恢复对话框 */}
      <Dialog open={backupDialogOpen} onOpenChange={setBackupDialogOpen}>
        <DialogContent className="max-w-md">
          <DialogHeader>
            <DialogTitle className="text-base">
              {activeBackupTarget?.label} 配置备份记录
            </DialogTitle>
            <DialogDescription className="text-xs text-muted-foreground">
              每次执行接入或更新配置前系统均会自动创建时间戳备份，可随时无损回滚。
            </DialogDescription>
          </DialogHeader>

          <div className="max-h-60 space-y-2 overflow-y-auto py-2">
            {loadingBackups ? (
              <div className="flex items-center justify-center py-6 text-xs text-muted-foreground">
                <Loader2 className="mr-2 size-4 animate-spin" />
                正在读取备份记录...
              </div>
            ) : backups.length === 0 ? (
              <div className="py-6 text-center text-xs text-muted-foreground">
                暂无备份记录（在首次写入配置后自动生成）
              </div>
            ) : (
              backups.map((b) => (
                <div
                  key={b.id}
                  className="flex items-center justify-between rounded-lg border border-border/60 bg-muted/30 px-3 py-2 text-xs"
                >
                  <div>
                    <div className="font-medium">
                      {b.createdAt ? new Date(b.createdAt).toLocaleString() : b.id}
                    </div>
                    <div className="font-mono text-[10.5px] text-muted-foreground">ID: {b.id}</div>
                  </div>
                  <Button
                    variant="outline"
                    size="sm"
                    className="h-7 text-xs"
                    disabled={restoringBackupId !== null}
                    onClick={() => void handleRestore(b.id)}
                  >
                    {restoringBackupId === b.id ? (
                      <Loader2 className="mr-1 size-3 animate-spin" />
                    ) : (
                      <RotateCcw className="mr-1 size-3" />
                    )}
                    恢复此版本
                  </Button>
                </div>
              ))
            )}
          </div>

          <DialogFooter className="flex items-center justify-between gap-2 sm:justify-between">
            <Button
              variant="outline"
              size="sm"
              onClick={() => setBackupDialogOpen(false)}
            >
              关闭
            </Button>
            {backups.length > 0 && (
              <Button
                variant="secondary"
                size="sm"
                disabled={restoringBackupId !== null}
                onClick={() => void handleRestore()}
              >
                {restoringBackupId === "latest" ? (
                  <Loader2 className="mr-1 size-3 animate-spin" />
                ) : (
                  <RotateCcw className="mr-1 size-3" />
                )}
                恢复最近备份
              </Button>
            )}
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
