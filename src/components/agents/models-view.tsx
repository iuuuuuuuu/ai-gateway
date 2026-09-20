import { useMemo, useState } from "react";
import { Link } from "react-router-dom";
import { Cpu, Loader2, Plus, RefreshCw, RotateCcw, Wand2 } from "lucide-react";

import {
  CLAUDE_PROTOCOL_TARGETS,
  CLAUDE_SLOT_LIMIT,
  iconOf,
  metaOf,
} from "@/components/agents/client-meta";
import { ListItemRow } from "@/components/common/list-item-row";
import { ManagementListSearch } from "@/components/common/management-list-search";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import type { AgentClientTarget, GatewayModelItem } from "@/lib/types";
import { cn } from "@/lib/utils";

interface ModelsViewProps {
  models: GatewayModelItem[];
  loadingModels: boolean;
  targets: AgentClientTarget[];
  activeClient: AgentClientTarget | null;
  customTargetModels: Record<string, string[]>;
  customInputModel: string;
  onCustomInputModelChange: (value: string) => void;
  onAddCustomModel: (e: React.FormEvent) => void;
  onSelectAll: () => void;
  onClearAll: () => void;
  onToggleTarget: (targetId: string, modelId: string) => void;
  getModelsForTarget: (targetId: string) => string[];
  onResetTarget: (targetId: string) => void;
  onRefreshModels: () => void;
  onImport: (target: AgentClientTarget) => void;
  onOpenDetail: (target: AgentClientTarget) => void;
  hasApiKey: boolean;
  operatingTarget: string | null;
  batchUpdating: boolean;
}

/** 槽位摘要（最多展示 N 个模型，第一个标记为主模型）。 */
function ModelChips({ assigned, limit = 3 }: { assigned: string[]; limit?: number }) {
  if (assigned.length === 0) {
    return <span className="text-[10.5px] text-muted-foreground">未分配模型</span>;
  }
  return (
    <div className="flex flex-wrap items-center gap-1">
      {assigned.slice(0, limit).map((m, i) => (
        <span
          key={m}
          className={cn(
            "rounded px-1.5 py-0.5 font-mono text-[10px]",
            i === 0 ? "bg-primary/10 font-medium text-primary" : "bg-muted text-muted-foreground",
          )}
        >
          {m}
          {i === 0 ? " (主)" : ""}
        </span>
      ))}
      {assigned.length > limit ? (
        <span className="text-[10px] text-muted-foreground">+{assigned.length - limit}</span>
      ) : null}
    </div>
  );
}

/**
 * 二级视图：模型分发。
 *
 * 上半部分是网关动态拉取的模型池（手动补充 / 重新拉取），
 * 下半部分是客户端槽位分配列表——第一位是默认主模型，直接写入对应配置文件。
 */
