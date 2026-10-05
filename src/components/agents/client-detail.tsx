import { CheckCircle2, Copy, History, Loader2, RefreshCw, RotateCcw } from "lucide-react";

import {
  CLAUDE_PROTOCOL_TARGETS,
  CLAUDE_SLOT_LIMIT,
  iconOf,
  metaOf,
} from "@/components/agents/client-meta";
import { ClientDetailQuota } from "@/components/agents/client-detail-quota";
import { ListItemRow } from "@/components/common/list-item-row";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Checkbox } from "@/components/ui/checkbox";
import type { AgentBackupItem, AgentClientTarget, GatewayModelItem } from "@/lib/types";
import { cn } from "@/lib/utils";

interface ClientDetailProps {
  target: AgentClientTarget;
  models: GatewayModelItem[];
  assignedModels: string[];
  isCustomized: boolean;
  expanded: boolean;
  onToggleExpanded: () => void;
  onToggleModel: (modelId: string) => void;
  onResetModels: () => void;
  onCopyPath: () => void;
  backups: AgentBackupItem[];
  loadingBackups: boolean;
  restoringBackupId: string | null;
  onRestore: (backupId?: string) => void;
  onRefreshBackups: () => void;
}

function formatTime(value: number): string {
  if (!value) return "未知时间";
  // 后端返回秒级时间戳时补齐成毫秒，避免渲染出 1970 年的记录。
  const ms = value < 1e12 ? value * 1000 : value;
  return new Date(ms).toLocaleString();
}

/**
 * 三级视图：单个客户端的配置详情（挂在 FullScreenPanel 内）。
 *
 * 三段式结构与 cc-switch 的全屏面板一致：顶部身份与配置文件信息，
 * 中间是模型槽位勾选（第 1 位为主模型），底部是该客户端的备份记录与回滚入口。
 */
