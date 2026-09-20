import { useCallback, useEffect, useMemo, useState } from "react";
import {
  AlertTriangle,
  ArrowLeft,
  Bot,
  Layers,
  LayoutGrid,
  ListChecks,
  Loader2,
  RefreshCw,
  RotateCcw,
  Wand2,
  Zap,
} from "lucide-react";
import { toast } from "sonner";

import { AgentSwitcher } from "@/components/agents/agent-switcher";
import { BackupsView } from "@/components/agents/backups-view";
import { ClientDetail } from "@/components/agents/client-detail";
import { metaOf, toClientItems } from "@/components/agents/client-meta";
import { ModelsView } from "@/components/agents/models-view";
import { OverviewView } from "@/components/agents/overview-view";
import { FullScreenPanel } from "@/components/common/full-screen-panel";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import * as api from "@/lib/api";
import type {
  AgentBackupItem,
  AgentClientTarget,
  AgentDetectionResult,
  GatewayModelItem,
  GatewayStatus,
} from "@/lib/types";
import { cn } from "@/lib/utils";

/** 二级视图。 */
export type AgentView = "overview" | "models" | "backups";

interface ViewDef {
  id: AgentView;
  label: string;
  icon: typeof Bot;
  description: string;
}

const VIEWS: ViewDef[] = [
  {
    id: "overview",
    label: "概览",
    icon: LayoutGrid,
    description: "本机智能体检测结果、接入状态与一键操作",
  },
  {
    id: "models",
    label: "模型分发",
    icon: ListChecks,
    description: "从网关动态拉取模型池，按客户端分配槽位",
  },
  {
    id: "backups",
    label: "配置备份",
    icon: Layers,
    description: "每次写入配置前自动创建的时间戳备份，可随时无损回滚",
  },
];

/** 视图 / 客户端选择的本地记忆键，与 cc-switch 的 VIEW_STORAGE_KEY 同思路。 */
const AGENT_VIEW_KEY = "ai-gateway-agents-view";
const AGENT_CLIENT_KEY = "ai-gateway-agents-client";

function readStoredView(): AgentView {
  if (typeof localStorage === "undefined") return "overview";
  const saved = localStorage.getItem(AGENT_VIEW_KEY);
  return VIEWS.some((v) => v.id === saved) ? (saved as AgentView) : "overview";
}

