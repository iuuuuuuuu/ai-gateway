import { useCallback, useEffect, useState } from "react";
import { toast } from "sonner";
import {
  ChevronLeft,
  ChevronRight,
  Eraser,
  FileText,
  Filter,
  Loader2,
  RefreshCw,
  Search,
} from "lucide-react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { DemoAction } from "@/components/demo-action";
import * as api from "@/lib/api";
import { cn } from "@/lib/utils";

const PAGE_SIZE = 25;

/** 状态码语义色：2xx 绿、4xx 黄、5xx 红，其余中性。 */
function statusTone(status: string): string {
  const code = Number.parseInt(status, 10);
  if (Number.isNaN(code)) return "text-muted-foreground";
  if (code >= 200 && code < 300) return "text-emerald-600 dark:text-emerald-400";
  if (code >= 400 && code < 500) return "text-amber-600 dark:text-amber-400";
  if (code >= 500) return "text-destructive";
  return "text-muted-foreground";
}

function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB"];
  const i = Math.min(units.length - 1, Math.floor(Math.log(bytes) / Math.log(1024)));
  const value = bytes / 1024 ** i;
  return `${value >= 10 || i === 0 ? Math.round(value) : value.toFixed(1)} ${units[i]}`;
}

/**
 * 抓包日志查看器。
 *
 * ## 为什么需要它
 *
 * 代理把每次请求写进 `logs/proxy_req_YYYY-MM-DD.log`（含脱敏后的头与正文预览），
 * 但此前只能自己去文件夹里翻 —— 而这些内容恰恰是「这次请求为什么失败」
 * 需要**当场**看的。这里把「列条目 → 看详情」接上。
 *
 * ## 与操作日志的区别
 *
 * 只看抓包日志。`proxy.log` 是代理自身的启动/停止/错误日志（逐行而非分块），
 * 形态完全不同，混在一起解析会让两边都难看。
 */
