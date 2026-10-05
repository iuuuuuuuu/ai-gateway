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
  Maximize2,
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
import traeWorkIcon from "@/assets/trae-work-official-icon.png";

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
import { AgentSwitcher } from "@/components/agents/agent-switcher";
import { ClientDetail } from "@/components/agents/client-detail";
import { ClientDetailQuota } from "@/components/agents/client-detail-quota";
import { FullScreenPanel } from "@/components/common/full-screen-panel";
import { ModelCapabilityRow } from "@/components/model-capability-chip";
// 「分发模型」卡片的**可复制写法**行与兼容网关的模型路由清单共用同一组实现。
//
// ⚠ 必须复用而不是各写一份：那个字符串是用户要贴进客户端的，
// 两处实现分叉会出现"这边能复制、那边不能"，或复制出来的字符串不同 ——
// 而写法错一个字符，网关就解析失败。
import { CopyableForm, routeFormsOf } from "@/components/model-routing-list";
import * as api from "@/lib/api";
import { capabilityViewOf, summarizeCapabilities } from "@/lib/model-capability";
import { accentOf } from "@/lib/client-accent";
import { productAccentOf } from "@/lib/product-accent";
import type {
  AgentBackupItem,
  AgentClientTarget,
  AgentDetectionResult,
  GatewayModelItem,
  GatewayStatus,
} from "@/lib/types";
import { cn } from "@/lib/utils";

