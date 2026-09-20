import { History, Loader2, RefreshCw, RotateCcw } from "lucide-react";

import { iconOf, metaOf } from "@/components/agents/client-meta";
import { ListItemRow } from "@/components/common/list-item-row";
import { ManagementListSearch } from "@/components/common/management-list-search";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import type { AgentBackupItem, AgentClientTarget } from "@/lib/types";

interface BackupsViewProps {
  activeClient: AgentClientTarget | null;
  backups: AgentBackupItem[];
  totalBackups: number;
  backupQuery: string;
  onBackupQueryChange: (value: string) => void;
  loadingBackups: boolean;
  restoringBackupId: string | null;
  onRestore: (backupId: string) => void;
  onRestoreLatest: () => void;
  onRefresh: () => void;
  onOpenDetail: (target: AgentClientTarget) => void;
}

function formatTime(value: number): string {
  if (!value) return "未知时间";
  // 后端返回秒级时间戳时补齐成毫秒，避免渲染出 1970 年的记录。
  const ms = value < 1e12 ? value * 1000 : value;
  return new Date(ms).toLocaleString();
}

/**
 * 二级视图：配置备份。
 *
 * 每次接入 / 更新配置前系统都会自动创建一份时间戳备份，这里按当前客户端列出并可无损回滚。
 * 切换头部一级菜单的客户端图标即可查看其他客户端的备份。
 */
export function BackupsView({
  activeClient,
  backups,
  totalBackups,
  backupQuery,
  onBackupQueryChange,
  loadingBackups,
  restoringBackupId,
  onRestore,
  onRestoreLatest,
  onRefresh,
  onOpenDetail,
}: BackupsViewProps) {
  if (!activeClient) {
    return (
      <div className="rounded-xl border border-border/70 bg-card px-4 py-10 text-center text-xs text-muted-foreground">
        正在检测本机智能体客户端...
      </div>
    );
  }

  const meta = metaOf(activeClient.id);

  return (
    <div className="space-y-4">
      <Card className="gap-3 rounded-xl border border-border/70 p-4 shadow-none">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div className="flex min-w-0 items-center gap-3">
            <img src={iconOf(activeClient.id)} alt="" className="size-9 shrink-0 object-contain" />
            <div className="min-w-0">
              <div className="flex flex-wrap items-center gap-1.5">
                <span className="truncate text-sm font-semibold">{activeClient.label}</span>
                <Badge variant="secondary" className="text-[10px]">
                  {totalBackups} 份备份
                </Badge>
              </div>
              <p className="truncate text-[11px] text-muted-foreground">
                {meta.protocol} · <span className="font-mono">{meta.protocolBadge}</span>
              </p>
            </div>
          </div>

          <div className="flex flex-wrap items-center gap-1.5">
            <Button
              variant="outline"
              size="sm"
              className="h-8 text-xs"
              onClick={onRefresh}
              disabled={loadingBackups}
            >
              <RefreshCw className={cnSpin(loadingBackups)} />
              刷新
            </Button>
            <Button
              variant="outline"
              size="sm"
              className="h-8 text-xs"
              onClick={() => onOpenDetail(activeClient)}
            >
              配置详情
            </Button>
            <Button
              size="sm"
              className="h-8 text-xs"
              disabled={totalBackups === 0 || restoringBackupId !== null}
              onClick={onRestoreLatest}
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
        <p className="text-[11px] text-muted-foreground">
          每次执行接入或更新配置前系统均会自动创建时间戳备份，可随时无损回滚。配置文件路径：
          <span className="ml-1 font-mono">{activeClient.configPath}</span>
        </p>
      </Card>

      <div className="space-y-3">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div>
            <h2 className="text-sm font-semibold">备份记录</h2>
            <p className="text-[11px] text-muted-foreground">
              共 {totalBackups} 条，倒序展示最近写入的配置快照
            </p>
          </div>
          <ManagementListSearch
            className="sm:max-w-xs"
            value={backupQuery}
            onValueChange={onBackupQueryChange}
            placeholder="搜索时间 / ID / 路径"
            ariaLabel="搜索备份记录"
          />
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
                {totalBackups === 0
                  ? "暂无备份记录（在首次写入配置后自动生成）"
                  : "没有匹配的备份记录"}
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

/** 刷新按钮里的图标旋转类名。 */
function cnSpin(spinning: boolean): string {
  return spinning ? "mr-1 size-3.5 animate-spin" : "mr-1 size-3.5";
}