export function ClientDetail({
  target,
  models,
  assignedModels,
  isCustomized,
  expanded,
  onToggleExpanded,
  onToggleModel,
  onResetModels,
  onCopyPath,
  backups,
  loadingBackups,
  restoringBackupId,
  onRestore,
  onRefreshBackups,
}: ClientDetailProps) {
  const meta = metaOf(target.id);
  const overLimit =
    CLAUDE_PROTOCOL_TARGETS.has(target.id) && assignedModels.length > CLAUDE_SLOT_LIMIT;
  const latestBackup = backups[0];

  return (
    <div className="space-y-5">
      {/* 客户端身份与配置文件 */}
      <Card className="gap-3 rounded-xl border border-border/70 p-4 shadow-none">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div className="flex min-w-0 items-center gap-3">
            <img src={iconOf(target.id)} alt="" className="size-10 shrink-0 object-contain" />
            <div className="min-w-0">
              <div className="flex flex-wrap items-center gap-1.5">
                <span className="truncate text-sm font-semibold">{target.label}</span>
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
              <p className="truncate text-[11px] text-muted-foreground">
                {meta.protocol} · <span className="font-mono">{meta.protocolBadge}</span>
              </p>
            </div>
          </div>
          <div className="flex shrink-0 flex-col items-end gap-0.5 font-mono text-[10.5px] text-muted-foreground">
            <span>{target.id}</span>
            <span>{target.version ?? "未检测到版本"}</span>
          </div>
        </div>

        <p className="text-[11.5px] leading-relaxed text-muted-foreground">{meta.description}</p>

        <div className="flex items-center justify-between gap-2 rounded-lg border border-border/50 bg-muted/30 px-3 py-2">
          <span className="truncate font-mono text-[11px] text-muted-foreground" title={target.configPath}>
            {target.configPath}
          </span>
          <Button
            type="button"
            variant="ghost"
            size="icon"
            className="size-6 shrink-0 rounded-md text-muted-foreground hover:text-foreground"
            aria-label="复制配置路径"
            title="复制文件路径"
            onClick={onCopyPath}
          >
            <Copy className="size-3" />
          </Button>
        </div>

        {target.note ? (
          <p className="text-[10.5px] leading-relaxed text-muted-foreground">{target.note}</p>
        ) : null}
      </Card>

      {/* 登录额度：仅对确实有本机登录凭证的客户端显示（Codex / Claude Code /
          Grok / Kimi）。用户在「智能体管理」里点开某个客户端时，最自然的
          问题就是「这个客户端还剩多少额度」。 */}
      <ClientDetailQuota targetId={target.id} />

      {/* 模型槽位分配 */}
      <Card className="gap-3 rounded-xl border border-border/70 p-4 shadow-none">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div className="flex flex-wrap items-center gap-2">
            <span className="text-[13px] font-semibold">模型槽位</span>
            <Badge variant="secondary" className="font-mono text-[10px]">
              已分配 {assignedModels.length} 个
            </Badge>
            {isCustomized ? (
              <Badge variant="outline" className="text-[10px]">
                已定制
              </Badge>
            ) : (
              <Badge variant="secondary" className="text-[10px]">
                跟随全局模型池
              </Badge>
            )}
            {overLimit ? (
              <Badge variant="warning" className="text-[10px]">
                超出 {CLAUDE_SLOT_LIMIT} 槽
              </Badge>
            ) : null}
          </div>
          <div className="flex flex-wrap items-center gap-1.5">
            {isCustomized ? (
              <Button
                type="button"
                variant="ghost"
                size="sm"
                className="h-7 text-[11px]"
                onClick={onResetModels}
              >
                <RotateCcw className="mr-1 size-3" />
                跟随全局模型池
              </Button>
            ) : null}
            <Button
              type="button"
              variant="outline"
              size="sm"
              className="h-7 text-[11px]"
              aria-expanded={expanded}
              onClick={onToggleExpanded}
            >
              {expanded ? "收起模型列表" : "自定义多选"}
            </Button>
          </div>
        </div>

        <p className="text-[11px] text-muted-foreground">
          排在第 1 位的模型会自动写入为该客户端的主模型；勾选结果在「更新配置 / 一键接入」时落盘。
        </p>

        {!expanded ? (
          <div className="flex flex-wrap gap-1">
            {assignedModels.length === 0 ? (
              <span className="text-[10.5px] text-muted-foreground">未分配模型</span>
            ) : (
              assignedModels.map((model, index) => (
                <span
                  key={model}
                  className={cn(
                    "rounded px-1.5 py-0.5 font-mono text-[10px]",
                    index === 0
                      ? "bg-primary/10 font-medium text-primary"
                      : "bg-muted text-muted-foreground",
                  )}
                >
                  {model}
                  {index === 0 ? " (主)" : ""}
                </span>
              ))
            )}
          </div>
        ) : models.length === 0 ? (
          <div className="rounded-xl border border-dashed border-border/70 bg-muted/15 px-4 py-6 text-center text-xs text-muted-foreground">
            上游模型池为空，请先在「兼容网关」启动网关并同步模型。
          </div>
        ) : (
          <div className="grid gap-1.5 sm:grid-cols-2 lg:grid-cols-3">
            {models.map((model) => {
              const index = assignedModels.indexOf(model.id);
              const checked = index >= 0;
              return (
                <div
                  key={model.id}
                  className={cn(
                    "flex items-center gap-2 rounded-lg border px-2.5 py-1.5 text-[11.5px] transition-colors",
                    checked ? "border-primary/50 bg-primary/5" : "border-border/60 bg-background",
                  )}
                >
                  <Checkbox
                    checked={checked}
                    onCheckedChange={() => onToggleModel(model.id)}
                    aria-label={`${target.label} 使用模型 ${model.id}`}
                  />
                  <button
                    type="button"
                    onClick={() => onToggleModel(model.id)}
                    className="min-w-0 flex-1 cursor-pointer truncate text-left font-mono outline-none focus-visible:ring-2 focus-visible:ring-ring/50"
                  >
                    {model.id}
                  </button>
                  {index === 0 ? (
                    <Badge className="h-4 px-1 text-[9px] font-normal">主</Badge>
                  ) : checked ? (
                    <span className="tabular-nums text-[10px] text-muted-foreground">
                      #{index + 1}
                    </span>
                  ) : null}
                </div>
              );
            })}
          </div>
        )}

        {overLimit ? (
          <div className="text-[10px] leading-relaxed text-amber-600 dark:text-amber-400">
            Claude 客户端仅支持 Sonnet / Opus / Haiku / Fable 四个模型槽位，
            一键接入时仅前 {CLAUDE_SLOT_LIMIT} 个模型生效
          </div>
        ) : null}
      </Card>

      {/* 该客户端的备份记录 */}
      <div className="space-y-3">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div>
            <h2 className="text-sm font-semibold">备份记录</h2>
            <p className="text-[11px] text-muted-foreground">
              共 {backups.length} 条，每次写入配置前自动创建时间戳快照，可随时无损回滚
            </p>
          </div>
          <div className="flex flex-wrap items-center gap-1.5">
            <Button
              type="button"
              variant="outline"
              size="sm"
              className="h-8 text-xs"
              onClick={onRefreshBackups}
              disabled={loadingBackups}
            >
              <RefreshCw className={cn("mr-1 size-3.5", loadingBackups && "animate-spin")} />
              刷新
            </Button>
            <Button
              type="button"
              size="sm"
              className="h-8 text-xs"
              disabled={!latestBackup || restoringBackupId !== null}
              onClick={() => onRestore()}
            >
              {restoringBackupId === "latest" ? (
                <Loader2 className="mr-1 size-3.5 animate-spin" />
              ) : (
                <RotateCcw className="mr-1 size-3.5" />
              )}
              恢复最近备份
            </Button>
          </div>
        </div>

        <div className="overflow-hidden rounded-xl border border-border/70 bg-card">
          {loadingBackups ? (
            <div className="flex items-center justify-center py-10 text-xs text-muted-foreground">
              <Loader2 className="mr-2 size-4 animate-spin" />
              正在读取备份记录...
            </div>
          ) : backups.length === 0 ? (
            <div className="flex flex-col items-center gap-2 px-4 py-10 text-center">
              <History className="size-6 text-muted-foreground/60" />
              <div className="text-xs text-muted-foreground">
                暂无备份记录（在首次写入配置后自动生成）
              </div>
            </div>
          ) : (
            backups.map((item, index) => (
              <ListItemRow key={item.id} isLast={index === backups.length - 1} className="py-3">
                <div className="flex size-8 shrink-0 items-center justify-center rounded-lg bg-muted/40 text-muted-foreground">
                  <History className="size-4" />
                </div>
                <div className="min-w-0 flex-1">
                  <div className="text-[12.5px] font-medium">{formatTime(item.createdAt)}</div>
                  <div className="truncate font-mono text-[10.5px] text-muted-foreground">
                    ID: {item.id}
                  </div>
                  {item.path ? (
                    <div className="truncate font-mono text-[10.5px] text-muted-foreground">
                      {item.path}
                    </div>
                  ) : null}
                </div>
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  className="h-7 shrink-0 text-xs"
                  disabled={restoringBackupId !== null}
                  onClick={() => onRestore(item.id)}
                >
                  {restoringBackupId === item.id ? (
                    <Loader2 className="mr-1 size-3 animate-spin" />
                  ) : (
                    <RotateCcw className="mr-1 size-3" />
                  )}
                  恢复此版本
                </Button>
              </ListItemRow>
            ))
          )}
        </div>
      </div>
    </div>
  );
}