export default function AgentsPage() {
  const [loading, setLoading] = useState(false);
  const [detection, setDetection] = useState<AgentDetectionResult | null>(null);
  const [gatewayStatus, setGatewayStatus] = useState<GatewayStatus | null>(null);
  const [gatewayModels, setGatewayModels] = useState<GatewayModelItem[]>([]);
  const [loadingModels, setLoadingModels] = useState(false);

  // 二级视图 + 一级客户端选择 + 三级详情目标
  const [view, setView] = useState<AgentView>(readStoredView);
  const [activeClientId, setActiveClientId] = useState<string>(() =>
    typeof localStorage === "undefined" ? "" : localStorage.getItem(AGENT_CLIENT_KEY) || "",
  );
  const [detailTargetId, setDetailTargetId] = useState<string | null>(null);

  // 全局模型多选（从网关动态拉取）
  const [selectedGlobalModels, setSelectedGlobalModels] = useState<string[]>([]);

  // 每个客户端单独定制的模型覆盖
  const [customTargetModels, setCustomTargetModels] = useState<Record<string, string[]>>({});
  const [expandedTargetModels, setExpandedTargetModels] = useState<Record<string, boolean>>({});

  // 用户手动添加模型
  const [customInputModel, setCustomInputModel] = useState("");

  const [operatingTarget, setOperatingTarget] = useState<string | null>(null);
  const [batchUpdating, setBatchUpdating] = useState(false);

  // 列表本地搜索
  const [clientQuery, setClientQuery] = useState("");
  const [backupQuery, setBackupQuery] = useState("");

  // 备份记录
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

  useEffect(() => {
    if (typeof localStorage === "undefined") return;
    localStorage.setItem(AGENT_VIEW_KEY, view);
  }, [view]);

  useEffect(() => {
    if (typeof localStorage === "undefined") return;
    if (activeClientId) localStorage.setItem(AGENT_CLIENT_KEY, activeClientId);
  }, [activeClientId]);

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
      setGatewayModels((prev) => [{ id, name: id, owned_by: "custom" }, ...prev]);
    }
    if (!selectedGlobalModels.includes(id)) {
      setSelectedGlobalModels((prev) => [id, ...prev]);
    }
    setCustomInputModel("");
    toast.success(`已添加并选中模型: ${id}`);
  };

  // 获取当前 target 分配的模型列表（未定制时跟随全局模型池）
  const getModelsForTarget = useCallback(
    (targetId: string): string[] => customTargetModels[targetId] ?? selectedGlobalModels,
    [customTargetModels, selectedGlobalModels],
  );

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

  // 放弃该客户端的定制，重新跟随全局模型池
  const resetTargetModels = (targetId: string) => {
    setCustomTargetModels((prev) => {
      const next = { ...prev };
      delete next[targetId];
      return next;
    });
    toast.info("已重置为跟随全局模型池");
  };

  // 读取某个客户端的备份记录
  const loadBackups = useCallback(async (target: AgentClientTarget, quiet = false) => {
    setActiveBackupTarget(target);
    if (!quiet) setLoadingBackups(true);
    try {
      const res = await api.listAgentBackups(target.id);
      setBackups(res.backups || []);
    } catch (err) {
      if (!quiet) toast.error("读取备份记录失败: " + String(err));
      setBackups([]);
    } finally {
      if (!quiet) setLoadingBackups(false);
    }
  }, []);

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
      // 写入会新建一份时间戳备份，备份视图要立刻反映出来
      await loadBackups(target, true);
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
    if (activeBackupTarget) await loadBackups(activeBackupTarget, true);

    if (failCount === 0) {
      toast.success(
        `一键更新完成！已成功将 ${successCount} 个客户端同步更新为当前最新网关配置与多模型组合`,
      );
    } else {
      toast.info(`更新完毕：${successCount} 个成功，${failCount} 个失败`);
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
      await fetchData(true);
      await loadBackups(activeBackupTarget, true);
    } catch (err) {
      toast.error("恢复配置失败: " + String(err));
    } finally {
      setRestoringBackupId(null);
    }
  };

  const hasApiKey = Boolean(gatewayStatus?.config?.api_key || detection?.hasApiKey);
  const targets = useMemo(() => detection?.targets ?? [], [detection]);

  const installedCount = useMemo(() => targets.filter((t) => t.installed).length, [targets]);
  const configuredCount = useMemo(() => targets.filter((t) => t.configured).length, [targets]);

  // 一级客户端选择的权威落点：选中项被移除时自动回落到第一个客户端。
  const activeClient = useMemo(
    () => targets.find((t) => t.id === activeClientId) ?? targets[0] ?? null,
    [targets, activeClientId],
  );

  const detailTarget = useMemo(
    () => (detailTargetId ? (targets.find((t) => t.id === detailTargetId) ?? null) : null),
    [targets, detailTargetId],
  );

  // 一级选择变化时把备份列表切到对应客户端（二级/三级视图共享同一份数据）
  useEffect(() => {
    if (!activeClient) return;
    if (activeBackupTarget?.id === activeClient.id) return;
    void loadBackups(activeClient);
  }, [activeClient, activeBackupTarget?.id, loadBackups]);

  const switcherItems = useMemo(() => toClientItems(targets), [targets]);

  const filteredClients = useMemo(() => {
    const q = clientQuery.trim().toLowerCase();
    if (!q) return targets;
    return targets.filter((target) => {
      const meta = metaOf(target.id);
      return (
        target.label.toLowerCase().includes(q) ||
        target.id.toLowerCase().includes(q) ||
        meta.protocol.toLowerCase().includes(q) ||
        target.configPath.toLowerCase().includes(q)
      );
    });
  }, [targets, clientQuery]);

  const filteredBackups = useMemo(() => {
    const q = backupQuery.trim().toLowerCase();
    if (!q) return backups;
    return backups.filter(
      (item) =>
        item.id.toLowerCase().includes(q) ||
        new Date(item.createdAt).toLocaleString().toLowerCase().includes(q) ||
        (item.path ?? "").toLowerCase().includes(q),
    );
  }, [backups, backupQuery]);

  const detailModels = detailTarget ? getModelsForTarget(detailTarget.id) : [];

  // 备份列表归属校验：还没切到目标客户端的备份时一律视为「加载中」，
  // 避免下钻瞬间把上一个客户端的备份记录显示成当前客户端的。
  const detailBackupsLoading = detailTarget
    ? loadingBackups || activeBackupTarget?.id !== detailTarget.id
    : false;
  const detailBackups = detailTarget && activeBackupTarget?.id === detailTarget.id ? backups : [];
  const currentView = VIEWS.find((v) => v.id === view) ?? VIEWS[0];

  // 打开三级详情面板：同时把一级选择切到该客户端，保持层级与选中态一致。
  const openDetail = (target: AgentClientTarget) => {
    setActiveClientId(target.id);
    setDetailTargetId(target.id);
  };

  return (
    <div className="flex min-h-full flex-col">
      {/* 顶部操作栏：二级视图标题 + 一级客户端分段切换 + 主操作 */}
      <header className="sticky top-0 z-30 border-b border-border/60 bg-background/80 backdrop-blur-md">
        <div className="flex h-16 items-center gap-2 px-4 sm:px-6">
          <div className="flex min-w-0 shrink-0 items-center gap-2">
            {view === "overview" ? (
              <>
                <div className="flex size-9 shrink-0 items-center justify-center rounded-xl bg-primary/10 text-primary">
                  <Bot className="size-5" />
                </div>
                <div className="hidden min-w-0 sm:block">
                  <h1 className="truncate text-base font-semibold tracking-tight">智能体管理</h1>
                  <p className="truncate text-[11px] text-muted-foreground">
                    已接入 {configuredCount} / {targets.length} · 本机已安装 {installedCount}
                  </p>
                </div>
              </>
            ) : (
              <>
                <Button
                  variant="outline"
                  size="icon"
                  className="shrink-0 rounded-lg"
                  aria-label="返回概览"
                  onClick={() => setView("overview")}
                >
                  <ArrowLeft className="size-4" />
                </Button>
                <div className="min-w-0">
                  <h1 className="truncate text-base font-semibold tracking-tight">
                    {currentView.label}
                  </h1>
                  <p className="hidden truncate text-[11px] text-muted-foreground sm:block">
                    {currentView.description}
                  </p>
                </div>
              </>
            )}
          </div>

          {/* 弹性中段：空间不足时由 AgentSwitcher 自行收纳溢出客户端 */}
          <div className="flex min-w-0 flex-1 items-center justify-end overflow-hidden">
            {switcherItems.length > 0 ? (
              <AgentSwitcher
                items={switcherItems}
                activeId={activeClient?.id ?? ""}
                onSelect={setActiveClientId}
              />
            ) : null}
          </div>

          {/* 固定右端：主操作 */}
          <div className="flex shrink-0 items-center gap-1.5">
            <Button
              size="sm"
              className="h-8 gap-1.5 px-3 text-xs font-medium shadow-xs"
              disabled={batchUpdating || !hasApiKey || installedCount === 0}
              onClick={() => void handleBatchUpdate()}
            >
              {batchUpdating ? (
                <Loader2 className="size-3.5 animate-spin" />
              ) : (
                <Zap className="size-3.5" />
              )}
              <span className="hidden sm:inline">一键更新全部</span>
            </Button>
            <Button
              variant="outline"
              size="icon"
              className="size-8"
              aria-label="刷新状态"
              onClick={() => {
                void fetchData();
                void fetchModels();
              }}
              disabled={loading || batchUpdating}
            >
              <RefreshCw className={cn("size-3.5", loading && "animate-spin")} />
            </Button>
          </div>
        </div>
      </header>

      <div className="mx-auto w-full max-w-6xl flex-1 space-y-5 p-4 sm:p-6">
        {/* 二级菜单：视图切换 */}
        <Tabs value={view} onValueChange={(value) => setView(value as AgentView)}>
          <TabsList className="h-9 w-full justify-start gap-1 sm:w-fit">
            {VIEWS.map((item) => {
              const Icon = item.icon;
              return (
                <TabsTrigger key={item.id} value={item.id} className="h-7 gap-1.5 px-3">
                  <Icon className="size-3.5" />
                  {item.label}
                  {item.id === "backups" && backups.length > 0 ? (
                    <span className="tabular-nums text-muted-foreground">({backups.length})</span>
                  ) : null}
                </TabsTrigger>
              );
            })}
          </TabsList>
        </Tabs>

        {/* 缺少 API Key 时的提示 */}
        {!hasApiKey && !loading ? (
          <Alert
            variant="destructive"
            className="border-amber-500/50 bg-amber-500/10 text-amber-900 dark:text-amber-200"
          >
            <AlertTriangle className="size-4 text-amber-600 dark:text-amber-400" />
            <AlertTitle className="text-xs font-semibold">网关尚未配置 API Key</AlertTitle>
            <AlertDescription className="text-xs">
              Claude Code、Codex、OpenCode 等客户端请求必须携带 Authorization 凭据才能通过网关鉴权。
              请先在「兼容网关」页面填写并保存 API Key。
            </AlertDescription>
          </Alert>
        ) : null}

        {view === "overview" ? (
          <OverviewView
            loading={loading}
            targets={targets}
            filteredClients={filteredClients}
            clientQuery={clientQuery}
            onClientQueryChange={setClientQuery}
            configuredCount={configuredCount}
            installedCount={installedCount}
            gatewayModelCount={gatewayModels.length}
            loadingModels={loadingModels}
            gatewayStatus={gatewayStatus}
            hasApiKey={hasApiKey}
            operatingTarget={operatingTarget}
            batchUpdating={batchUpdating}
            getModelsForTarget={getModelsForTarget}
            onImport={(target) => void handleImport(target)}
            onOpenDetail={openDetail}
            onOpenBackups={(target) => {
              setView("backups");
              void loadBackups(target);
            }}
          />
        ) : null}

        {view === "models" ? (
          <ModelsView
            models={gatewayModels}
            loadingModels={loadingModels}
            activeClient={activeClient}
            targets={targets}
            customTargetModels={customTargetModels}
            customInputModel={customInputModel}
            onCustomInputModelChange={setCustomInputModel}
            onAddCustomModel={handleAddCustomModel}
            onSelectAll={() => setSelectedGlobalModels(gatewayModels.map((m) => m.id))}
            onClearAll={() => setSelectedGlobalModels([])}
            onToggleTarget={toggleTargetModel}
            getModelsForTarget={getModelsForTarget}
            onResetTarget={resetTargetModels}
            onRefreshModels={() => void fetchModels()}
            onImport={(target) => void handleImport(target)}
            onOpenDetail={openDetail}
            hasApiKey={hasApiKey}
            operatingTarget={operatingTarget}
            batchUpdating={batchUpdating}
          />
        ) : null}

        {view === "backups" ? (
          <BackupsView
            activeClient={activeClient}
            backups={filteredBackups}
            totalBackups={backups.length}
            backupQuery={backupQuery}
            onBackupQueryChange={setBackupQuery}
            loadingBackups={loadingBackups}
            restoringBackupId={restoringBackupId}
            onRestore={(id) => void handleRestore(id)}
            onRestoreLatest={() => void handleRestore()}
            onRefresh={() => {
              if (activeClient) void loadBackups(activeClient);
            }}
            onOpenDetail={openDetail}
          />
        ) : null}
      </div>

      {/* 三级菜单：客户端全屏详情面板 */}
      <FullScreenPanel
        isOpen={Boolean(detailTarget)}
        motionPreset="slide-from-right"
        title={detailTarget ? detailTarget.label : ""}
        description={
          detailTarget
            ? `${metaOf(detailTarget.id).protocol} · ${metaOf(detailTarget.id).protocolBadge}`
            : undefined
        }
        onClose={() => setDetailTargetId(null)}
        footer={
          detailTarget ? (
            <>
              <Button variant="outline" size="sm" onClick={() => setDetailTargetId(null)}>
                关闭
              </Button>
              <TooltipProvider>
                <Tooltip>
                  <TooltipTrigger asChild>
                    <span>
                      <Button
                        size="sm"
                        disabled={operatingTarget !== null || batchUpdating || !hasApiKey}
                        onClick={() => void handleImport(detailTarget)}
                      >
                        {operatingTarget === detailTarget.id ? (
                          <Loader2 className="mr-1 size-3.5 animate-spin" />
                        ) : detailTarget.configured ? (
                          <RotateCcw className="mr-1 size-3.5" />
                        ) : (
                          <Wand2 className="mr-1 size-3.5" />
                        )}
                        {detailTarget.configured ? "更新配置" : "一键接入"}
                      </Button>
                    </span>
                  </TooltipTrigger>
                  {!hasApiKey ? (
                    <TooltipContent className="text-xs">请先在「兼容网关」设置 API Key</TooltipContent>
                  ) : null}
                </Tooltip>
              </TooltipProvider>
            </>
          ) : null
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
            onRestore={(id) => void handleRestore(id)}
            onRefreshBackups={() => void loadBackups(detailTarget)}
          />
        ) : null}
      </FullScreenPanel>
    </div>
  );
}