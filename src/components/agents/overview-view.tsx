import {
  CheckCircle2,
  Cpu,
  History,
  Laptop,
  Loader2,
  Network,
  RotateCcw,
  Wand2,
} from "lucide-react";

import { iconOf, metaOf } from "@/components/agents/client-meta";
import { ListItemRow } from "@/components/common/list-item-row";
import { ManagementListSearch } from "@/components/common/management-list-search";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import type { AgentClientTarget, GatewayStatus } from "@/lib/types";
import { cn } from "@/lib/utils";

interface OverviewViewProps {
  loading: boolean;
  targets: AgentClientTarget[];
  filteredClients: AgentClientTarget[];
  clientQuery: string;
  onClientQueryChange: (value: string) => void;
  configuredCount: number;
  installedCount: number;
  gatewayModelCount: number;
  loadingModels: boolean;
  gatewayStatus: GatewayStatus | null;
  hasApiKey: boolean;
  operatingTarget: string | null;
  batchUpdating: boolean;
  getModelsForTarget: (targetId: string) => string[];
  onImport: (target: AgentClientTarget) => void;
  onOpenDetail: (target: AgentClientTarget) => void;
  onOpenBackups: (target: AgentClientTarget) => void;
}

/** 顶部核心运行态数据卡。 */
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
    <Card className="flex flex-row items-center justify-between gap-3 rounded-xl border border-border/60 bg-card p-3.5 shadow-none sm:p-4">
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
        {subtext ? <div className="text-[10.5px] text-muted-foreground">{subtext}</div> : null}
      </div>
      {Icon ? (
        <div className="flex size-9 shrink-0 items-center justify-center rounded-xl bg-muted/40 text-muted-foreground">
          <Icon className="size-4.5" />
        </div>
      ) : null}
    </Card>
  );
}

function StatusBadge({ target }: { target: AgentClientTarget }) {
  if (target.configured) {
    return (
      <Badge className="border-emerald-500/30 bg-emerald-50 text-[10px] text-emerald-700 dark:bg-emerald-950/40 dark:text-emerald-300">
        <CheckCircle2 className="mr-1 size-3" />
        已接入
      </Badge>
    );
  }
  if (target.installed) {
    return (
      <Badge variant="secondary" className="text-[10px]">
        {target.version ? target.version : "已安装"}
      </Badge>
    );
  }
  return (
    <Badge variant="outline" className="text-[10px] text-muted-foreground">
      未安装
    </Badge>
  );
}

/**
 * 二级视图：概览。
 *
 * 列表采用 cc-switch 管理面板的行式布局（`rounded-xl border` + `ListItemRow`），
 * 而非卡片网格；每行右端是主操作，行本身可点击下钻到三级详情。
 */