// Trae 图标 —— **官方应用图标**（2026-09-22 改为官方资源）。
//
// # 为什么改了（此前是内联 data-URI 的「横杠 + 竖杆」渐变方块）
//
// 所有者原话：
//
//	「zcode后面不还是一个闪电小卡片么?还不是官方的logo  qoder也一样」
//
// 此前的注释写着「官方图标是受版权保护的美术资源，本仓库不内置」——
// **那个理由是错的/过期的**：上面第 35 行 import 的 `agent-icons/zcode.png`
// 就是官方图标，本页早就在用它；`components/product-marks.tsx` 的
// WorkBuddy / Qoder / ZCode 也全是官方资源，只剩 Trae 是自绘。
//
// # 来源
//
// 从**本机已安装的官方客户端 exe** 的 RT_GROUP_ICON / RT_ICON 资源里提取
//（取 256×256 那一档）：`D:\APP\TRAE SOLO CN\TRAE SOLO CN.exe`（白底 + 黑字形）。
//
// ⚠ 图标映射的值是图片地址（`string`），放不下 React 组件；
// 这里 import 官方 png 后由 Vite 转成 URL 字符串，写法与上面的
// `zcodeIcon` 完全一致（这正是当初"只能内联 data-URI"那个前提失效的地方）。

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
  trae: traeWorkIcon,
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
  trae: {
    protocol: "OpenAI Chat",
    protocolBadge: "POST /v1/chat/completions",
    description:
      "Trae Work / Trae 客户端。模型列表存在 SQLite 的 state.vscdb 里，导入时把本网关作为自定义模型写进全部模型组（SOLO 的 9 组与 IDE 的 7 组）；写入前自动备份原库，官方内置模型原样保留。",
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

  /**
   * 「模型 → 允许的平台」白名单（所有者的需求：
   * 「平台区分使用哪个平台的模型」）。
   *
   *   undefined  → 该模型**不限制**（默认，行为与加该功能之前一致）
   *   ["zcode"]  → 只允许 ZCode
   *   []         → 用户在界面上把全部平台都关掉了 = 想恢复默认
   *                （故存盘时会被过滤掉，见 api.setModelPlatforms）
   */
  const [modelPlatforms, setModelPlatforms] = useState<Record<string, string[]>>({});
  const [platformSaving, setPlatformSaving] = useState<string | null>(null);

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

  /**
   * 客户端分段切换控件的选中项。
   *
   * 它是**纯视图状态**，不参与任何数据流：`AgentSwitcher` 只负责在头部把
   * 12 个客户端收成一个不溢出的分段条（溢出项进「更多」），点一下就把
   * 卡片网格过滤到那一个客户端 —— 这是本页唯一缺的「快速定位某个客户端」
   * 入口（此前只能靠下面的 全部/已安装/已接入/未安装 四个粗粒度页签）。
   *
   * ⚠ 与 `filterTab` **正交**：切换器选的是"哪一个客户端"，页签选的是
   * "哪一类客户端"。两者同时生效（见 `filteredTargets`），不互相覆盖 ——
   * 否则用户点一下切换器就会发现自己的页签选择被悄悄重置了。
   *
   * `null` = 不聚焦任何客户端（显示页签筛出的全部）。
   */
  const [focusedTargetId, setFocusedTargetId] = useState<string | null>(null);

  /**
   * 三级下钻：当前展开详情的客户端 id。
   *
   * 用 id 而不是 `AgentClientTarget` 对象：`fetchData()` 每次刷新都会
   * 产生一批**新对象**，持有旧对象会让「刷新后详情面板里还是上一份
   * installed/configured 状态」成为可能。存 id 则每轮渲染都从最新的
   * `targets` 里现取，永远一致。
   */
  const [detailTargetId, setDetailTargetId] = useState<string | null>(null);

  // 备份管理对话框
  const [backupDialogOpen, setBackupDialogOpen] = useState(false);
  const [activeBackupTarget, setActiveBackupTarget] = useState<AgentClientTarget | null>(null);
  const [backups, setBackups] = useState<AgentBackupItem[]>([]);
  const [loadingBackups, setLoadingBackups] = useState(false);
  const [restoringBackupId, setRestoringBackupId] = useState<string | null>(null);

  /**
   * 详情面板的备份记录，与卡片上的「备份历史」弹窗**分开持有**。
   *
   * 不复用上面那三个 state：两个入口可以同时开着（弹窗盖在面板上），
   * 共用一个 `backups` 会让后打开的那个把前一个的列表冲掉 ——
   * 表现为"面板里点刷新，背后的弹窗列表跟着变了"。
   */
  const [detailBackups, setDetailBackups] = useState<AgentBackupItem[]>([]);
  const [detailBackupsLoading, setDetailBackupsLoading] = useState(false);

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
  /**
   * 该模型是否允许走某个平台。
   *
   * 没配置过（`undefined`）= 允许 —— 这是"加功能不改变老行为"的关键。
   */
  const isPlatformOn = useCallback(
    (modelId: string, product: string): boolean => {
      const allowed = modelPlatforms[modelId];
      if (!allowed) return true; // 未配置 = 不限制
      return allowed.includes(product);
    },
    [modelPlatforms],
  );

  /**
   * 切换某模型的平台开关。
   *
   * ## 语义：点亮 = 允许，全熄灭 = 恢复不限制
   *
   * 「全熄灭」**不是**"这个模型哪个平台都不能用" —— 那样用户取消全部
   * 勾选会把模型彻底禁掉，是个陷阱。故全熄灭时把该键**删掉**，
   * 回到"未配置 = 不限制"。后端也会过滤空数组（双保险）。
   *
   * ## 为什么要点一下存一次
   *
   * 不做"改完再点保存"：这个页面已经有多处即时生效的开关
   *（模型勾选、客户端接入），再加一个"保存"按钮会让人不确定
   * 到底哪些改动已生效。代价是每次点击会重启网关（白名单在网关启动时
   * 读取）—— 故按钮在保存期间置灰，避免连点导致反复重启。
   */
  const togglePlatform = useCallback(
    async (modelId: string, product: string) => {
      const current = modelPlatforms[modelId];
      // 当前该模型可见的平台全集（从网关下发的 channels 取）
      const item = gatewayModels.find((m) => m.id === modelId);
      const all = (item?.channels ?? []).map((c) => c.product);
      if (all.length === 0) return;

      // 未配置 → 以"全部允许"为起点
      const on = current ? [...current] : [...all];
      const next = on.includes(product) ? on.filter((p) => p !== product) : [...on, product];

      // 构造新配置；全熄灭 / 与全集相同 → 删键（恢复不限制）
      const nextAll: Record<string, string[]> = { ...modelPlatforms };
      if (next.length === 0 || next.length === all.length) {
        delete nextAll[modelId];
      } else {
        nextAll[modelId] = next;
      }

      const prev = modelPlatforms;
      setModelPlatforms(nextAll); // 乐观更新，点击立刻有反馈
      setPlatformSaving(modelId);
      try {
        await api.setModelPlatforms(nextAll);
        toast.success(
          nextAll[modelId]
            ? `${modelId} 只走：${next.join(" / ")}`
            : `${modelId} 已恢复为「不限制平台」`,
          // 网关要重启才生效，说清楚以免用户以为没生效
          { description: "网关正在按新配置重启，几秒后生效" },
        );
      } catch (err) {
        setModelPlatforms(prev); // 失败回滚，别让界面显示一个没保存成功的状态
        toast.error("保存平台限制失败", {
          description: err instanceof Error ? err.message : String(err),
        });
      } finally {
        setPlatformSaving(null);
      }
    },
    [modelPlatforms, gatewayModels],
  );

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

  /**
   * 把某个客户端重置回「跟随全局模型池」。
   *
   * 实现是**删除**该 key 而不是写入 `selectedGlobalModels`：`getModelsForTarget`
   * 的判据是 `customTargetModels[targetId] ?? selectedGlobalModels`，
   * 只有 key 不存在才代表"没定制过"。若写成写入一份全局池的**副本**，
   * 之后全局池再变（用户在上面勾了别的模型）这个客户端就跟不上了 ——
   * 而界面上它会显示成"跟随全局"，语义与行为分叉。
   */
  const resetTargetModels = (targetId: string) => {
    setCustomTargetModels((prev) => {
      if (!(targetId in prev)) return prev;
      const next = { ...prev };
      delete next[targetId];
      return next;
    });
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

  /**
   * 读详情面板那份备份记录。
   *
   * 与 `handleOpenBackups` 的唯一区别是**不弹窗**：详情面板自己就是一块
   * 常驻区域，进去时自动拉一次，之后由面板上的「刷新」按钮触发。
   * 传 id 而不是 target 对象，与 `detailTargetId` 的存法保持一致。
   */
  const loadDetailBackups = useCallback(async (targetId: string) => {
    setDetailBackupsLoading(true);
    try {
      const res = await api.listAgentBackups(targetId);
      setDetailBackups(res.backups || []);
    } catch (err) {
      // 备份读不到不该让详情面板报错：它只是面板里的一块次要信息，
      // 主信息（协议、模型槽位）仍然可用。给空列表 + 控制台留痕。
      console.warn("读取详情备份记录失败:", err);
      setDetailBackups([]);
    } finally {
      setDetailBackupsLoading(false);
    }
  }, []);

  /**
   * 打开详情面板时拉一次备份记录。
   *
   * 依赖 `detailTargetId` 而不是 `detailTarget`：后者每轮 `fetchData()`
   * 都会产生**新对象**，用它作依赖会让"刷新状态"顺带重拉一次备份 ——
   * 而备份列表并不随客户端状态刷新而变，那次请求纯属浪费。
   */
  useEffect(() => {
    if (!detailTargetId) {
      setDetailBackups([]);
      return;
    }
    void loadDetailBackups(detailTargetId);
  }, [detailTargetId, loadDetailBackups]);

  // 恢复备份
  //
  // 抽成"显式传 target"的形态，因为有两个入口：卡片上的备份弹窗
  // （用 `activeBackupTarget`）与详情面板（用当前下钻的客户端）。
  // ⚠ 详情面板那条路**不能**靠 `setActiveBackupTarget(detailTarget)` 再调
  // 旧签名 —— setState 是异步的，而 `handleRestore` 闭包读的是**本次渲染**
  // 的 `activeBackupTarget`，会拿到 null 而静默 return（表现为"点了没反应"）。
  const restoreBackupFor = async (target: AgentClientTarget, backupId?: string) => {
    setRestoringBackupId(backupId ?? "latest");
    try {
      const res = await api.restoreAgentClient(target.id, backupId);
      toast.success(`已成功恢复 ${target.label} 的配置`, {
        description: `已回滚 ${res.restored} 个配置文件至备份状态`,
      });
      setBackupDialogOpen(false);
      await fetchData(true);
      await loadDetailBackups(target.id);
    } catch (err) {
      toast.error("恢复配置失败: " + String(err));
    } finally {
      setRestoringBackupId(null);
    }
  };

  const handleRestore = async (backupId?: string) => {
    if (!activeBackupTarget) return;
    await restoreBackupFor(activeBackupTarget, backupId);
  };

  const hasApiKey = Boolean(gatewayStatus?.config?.api_key || detection?.hasApiKey);
  const targets = detection?.targets ?? [];

  const installedCount = useMemo(() => targets.filter((t) => t.installed).length, [targets]);
  const configuredCount = useMemo(() => targets.filter((t) => t.configured).length, [targets]);

  const filteredTargets = useMemo(() => {
    const byTab = (() => {
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
    })();
    // 切换器聚焦：与上面的页签**同时**生效（两者语义正交，见 focusedTargetId 注释）。
    // 聚焦的客户端若被页签筛掉，结果是空网格 —— 这是**正确**的：
    // 「未安装」页签下聚焦一个已安装客户端，本来就不该显示它。
    if (!focusedTargetId) return byTab;
    return byTab.filter((t) => t.id === focusedTargetId);
  }, [targets, filterTab, focusedTargetId]);

  /**
   * 分段切换器的数据源。
   *
   * 取 `targets` 而**不是** `filteredTargets`：切换器自己就是筛选器，
   * 用筛选结果喂它会让"点一下 → 候选只剩一个 → 再也切不回去"。
   *
   * `icon` 用页面已有的 `CLIENT_ICONS`（与卡片上那个是同一份映射），
   * `badge` 只在同品牌两个客户端之间做区分（Claude Code 终端 /
   * Claude Desktop 显示器）—— 这两个图标完全相同，不给角标就分不出谁是谁。
   * `hint` 给出该客户端的接入状态，鼠标悬浮即可确认，不必先点开。
   */
  const switcherItems = useMemo(
    () =>
      targets.map((t) => ({
        id: t.id,
        label: t.label,
        icon: CLIENT_ICONS[t.id] || claudeIcon,
        badge:
          t.id === "claude-code"
            ? ("terminal" as const)
            : t.id === "claude-desktop"
              ? ("monitor" as const)
              : undefined,
        hint: t.configured ? "已接入网关" : t.installed ? "已安装，未接入" : "未安装",
      })),
    [targets],
  );

  /** 三级下钻的目标：每轮渲染都从最新的 `targets` 现取（见 detailTargetId 注释）。 */
  const detailTarget = useMemo(
    () => targets.find((t) => t.id === detailTargetId) ?? null,
    [targets, detailTargetId],
  );

  /** 详情面板里这个客户端当前的模型槽位。 */
  const detailModels = detailTarget ? getModelsForTarget(detailTarget.id) : [];

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
    <div className="space-y-5 p-4 sm:p-6">
      {/*
        ⚠ 这里原本是 `mx-auto max-w-6xl`，已按所有者 2026-09-20 的要求去掉：
        「智能体管理两边留白太多了,优化一下」。

        `max-w-6xl` = 1152px 封顶 + `mx-auto` 居中 ⇒ 宽屏下两侧各空一大块。
        而**同级的「兼容网关」页没有任何 max-w 约束**（占满可用宽度），
        两页切换时一个满宽、一个居中窄栏，观感差异很明显。

        实测：这个上限让下方网格容器只有 1104px，直接导致「流式布局」
        最多只能排 3 列（见网格处的算式）。故去掉它同时也是
        让流式布局真正生效的前提。

        保留 `p-4 sm:p-6`（内容与窗口边缘的呼吸），只去掉**宽度封顶** ——
        两者是不同的事，一起删会让内容贴边。
      */}
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

        {/*
          弹性中段：空间不足时由 AgentSwitcher 自行把溢出的客户端收进「更多」。
          ⚠ 外层必须是 `min-w-0 flex-1 overflow-hidden` —— AgentSwitcher 内部用
          `ResizeObserver` 观察**父元素**宽度来决定收几个（见其 :79-110 注释：
          用自身宽度会形成「收起→变窄→再收起」的反馈循环）。父容器若没有
          确定宽度，它就永远量不到可用空间，只能一路收到底。
        */}
        <div className="flex min-w-0 flex-1 items-center justify-end overflow-hidden">
          {switcherItems.length > 0 ? (
            <AgentSwitcher
              items={switcherItems}
              activeId={focusedTargetId ?? ""}
              onSelect={(id) => setFocusedTargetId((prev) => (prev === id ? null : id))}
            />
          ) : null}
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

            ⚠ 加 `h-full` 会不会撑破这个滚动容器（`max-h-96` + `overflow-y-auto`）？
              实测**不会**（2026-10-01，40 个模型 / 14 行，含超长名与三平台折行）：
                · 容器 clientHeight 仍为 **384px**（= max-h-96），没被撑开；
                · scrollHeight **3109px** > clientHeight ⇒ 滚动条正常出现；
                · 滚到底 scrollTop 到 2725（= 最大滚动量），最后一张卡完整可见。
              原因：`h-full` 的高度参照是 **grid 区**（由行高决定），不是容器的
              滚动高度；`max-h` 限制的是容器自身，两者不冲突。
            */
            <div className="grid grid-cols-1 gap-2 pt-1 max-h-96 overflow-y-auto p-0.5 sm:grid-cols-2 lg:grid-cols-3">
              {gatewayModels.map((m, index) => {
                const selected = selectedGlobalModels.includes(m.id);
                const isPrimary = selectedGlobalModels[0] === m.id;
                // 每张卡都需要能力视图；map 里查表而不是现算，保证与
                // capabilitySummary 的口径完全一致（同一份 view）。
                const view = capabilityViews.get(m.id) ?? capabilityViewOf(m);

                return (
                  /* ⚠ 这里必须是 `<div role="button">` 而**不是** `<button>`。
                     原因：卡片内部还有「平台开关」（`model-platform-toggle`），
                     而 **HTML 不允许 button 嵌套 button** —— 浏览器会把内层
                     button 提到外层之后，于是内层的点击**永远不触发自己的
                     onClick**（实测：点平台开关毫无反应，也不报错，
                     因为事件被外层的"选中模型"处理器吞掉了）。

                     用 div + role="button" + 键盘处理保持可访问性。 */
                  <div
                    key={m.id}
                    role="button"
                    tabIndex={0}
                    data-slot="agent-model-card"
                    data-model={m.id}
                    data-selected={selected ? "true" : "false"}
                    data-primary={isPrimary ? "true" : "false"}
                    onClick={() => toggleGlobalModel(m.id)}
                    onKeyDown={(e) => {
                      // 键盘可访问性：Enter/Space 等价于点击。
                      // 但内层开关自己会处理键盘事件，故先判断来源。
                      if (e.target !== e.currentTarget) return;
                      if (e.key === "Enter" || e.key === " ") {
                        e.preventDefault();
                        toggleGlobalModel(m.id);
                      }
                    }}
                    className={cn(
                      // ⚠ 关于 `h-full`：**本网格其实不靠它等高**（2026-10-01 实测）。
                      //
                      // 我原先写的理由是「没有 h-full ⇒ 卡片按内容取高 ⇒ 同排高矮
                      // 不一」。用真实 bundle 在 Chrome 里量过之后，**这个推断被证伪**：
                      // 这里的卡片 div **本身就是 grid 项**（外面没有包一层），
                      // 于是 grid 默认的 `align-items: stretch` 直接作用在它身上 ——
                      // 它本来就被拉伸到行高，不存在「只拉伸了外层」这回事。
                      //
                      // 实测（40 个模型 / 14 行，含超长模型名与三平台折行）：
                      // 带 `h-full` 与摘掉 `h-full`，每行的行内高度差**都是 0.00px**。
                      //
                      // 留着它是**防御性**的，理由也已实测：给这个网格人为加上
                      // `items-start`（即禁用 stretch）后 ——
                      //   · 卡片**无** `h-full` ⇒ 行内高度差立刻爆到 **416px**
                      //     （各模型写法条数不同，最高那行被撑开）；
                      //   · 卡片**有** `h-full` ⇒ 行内差回到 **0.00px**。
                      // 即 `h-full` 确实能在「stretch 被关掉」时兜住，是**冗余但有用**。
                      // 万一后人给这个网格补上 `items-start`
                      //（下方「智能体客户端」那个网格就踩过这个坑，见它的四格矩阵），
                      // 有 `h-full` 就不会重新裂开。它是**保险**，不是等高主因。
                      "flex h-full cursor-pointer flex-col gap-1.5 rounded-lg border px-2.5 py-2 text-left text-xs transition-all",
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

                    {/* 渠道：这个模型来自哪个平台，**并且可以点击限定**。
                        所有者的需求：
                          「可以加一个渠道，是来自于哪个平台，如果重叠，
                            就显示多个平台」
                          「我希望可以加上 **平台区分使用哪个平台的模型**」

                        第二句是**控制**需求，不只是显示：同名模型可能同时在
                        多个平台上（实测 glm-5.2 在 WorkBuddy 与 ZCode 都有），
                        而各平台额度性质不同（ZCode 体验套餐 9/23 到期作废，
                        该优先烧掉）。故 chip 做成**开关**：
                          · 全部点亮 = 不限制（默认，行为不变）
                          · 只点亮一个 = 该模型只走这个平台
                          · 全部熄灭 = 恢复"不限制"（不是"全都不能用"，
                            否则用户取消全部勾选会把模型彻底禁掉 —— 那是陷阱）

                        只在下发时展示（旧网关没有这个字段 = 未声明）。 */}
                    {m.channels && m.channels.length > 0 && (
                      <div
                        className="flex flex-wrap items-center gap-1"
                        data-slot="agent-model-channels"
                      >
                        {m.channels.length > 1 && (
                          <span className="text-[10px] leading-4 text-muted-foreground">
                            平台：
                          </span>
                        )}
                        {m.channels.map((ch) => {
                          const on = isPlatformOn(m.id, ch.product);
                          const accent = productAccentOf(ch.product);
                          const onlyThis =
                            on &&
                            m.channels!.filter((x) => isPlatformOn(m.id, x.product)).length === 1 &&
                            m.channels!.length > 1;
                          return (
                            <button
                              key={ch.product}
                              type="button"
                              data-slot="model-platform-toggle"
                              data-product={ch.product}
                              data-on={on ? "1" : "0"}
                              aria-pressed={on}
                              disabled={platformSaving === m.id}
                              aria-label={`${m.id} 使用 ${ch.label} 平台（当前${on ? "允许" : "不允许"}）`}
                              title={
                                (ch.regions && ch.regions.length > 0
                                  ? `${ch.label}（${ch.regions.join(" / ")}）`
                                  : ch.label) +
                                (onlyThis ? " · 当前只允许这个平台" : "")
                              }
                              onClick={(e) => {
                                // ⚠ 必须阻止冒泡：外层模型卡片也有 onClick
                                //（选中/取消该模型）。不阻止的话，点平台开关
                                // 会连带把模型选中状态切掉。
                                e.stopPropagation();
                                void togglePlatform(m.id, ch.product);
                              }}
                              className={cn(
                                // 字号 9px → 11px、加大内边距：
                                // 所有者反馈「都看不清」，9px 确实太小。
                                "rounded border px-1.5 py-0.5 text-[11px] font-medium leading-4 transition-colors",
                                // 每个平台一个色相（所有者的要求：
                                // 「不同平台应该用不同的颜色表示」）。
                                // 此前三个平台共用 primary 一套色 → 分不出来。
                                on
                                  ? cn(accent.border, accent.bg, accent.text)
                                  : cn(accent.off, "line-through"),
                              )}
                            >
                              {ch.label}
                            </button>
                          );
                        })}
                      </div>
                    )}

                    {/* 行 4：**可复制写法** —— 与「兼容网关」的模型路由清单同一个组件。

                        所有者 2026-09-20：「智能网关的 分发模型样式 和 那个模型逻辑
                        要跟兼容网关显示的一样」。

                        这一页的用户**最需要**它：他在这里勾完模型，接着就去客户端里
                        填模型名。有这组可复制写法就能直接抄，不必回兼容网关页找。

                        ⚠ 复用 `CopyableForm` / `routeFormsOf`，**不自己拼字符串** ——
                        写法必须与网关 `resolveModel` 的解析口径逐字一致，
                        错一个字符用户抄进去就解析失败。 */}
                    {(() => {
                      const forms = routeFormsOf(m);
                      if (forms.length === 0) return null;
                      return (
                        /*
                         * ★ `mt-auto` 让这一块**贴到卡片底部**（2026-10-01）。
                         *
                         * 这是「(b) 等高 + 底部元素对齐到底」而不是「(a) 纯等高」。
                         *
                         * 为什么选 (b)：本卡片的信息层次是
                         * 模型名 → 能力 → 平台开关 → **可复制写法**（用户看完就去
                         * 客户端里粘贴）。最后一块是**行动出口**，让它对齐成一排
                         * 比让它随内容浮动更好找；而「平台开关」那一行的高度天然
                         * 不同（1/2/3 个平台、折行与否），无法也不需要对齐。
                         *
                         * ⚠ 实测（反事实检验，2026-10-01）：摘掉 `mt-auto` 后，
                         * 同排「可复制写法」的**底边差从 0px 变成 50~52px**
                         *（`hy3` 的底边距卡底 9px → 31px，`kimi-k2.7` → 59px），
                         * 卡片本身仍然等高（高度差 0px）—— 正好说明：
                         * 等高由 grid 保证，**底部对齐由这一行 `mt-auto` 保证**，
                         * 两者解决的是不同的问题，缺了它就会「卡片齐、内容不齐」。
                         *
                         * ⚠ 关于「平台开关行错位」：那一行的**上边偏移**在同排内
                         * 实测差 22px（有「默认主模型」徽章的卡片把它往下推了
                         * 22px），这是**信息层次的真实差异**，不是缺陷 ——
                         * 强行拉平只能靠给无徽章的卡片塞等高空占位，
                         * 代价是每张卡都多一块空白，反而更难读。
                         * 真正需要对齐的是「可复制写法」这一行，已由 `mt-auto` 保证。
                         */
                        <div className="mt-auto flex flex-wrap gap-1 pt-0.5" data-slot="agent-model-route-forms">
                          {forms.map((f) => (
                            <CopyableForm key={f} form={f} />
                          ))}
                        </div>
                      );
                    })()}
                  </div>
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

        {/*
          客户端卡片网格 —— **自适应流式**（所有者 2026-09-20：
          「智能体管理布局也要改成流式的」）。

          原来是固定断点 `sm:grid-cols-2 lg:grid-cols-3`：宽屏下永远只有
          3 列，右侧留一大片空白。

          改成 `auto-fill` + `minmax(260px,1fr)`：列数随容器宽度**自动**决定。

          ⚠ 下界取 **260px** 而不是 320 —— 这是**实测**定的，不是估的：

              容器的实际宽度只有 **1104px**（视口 1800，但侧边栏 + 内边距
              吃掉约 700px），gap 14px：

                  minmax(320px) → 3 列（每列 359px）   ← 我第一版，等于没改
                  minmax(300px) → 3 列
                  minmax(280px) → 3 列
                  minmax(260px) → **4 列**（每列 266px）← 正确

              算法：n 列需要 `n*min + (n-1)*gap ≤ 1104`，故 4 列要求 min ≤ 261。
              我第一版凭"卡片内容需要 320px"的感觉定，结果**一列都没多** ——
              是验证脚本量出 `gridWidth: 1104` 才发现的。

          取 260 还与「兼容网关」的模型路由清单**同一个下界**（那边也是 260），
          两处同类布局保持一致。

          ⚠ 不加 `auto-rows-fr`：那会让所有行等于容器高度的一份，
          短卡片被撑出大片空白（我在模型清单上刚犯过这个错）。
        */}
        {/*
          ★ 卡片等高（2026-10-01 所有者反馈「卡片的高度不一致」）。

          这里此前是 `items-start` —— 它**明确禁止** grid 的默认拉伸行为
          （`align-items: stretch`），于是每张卡片的高度就是它自己的内容高度。
          而各客户端卡片的内容行数天然不同：
            · 描述文字 1 行 vs 2 行
            · 「已接入」时的账号/模型明细块有无
            · 展开态（`expandedTargetModels`）多出一整块
          ⇒ 同一排的卡片高矮不齐，视觉上像没对齐。

          改法：去掉 `items-start`，恢复 grid 默认的 `stretch`。

          ★ 实测的四格矩阵（2026-10-01，真实 bundle + 12 张卡，量行内高度差）：

              grid 的 items-start | 卡片的 h-full | 行内高度差
              ────────────────────┼───────────────┼────────────
                     有           |      无       |  122px  ← 缺陷原状
                     有           |      有       |    0px
                     无           |      无       |    0px
                     无           |      有       |    0px  ← 现状

            读法：**只要二者有其一，就等高**。
              · 去掉 `items-start` ⇒ grid 项被 stretch 拉齐，行高取本行最高者；
              · 卡片自带 `h-full` ⇒ 即使网格是 `flex-start`，`height:100%`
                也会把卡片撑到 grid 区的高度。
            两者是**互相替代**的关系，不是「缺一不可」—— 我最初写的
            「必须同时做」是**推断，且被实测证伪**。同时保留只是双保险。

          ⚠ 之所以两处都留着：本网格与上方「分发模型配置」那个网格（`max-h-96`
          滚动区）日后任一处被改，另一处的写法仍能兜住；且这两条各自都不产生
          副作用（`stretch` 按行对齐、`h-full` 在已被拉伸时等于无操作）。

          ⚠ 为什么**不加** `auto-rows-fr`（原作者在下面注释里也提过）：
          那会让**所有行**都等于最高那一行的高度，短卡片被撑出大片空白。
          这里要的只是「同一行内等高」，而 `stretch` 正是这个语义 ——
          它按**行**对齐，不跨行统一。

          卡片本身已有 `flex flex-col justify-between`，等高之后
          底部那块会被 `justify-between` 推到卡底，同排卡片的底边自然对齐。
        */}
        <div className="grid grid-cols-[repeat(auto-fill,minmax(260px,1fr))] gap-3.5">
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
                  // `h-full` 与上面 grid 去掉 `items-start` **任一即可**等高
                  //（实测：有其一 → 行内差 0px；两者都无 → 122px），
                  // 这里两条都留着是双保险，理由见网格处的四格矩阵。
                  "flex h-full flex-col justify-between rounded-xl border p-4 transition-all",
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

                  {/*
                    套餐额度（Claude Code / Codex / Grok Build / Kimi Code）。

                    只有 `CLIENT_TO_PROVIDER` 里确实有映射的客户端才会渲染 ——
                    其余客户端这个组件 `return null`，卡片外观与从前**完全一致**，
                    不会多出一块空白。映射表刻意不含 `claude-desktop`：
                    它和 claude-code 共用一套额度，宁可少显示也不显示错的账号。

                    ⚠ 组件自身 `autoLoad` 缺省 false ⇒ **不会**自动发查询请求，
                    要用户点里面的「查询」按钮才拉数据。这是有意为之：
                    卡片网格一次渲染 12 张，自动查询会瞬间打 12 个上游请求。
                  */}
                  <ClientDetailQuota targetId={target.id} />

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
                  <div className="flex items-center gap-0.5">
                    <Button
                      variant="ghost"
                      size="sm"
                      className="h-7 px-2 text-[11px] text-muted-foreground"
                      onClick={() => void handleOpenBackups(target)}
                    >
                      <History className="mr-1 size-3" />
                      备份历史
                    </Button>

                    {/*
                      三级下钻入口。卡片上只放得下模型摘要与两个操作，
                      协议细节 / 完整模型池 / 备份记录放不进来 ——
                      点这里进全屏详情面板（FullScreenPanel + ClientDetail）。
                    */}
                    <Button
                      variant="ghost"
                      size="sm"
                      className="h-7 px-2 text-[11px] text-muted-foreground"
                      onClick={() => setDetailTargetId(target.id)}
                      title="查看客户端详情"
                    >
                      <Maximize2 className="mr-1 size-3" />
                      详情
                    </Button>
                  </div>

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

      {/*
        三级下钻：单个客户端的完整详情（全屏面板）。

        为什么用 `FullScreenPanel` 而不是再开一个 Dialog：卡片网格本身已经很密
        （12 张卡 × 模型多选），详情里要放协议细节 + 完整模型池勾选 + 备份记录，
        用居中弹窗会立刻撑爆高度并出现内外两层滚动条。全屏面板自带
        `createPortal` + `z-[60]` + ESC 关闭（且会跳过输入框里的 ESC），
        正好是"从卡片推进去、按返回退回来"的形态。

        ⚠ `isOpen` 与 `children` 都受 `detailTarget` 控制：面板关闭后
        `usePresence` 还要播 220ms 退场动画才卸载，所以退场期间
        `detailTargetId` 已被置 null ⇒ `detailTarget` 为 null ⇒ 这里渲染
        `null` 而不是崩在 `target.id` 上。标题同理走可选链。
      */}
      <FullScreenPanel
        isOpen={Boolean(detailTarget)}
        motionPreset="slide-from-right"
        title={detailTarget ? detailTarget.label : ""}
        description={
          detailTarget
            ? `${(CLIENT_METAS[detailTarget.id] ?? { protocol: "通用兼容", protocolBadge: "API" }).protocol} · ${
                (CLIENT_METAS[detailTarget.id] ?? { protocol: "通用兼容", protocolBadge: "API" }).protocolBadge
              }`
            : undefined
        }
        onClose={() => setDetailTargetId(null)}
        footer={
          <div className="flex items-center justify-between gap-2">
            <Button variant="outline" size="sm" onClick={() => setDetailTargetId(null)}>
              返回
            </Button>
            {detailTarget ? (
              <Button
                size="sm"
                className="gap-1.5 text-xs font-medium"
                disabled={operatingTarget === detailTarget.id || batchUpdating || !hasApiKey}
                onClick={() => void handleImport(detailTarget)}
              >
                {operatingTarget === detailTarget.id ? (
                  <Loader2 className="size-3.5 animate-spin" />
                ) : detailTarget.configured ? (
                  <RotateCcw className="size-3.5" />
                ) : (
                  <Wand2 className="size-3.5" />
                )}
                {detailTarget.configured ? "更新配置" : "一键接入"}
              </Button>
            ) : null}
          </div>
        }
      >
        {detailTarget ? (
          <ClientDetail
            target={detailTarget}
            models={gatewayModels}
            assignedModels={detailModels}
            isCustomized={Boolean(customTargetModels[detailTarget.id])}
            expanded={Boolean(expandedTargetModels[detailTarget.id])}
            onToggleExpanded={() =>
              setExpandedTargetModels((prev) => ({
                ...prev,
                [detailTarget.id]: !prev[detailTarget.id],
              }))
            }
            onToggleModel={(modelId) => toggleTargetModel(detailTarget.id, modelId)}
            onResetModels={() => resetTargetModels(detailTarget.id)}
            onCopyPath={() => void copyToClipboard(detailTarget.configPath, "配置路径")}
            backups={detailBackups}
            loadingBackups={detailBackupsLoading}
            restoringBackupId={restoringBackupId}
            onRestore={(backupId) => void restoreBackupFor(detailTarget, backupId)}
            onRefreshBackups={() => void loadDetailBackups(detailTarget.id)}
          />
        ) : null}
      </FullScreenPanel>
    </div>
  );
}