export function ModelsView({
  models,
  loadingModels,
  targets,
  activeClient,
  customTargetModels,
  customInputModel,
  onCustomInputModelChange,
  onAddCustomModel,
  onSelectAll,
  onClearAll,
  onToggleTarget,
  getModelsForTarget,
  onResetTarget,
  onRefreshModels,
  onImport,
  onOpenDetail,
  hasApiKey,
  operatingTarget,
  batchUpdating,
}: ModelsViewProps) {
  const [query, setQuery] = useState("");
  const [expandedOnly, setExpandedOnly] = useState<string | null>(null);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return targets;
    return targets.filter(
      (t) => t.label.toLowerCase().includes(q) || t.id.toLowerCase().includes(q),
    );
  }, [targets, query]);

  return (
    <div className="space-y-5">
      {/* 上游模型池 */}
      <Card className="gap-3 rounded-xl border border-border/70 p-4 shadow-none">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div>
            <div className="flex items-center gap-2">
              <Cpu className="size-4 text-primary" />
              <span className="text-[13px] font-semibold">上游模型池</span>
              <Badge variant="secondary" className="font-mono text-[10px]">
                {loadingModels ? "正在拉取..." : `${models.length} 个模型`}
              </Badge>
            </div>
            <p className="mt-1 text-xs text-muted-foreground">
              网关启动后自动从官方上游拉取账号下的全部真实可用模型，按 ID 权威去重，无静态预设。
            </p>
          </div>
          <div className="flex flex-wrap items-center gap-1.5">
            <Button
              variant="ghost"
              size="sm"
              className="h-7 px-2 text-[11px]"
              onClick={onSelectAll}
              disabled={models.length === 0}
            >
              全选所有客户端 ({models.length})
            </Button>
            <Button
              variant="ghost"
              size="sm"
              className="h-7 px-2 text-[11px] text-muted-foreground"
              onClick={onClearAll}
            >
              清空
            </Button>
            <Button
              variant="outline"
              size="sm"
              className="h-7 text-[11px]"
              onClick={onRefreshModels}
              disabled={loadingModels}
            >
              <RefreshCw className={cn("mr-1 size-3", loadingModels && "animate-spin")} />
              重新拉取
            </Button>
          </div>
        </div>

        {models.length === 0 ? (
          <div className="space-y-2.5 rounded-xl border border-dashed border-border/70 bg-muted/15 p-6 text-center">
            <Cpu className="mx-auto size-7 text-muted-foreground/60" />
            <div className="text-sm font-medium">尚未从网关获取到上游模型</div>
            <p className="mx-auto max-w-md text-xs text-muted-foreground">
              网关尚未启动或正在同步。前往「兼容网关」启动服务后，网关会自动拉取您账号下的全部真实可用模型。
            </p>
            <div className="flex items-center justify-center gap-2 pt-1">
              <Button size="sm" asChild className="h-7 text-xs">
                <Link to="/gateway">前往「兼容网关」启动网关 →</Link>
              </Button>
            </div>
          </div>
        ) : (
          <div className="flex max-h-44 flex-wrap gap-1.5 overflow-y-auto p-0.5">
            {models.map((m) => (
              <span
                key={m.id}
                className="rounded-lg border border-border/60 bg-muted/20 px-2 py-1 font-mono text-[11px] text-muted-foreground"
              >
                {m.id}
              </span>
            ))}
          </div>
        )}

        <form
          onSubmit={onAddCustomModel}
          className="flex items-center gap-2 border-t border-border/40 pt-3"
        >
          <Input
            type="text"
            placeholder="手动补充指定模型名称（若上游刚上线新模型）"
            value={customInputModel}
            onChange={(e) => onCustomInputModelChange(e.target.value)}
            className="h-8 max-w-sm font-mono text-xs"
          />
          <Button type="submit" size="sm" variant="secondary" className="h-8 text-xs">
            <Plus className="mr-1 size-3.5" />
            添加模型
          </Button>
        </form>
      </Card>

      {/* 客户端槽位分配 */}
      <div className="space-y-3">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div>
            <h2 className="text-sm font-semibold">客户端槽位分配</h2>
            <p className="text-[11px] text-muted-foreground">
              勾选写回到对应客户端配置文件；排在第 1 位的模型自动作为默认主模型。
            </p>
          </div>
          <ManagementListSearch
            className="sm:max-w-xs"
            value={query}
            onValueChange={setQuery}
            placeholder="搜索客户端"
            ariaLabel="搜索需要分配模型的客户端"
          />
        </div>

        <div className="overflow-hidden rounded-xl border border-border/70 bg-card">
          {filtered.length === 0 ? (
            <div className="px-4 py-10 text-center text-xs text-muted-foreground">
              {targets.length === 0 ? "正在检测本机智能体客户端..." : "没有匹配的客户端"}
            </div>
          ) : (
            filtered.map((target, index) => {
              const assigned = getModelsForTarget(target.id);
              const meta = metaOf(target.id);
              const isCustomized = Boolean(customTargetModels[target.id]);
              const isOperating = operatingTarget === target.id || batchUpdating;
              const expanded = expandedOnly === target.id;
              const overLimit =
                CLAUDE_PROTOCOL_TARGETS.has(target.id) && assigned.length > CLAUDE_SLOT_LIMIT;

              return (
                <div
                  key={target.id}
                  className={cn(!(index === filtered.length - 1) && "border-b border-border/60")}
                >
                  <ListItemRow className="gap-3 py-2.5 hover:bg-transparent">
                    <img src={iconOf(target.id)} alt="" className="size-7 shrink-0 object-contain" />

                    <button
                      type="button"
                      onClick={() => setExpandedOnly(expanded ? null : target.id)}
                      className="min-w-0 flex-1 cursor-pointer text-left outline-none focus-visible:ring-2 focus-visible:ring-ring/50"
                      aria-expanded={expanded}
                    >
                      <div className="flex flex-wrap items-center gap-1.5">
                        <span className="truncate text-[13px] font-semibold">{target.label}</span>
                        {isCustomized ? (
                          <Badge variant="outline" className="text-[10px]">
                            已定制
                          </Badge>
                        ) : (
                          <Badge variant="secondary" className="text-[10px]">
                            跟随全局
                          </Badge>
                        )}
                        {overLimit ? (
                          <Badge variant="warning" className="text-[10px]">
                            超出 {CLAUDE_SLOT_LIMIT} 槽
                          </Badge>
                        ) : null}
                      </div>
                      <div className="mt-1">
                        <ModelChips assigned={assigned} />
                      </div>
                    </button>

                    <div className="flex shrink-0 items-center gap-1.5">
                      <span className="hidden text-[10.5px] text-muted-foreground sm:inline">
                        {meta.protocol}
                      </span>
                      <Button
                        variant="ghost"
                        size="sm"
                        className="h-7 px-2 text-[11px]"
                        onClick={() => setExpandedOnly(expanded ? null : target.id)}
                      >
                        {expanded ? "收起" : "分配模型"}
                      </Button>
                      <Button
                        variant="ghost"
                        size="sm"
                        className="h-7 px-2 text-[11px] text-muted-foreground"
                        onClick={() => onOpenDetail(target)}
                      >
                        详情
                      </Button>
                      <Button
                        size="sm"
                        variant={target.configured ? "outline" : "default"}
                        className="h-7 px-3 text-xs font-medium"
                        disabled={isOperating || !hasApiKey || assigned.length === 0}
                        onClick={() => onImport(target)}
                      >
                        {isOperating ? (
                          <Loader2 className="mr-1 size-3 animate-spin" />
                        ) : target.configured ? (
                          <RotateCcw className="mr-1 size-3" />
                        ) : (
                          <Wand2 className="mr-1 size-3" />
                        )}
                        {target.configured ? "更新" : "接入"}
                      </Button>
                    </div>
                  </ListItemRow>

                  {expanded ? (
                    <div className="space-y-2 border-t border-border/40 bg-muted/10 px-4 py-3">
                      {models.length === 0 ? (
                        <div className="text-xs text-muted-foreground">
                          上游模型池为空，请先启动兼容网关。
                        </div>
                      ) : (
                        <div className="grid gap-1.5 sm:grid-cols-2 lg:grid-cols-3">
                          {models.map((model) => {
                            const idx = assigned.indexOf(model.id);
                            const checked = idx >= 0;
                            return (
                              <div
                                key={model.id}
                                className={cn(
                                  "flex items-center gap-2 rounded-lg border px-2.5 py-1.5 text-[11.5px] transition-colors",
                                  checked
                                    ? "border-primary/50 bg-primary/5"
                                    : "border-border/60 bg-background",
                                )}
                              >
                                <Checkbox
                                  checked={checked}
                                  onCheckedChange={() => onToggleTarget(target.id, model.id)}
                                  aria-label={`${target.label} 使用模型 ${model.id}`}
                                />
                                <button
                                  type="button"
                                  onClick={() => onToggleTarget(target.id, model.id)}
                                  className="min-w-0 flex-1 cursor-pointer truncate text-left font-mono outline-none focus-visible:ring-2 focus-visible:ring-ring/50"
                                >
                                  {model.id}
                                </button>
                                {idx === 0 ? (
                                  <Badge className="h-4 px-1 text-[9px] font-normal">主</Badge>
                                ) : checked ? (
                                  <span className="tabular-nums text-[10px] text-muted-foreground">
                                    #{idx + 1}
                                  </span>
                                ) : null}
                              </div>
                            );
                          })}
                        </div>
                      )}

                      <div className="flex flex-wrap items-center justify-between gap-2 pt-1">
                        <div className="text-[10px] leading-relaxed text-amber-600 dark:text-amber-400">
                          {overLimit
                            ? `Claude 客户端仅支持 Sonnet / Opus / Haiku / Fable 四个槽位，一键接入时仅前 ${CLAUDE_SLOT_LIMIT} 个模型生效`
                            : ""}
                        </div>
                        <div className="flex items-center gap-1.5">
                          {isCustomized ? (
                            <Button
                              variant="ghost"
                              size="sm"
                              className="h-7 text-[11px]"
                              onClick={() => onResetTarget(target.id)}
                            >
                              <RotateCcw className="mr-1 size-3" />
                              跟随全局模型池
                            </Button>
                          ) : null}
                          <Button
                            variant="outline"
                            size="sm"
                            className="h-7 text-[11px]"
                            onClick={() => onOpenDetail(target)}
                          >
                            打开详情面板
                          </Button>
                        </div>
                      </div>
                    </div>
                  ) : null}
                </div>
              );
            })
          )}
        </div>

        {activeClient ? (
          <p className="text-[10.5px] text-muted-foreground">
            当前一级选择：{activeClient.label}。切换头部图标可同步其他客户端的槽位分配。
          </p>
        ) : null}
      </div>
    </div>
  );
}