export function OverviewView({
  loading,
  targets,
  filteredClients,
  clientQuery,
  onClientQueryChange,
  configuredCount,
  installedCount,
  gatewayModelCount,
  loadingModels,
  gatewayStatus,
  hasApiKey,
  operatingTarget,
  batchUpdating,
  getModelsForTarget,
  onImport,
  onOpenDetail,
  onOpenBackups,
}: OverviewViewProps) {
  return (
    <div className="space-y-5">
      {/* 核心运行态数据卡 */}
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
          value={loadingModels ? "..." : gatewayModelCount}
          subtext="来自网关实时同步"
          icon={Cpu}
        />
        <StatCard
          label="网关服务状态"
          value={gatewayStatus?.running ? "在线" : "未启动"}
          subtext={
            gatewayStatus?.running ? `端口 :${gatewayStatus?.port || 7863}` : "前往兼容网关启动"
          }
          tone={gatewayStatus?.running ? "ok" : "warn"}
          icon={Network}
        />
      </div>

      {/* 客户端列表 */}
      <div className="space-y-3">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div>
            <h2 className="text-sm font-semibold">智能体客户端</h2>
            <p className="text-[11px] text-muted-foreground">
              共 {targets.length} 类客户端，点击任意一行查看配置详情与模型槽位
            </p>
          </div>
          <ManagementListSearch
            className="sm:max-w-xs"
            value={clientQuery}
            onValueChange={onClientQueryChange}
            placeholder="搜索客户端 / 协议 / 路径"
            ariaLabel="搜索智能体客户端"
          />
        </div>

        <div className="overflow-hidden rounded-xl border border-border/70 bg-card">
          {loading && targets.length === 0 ? (
            <div className="space-y-2 p-4">
              {Array.from({ length: 4 }).map((_, i) => (
                <Skeleton key={i} className="h-14 w-full rounded-lg" />
              ))}
            </div>
          ) : filteredClients.length === 0 ? (
            <div className="px-4 py-10 text-center text-xs text-muted-foreground">
              {targets.length === 0 ? "正在检测本机智能体客户端..." : "没有匹配的客户端"}
            </div>
          ) : (
            filteredClients.map((target, index) => {
              const meta = metaOf(target.id);
              const assignedModels = getModelsForTarget(target.id);
              const isOperating = operatingTarget === target.id || batchUpdating;
              const isLast = index === filteredClients.length - 1;

              return (
                <ListItemRow key={target.id} isLast={isLast} className="items-start gap-3 py-3">
                  <img src={iconOf(target.id)} alt="" className="mt-0.5 size-8 shrink-0 object-contain" />

                  <button
                    type="button"
                    onClick={() => onOpenDetail(target)}
                    className="min-w-0 flex-1 cursor-pointer text-left outline-none focus-visible:ring-2 focus-visible:ring-ring/50"
                  >
                    <div className="flex flex-wrap items-center gap-1.5">
                      <span className="truncate text-[13.5px] font-semibold text-foreground">
                        {target.label}
                      </span>
                      <StatusBadge target={target} />
                    </div>
                    <div className="mt-0.5 truncate text-[11px] text-muted-foreground">
                      {meta.protocol} · <span className="font-mono">{meta.protocolBadge}</span>
                    </div>
                    <div className="mt-1.5 flex flex-wrap items-center gap-1 text-[10px]">
                      <span className="text-muted-foreground">
                        配置模型 {assignedModels.length} 个：
                      </span>
                      {assignedModels.slice(0, 4).map((m, i) => (
                        <span
                          key={m}
                          className={cn(
                            "rounded px-1.5 py-0.5 font-mono",
                            i === 0
                              ? "bg-primary/10 font-medium text-primary"
                              : "bg-muted text-muted-foreground",
                          )}
                        >
                          {m}
                          {i === 0 ? " (主)" : ""}
                        </span>
                      ))}
                      {assignedModels.length > 4 ? (
                        <span className="text-muted-foreground">
                          +{assignedModels.length - 4}
                        </span>
                      ) : null}
                    </div>
                  </button>

                  <div className="flex shrink-0 items-center gap-1.5">
                    <Button
                      variant="ghost"
                      size="sm"
                      className="h-7 px-2 text-[11px] text-muted-foreground"
                      onClick={() => onOpenBackups(target)}
                    >
                      <History className="mr-1 size-3" />
                      备份
                    </Button>
                    <TooltipProvider>
                      <Tooltip>
                        <TooltipTrigger asChild>
                          <span>
                            <Button
                              size="sm"
                              variant={target.configured ? "outline" : "default"}
                              className={cn(
                                "h-7 px-3 text-xs font-medium",
                                target.configured &&
                                  "border-emerald-500/30 bg-emerald-50 text-emerald-800 hover:bg-emerald-100 dark:bg-emerald-950/40 dark:text-emerald-200 dark:hover:bg-emerald-900/50",
                              )}
                              disabled={isOperating || !hasApiKey}
                              onClick={() => onImport(target)}
                            >
                              {isOperating ? (
                                <Loader2 className="mr-1 size-3 animate-spin" />
                              ) : target.configured ? (
                                <RotateCcw className="mr-1 size-3" />
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
                </ListItemRow>
              );
            })
          )}
        </div>
      </div>
    </div>
  );
}