export function ProxyLogsPanel() {
  const [entries, setEntries] = useState<api.ProxyLogEntry[]>([]);
  const [total, setTotal] = useState(0);
  const [overview, setOverview] = useState<api.ProxyLogsOverview | null>(null);
  const [page, setPage] = useState(0);
  const [loading, setLoading] = useState(true);
  const [detail, setDetail] = useState<{ id: string; content: string } | null>(null);
  const [detailLoading, setDetailLoading] = useState(false);

  // 筛选条件分「输入中」与「已生效」两份：只有点了查询才生效，
  // 否则每敲一个字符都会打一次接口。
  const [keywordInput, setKeywordInput] = useState("");
  const [keyword, setKeyword] = useState("");
  const [startTime, setStartTime] = useState("");
  const [endTime, setEndTime] = useState("");
  const [filterOpen, setFilterOpen] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const res = await api.proxyLogsList({
        keyword: keyword || undefined,
        startTime: startTime || undefined,
        endTime: endTime || undefined,
        offset: page * PAGE_SIZE,
        limit: PAGE_SIZE,
      });
      setEntries(res.entries);
      setTotal(res.total);
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setLoading(false);
    }
    // 概况描述的是整个日志目录（不受筛选影响），与列表一起刷新才能保证两处数字一致。
    // 单独用 total 做依赖会在「筛选后总数不变但文件已变」时漏刷新。
    try {
      setOverview(await api.proxyLogsOverview());
    } catch {
      // 概况只是附注信息，拿不到不该让整个面板报错
    }
  }, [keyword, startTime, endTime, page]);

  useEffect(() => {
    void load();
  }, [load]);

  const openDetail = async (id: string) => {
    setDetailLoading(true);
    try {
      const content = await api.proxyLogDetail(id);
      setDetail({ id, content });
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setDetailLoading(false);
    }
  };

  const doClear = async () => {
    // 清空是删文件且不可撤销，必须先确认。
    if (!window.confirm("确认清空全部抓包日志？此操作不可撤销。")) return;
    try {
      const res = await api.proxyLogsClear();
      toast.success(`已删除 ${res.removed} 个日志文件`);
      setPage(0);
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    }
  };

  const pages = Math.max(1, Math.ceil(total / PAGE_SIZE));
  const activeFilters = [startTime && "时间区间", endTime && "时间区间", keyword && "关键字"].filter(
    Boolean,
  ).length;

  return (
    <Card>
      <CardHeader className="pb-3">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div className="space-y-1">
            <CardTitle className="flex items-center gap-2 text-base">
              <FileText className="size-4" />
              抓包日志
            </CardTitle>
            <CardDescription>
              代理记录的每次请求与响应（凭证头与正文已脱敏）。
              {overview && overview.fileCount > 0
                ? `共 ${overview.fileCount} 个文件、${formatBytes(overview.totalBytes)}，覆盖 ${overview.oldest} ~ ${overview.newest}。`
                : "暂无日志文件。"}
            </CardDescription>
          </div>
          <div className="flex items-center gap-2">
            <Button variant="outline" size="sm" onClick={() => setFilterOpen((v) => !v)}>
              <Filter className="size-3.5" />
              筛选
              {activeFilters > 0 && (
                <Badge variant="secondary" className="ml-1">
                  {activeFilters}
                </Badge>
              )}
            </Button>
            <Button variant="outline" size="sm" disabled={loading} onClick={() => void load()}>
              {loading ? (
                <Loader2 className="size-3.5 animate-spin" />
              ) : (
                <RefreshCw className="size-3.5" />
              )}
              刷新
            </Button>
            <DemoAction>
              <Button variant="outline" size="sm" disabled={total === 0} onClick={() => void doClear()}>
                <Eraser className="size-3.5" />
                清空
              </Button>
            </DemoAction>
          </div>
        </div>
      </CardHeader>
      <CardContent className="space-y-3">
        <div className="flex flex-wrap items-end gap-2">
          <div className="min-w-56 flex-1 space-y-1.5">
            <Label htmlFor="proxy-log-keyword" className="text-xs">
              关键字
            </Label>
            <div className="flex gap-2">
              <Input
                id="proxy-log-keyword"
                value={keywordInput}
                placeholder="按 URL、主机名或响应内容搜索"
                onChange={(e) => setKeywordInput(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") {
                    setKeyword(keywordInput.trim());
                    setPage(0);
                  }
                }}
              />
              <Button
                size="sm"
                variant="secondary"
                onClick={() => {
                  setKeyword(keywordInput.trim());
                  setPage(0);
                }}
              >
                <Search className="size-3.5" />
                查询
              </Button>
            </div>
          </div>
        </div>

        {filterOpen && (
          <div className="grid gap-3 sm:grid-cols-2">
            <div className="space-y-1.5">
              <Label htmlFor="proxy-log-start" className="text-xs">
                起始时间（可选）
              </Label>
              <Input
                id="proxy-log-start"
                value={startTime}
                placeholder="2025-01-15 09:00:00"
                onChange={(e) => {
                  setStartTime(e.target.value);
                  setPage(0);
                }}
              />
            </div>
            <div className="space-y-1.5">
              <Label htmlFor="proxy-log-end" className="text-xs">
                结束时间（可选）
              </Label>
              <Input
                id="proxy-log-end"
                value={endTime}
                placeholder="2025-01-15 18:00:00"
                onChange={(e) => {
                  setEndTime(e.target.value);
                  setPage(0);
                }}
              />
            </div>
          </div>
        )}

        {loading ? (
          <div className="space-y-2">
            {Array.from({ length: 5 }).map((_, i) => (
              <Skeleton key={i} className="h-10 w-full" />
            ))}
          </div>
        ) : entries.length === 0 ? (
          <Alert>
            <FileText className="size-4" />
            <AlertTitle>没有匹配的抓包记录</AlertTitle>
            <AlertDescription>
              {total === 0 && !keyword && !startTime && !endTime
                ? "还没有抓包日志 —— 启动设备身份代理并访问一次客户端后即可看到记录。"
                : "当前筛选条件下没有匹配项，试试放宽时间区间或清空关键字。"}
            </AlertDescription>
          </Alert>
        ) : (
          <div className="overflow-hidden rounded-lg border">
            <div className="max-h-[28rem] overflow-y-auto">
              <table className="w-full text-sm">
                <thead className="sticky top-0 z-10 bg-muted/60 backdrop-blur">
                  <tr className="text-left text-xs text-muted-foreground">
                    <th className="px-3 py-2 font-medium">时间</th>
                    <th className="px-3 py-2 font-medium">请求</th>
                    <th className="px-3 py-2 font-medium">状态</th>
                    <th className="px-3 py-2 text-right font-medium">大小</th>
                  </tr>
                </thead>
                <tbody>
                  {entries.map((e) => (
                    <tr
                      key={e.id}
                      className="cursor-pointer border-t transition-colors hover:bg-foreground/[0.03]"
                      onClick={() => void openDetail(e.id)}
                    >
                      <td className="whitespace-nowrap px-3 py-2 font-mono text-xs text-muted-foreground">
                        {e.timestamp || "—"}
                      </td>
                      <td className="px-3 py-2">
                        <div className="flex items-center gap-2">
                          <Badge
                            variant={
                              e.method === "WebSocket"
                                ? "default"
                                : e.method === "HTTP GET"
                                  ? "secondary"
                                  : "outline"
                            }
                            className="shrink-0 font-mono text-[10px]"
                          >
                            {e.method}
                          </Badge>
                          <span className="truncate font-mono text-xs">{e.host}</span>
                        </div>
                        <div className="mt-0.5 truncate font-mono text-xs text-muted-foreground">
                          {e.path || "/"}
                        </div>
                        {(e.sseModel || e.sseTokens) && (
                          <div className="mt-1 flex flex-wrap items-center gap-1.5">
                            {e.sseModel && (
                              <Badge variant="secondary" className="text-[10px]">
                                {e.sseModel}
                              </Badge>
                            )}
                            {e.sseTokens && (
                              <span className="font-mono text-[10px] text-muted-foreground">
                                {e.sseTokens}
                              </span>
                            )}
                          </div>
                        )}
                      </td>
                      <td className={cn("whitespace-nowrap px-3 py-2 font-mono text-xs", statusTone(e.status))}>
                        {e.status}
                      </td>
                      <td className="whitespace-nowrap px-3 py-2 text-right text-xs text-muted-foreground">
                        {formatBytes(e.size)}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </div>
        )}

        {total > PAGE_SIZE && (
          <div className="flex items-center justify-between text-xs text-muted-foreground">
            <span>
              共 {total} 条，第 {page + 1} / {pages} 页
            </span>
            <div className="flex items-center gap-2">
              <Button
                variant="outline"
                size="sm"
                disabled={page === 0 || loading}
                onClick={() => setPage((p) => Math.max(0, p - 1))}
              >
                <ChevronLeft className="size-3.5" />
                上一页
              </Button>
              <Button
                variant="outline"
                size="sm"
                disabled={page + 1 >= pages || loading}
                onClick={() => setPage((p) => p + 1)}
              >
                下一页
                <ChevronRight className="size-3.5" />
              </Button>
            </div>
          </div>
        )}
      </CardContent>

      <Dialog
        open={detail !== null || detailLoading}
        onOpenChange={(open) => {
          if (!open) setDetail(null);
        }}
      >
        <DialogContent className="max-w-4xl">
          <DialogHeader>
            <DialogTitle>请求详情</DialogTitle>
            <DialogDescription className="font-mono text-xs">{detail?.id ?? "加载中…"}</DialogDescription>
          </DialogHeader>
          {detailLoading ? (
            <div className="flex items-center gap-2 py-8 text-sm text-muted-foreground">
              <Loader2 className="size-4 animate-spin" />
              正在读取…
            </div>
          ) : (
            <div className="max-h-[60vh] overflow-y-auto">
              <pre className="whitespace-pre-wrap break-all rounded-md bg-muted/50 p-3 font-mono text-xs">
                {detail?.content}
              </pre>
            </div>
          )}
        </DialogContent>
      </Dialog>
    </Card>
  );
}
