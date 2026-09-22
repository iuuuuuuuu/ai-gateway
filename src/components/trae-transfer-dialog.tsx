import { useRef, useState } from "react";
import { AlertTriangle, Download, Loader2, Upload } from "lucide-react";
import { toast } from "sonner";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Separator } from "@/components/ui/separator";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import * as api from "@/lib/api";
import type { TraeAccountMeta, TraeImportPreviewItem } from "@/lib/api";

/**
 * Trae 账号导出 / 导入。
 *
 * 导出文件里是**明文 JWT 与 refresh_token** —— 拿到它就等于拿到账号，
 * 因此这里在下载前把这件事明确写出来，而不是静默生成一个文件。
 */
export function TraeTransferDialog({
  open,
  onOpenChange,
  accounts,
  onImported,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  accounts: TraeAccountMeta[];
  onImported: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const [selected, setSelected] = useState<string[]>([]);
  const [preview, setPreview] = useState<{
    count: number;
    exportedAt: string | null;
    accounts: TraeImportPreviewItem[];
  } | null>(null);
  const [fileText, setFileText] = useState("");
  const fileInputRef = useRef<HTMLInputElement>(null);

  const toggle = (uid: string) => {
    setSelected((prev) =>
      prev.includes(uid) ? prev.filter((x) => x !== uid) : [...prev, uid],
    );
  };

  const doExport = async () => {
    setBusy(true);
    try {
      // 不选任何账号 = 导出全部（与后端语义一致）
      const res = await api.traeExportAccounts(selected.length > 0 ? selected : undefined);
      const blob = new Blob([res.text], { type: "application/json" });
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = `trae-accounts-${new Date().toISOString().slice(0, 10)}.json`;
      a.click();
      // 立刻回收：blob URL 留到页面卸载才释放会白占内存
      URL.revokeObjectURL(url);
      toast.success(`已导出 ${res.count} 个账号（文件含明文凭证，请妥善保管）`, {
        duration: 8000,
      });
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const pickFile = async (file: File) => {
    setBusy(true);
    setPreview(null);
    setFileText("");
    try {
      const text = await file.text();
      const res = await api.traePreviewImport(text);
      setFileText(text);
      setPreview({ count: res.count, exportedAt: res.exportedAt, accounts: res.accounts });
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
      // 清空 input：同一文件选第二次也要能触发 change
      if (fileInputRef.current) fileInputRef.current.value = "";
    }
  };

  const doImport = async () => {
    if (!fileText) return;
    setBusy(true);
    try {
      const res = await api.traeImportAccounts(fileText);
      const parts = [`新增 ${res.added}`];
      if (res.updated) parts.push(`覆盖 ${res.updated}`);
      if (res.skipped) parts.push(`跳过 ${res.skipped}`);
      if (res.skipped > 0) toast.warning(`导入完成：${parts.join(" · ")}`);
      else toast.success(`导入完成：${parts.join(" · ")}`);
      setPreview(null);
      setFileText("");
      onImported();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const overwriteCount = preview?.accounts.filter((a) => a.willOverwrite).length ?? 0;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-xl">
        <DialogHeader>
          <DialogTitle>导出 / 导入 Trae 账号</DialogTitle>
          <DialogDescription>
            用于在多台机器之间迁移账号库。导出文件包含完整登录凭证。
          </DialogDescription>
        </DialogHeader>

        <Tabs defaultValue="export">
          <TabsList>
            <TabsTrigger value="export">导出</TabsTrigger>
            <TabsTrigger value="import">导入</TabsTrigger>
          </TabsList>

          <TabsContent value="export" className="space-y-3 pt-3">
            <Alert>
              <AlertTriangle className="size-4" />
              <AlertTitle>导出文件含明文凭证</AlertTitle>
              <AlertDescription>
                文件里的 JWT 与 refresh token 可直接登录对应账号。请勿通过聊天工具、
                网盘等渠道传播；用完后及时删除。
              </AlertDescription>
            </Alert>

            <div className="space-y-2">
              <div className="flex items-center justify-between text-xs">
                <span className="text-muted-foreground">
                  选择要导出的账号（不选则导出全部 {accounts.length} 个）
                </span>
                {selected.length > 0 && (
                  <Button
                    size="sm"
                    variant="link"
                    className="h-auto p-0 text-xs"
                    onClick={() => setSelected([])}
                  >
                    清空选择
                  </Button>
                )}
              </div>
              <div className="max-h-56 space-y-1 overflow-y-auto rounded-lg border border-border/60 p-2">
                {accounts.map((a) => (
                  <label
                    key={a.userId}
                    className="flex cursor-pointer items-center gap-2 rounded px-2 py-1.5 text-xs hover:bg-accent"
                  >
                    <input
                      type="checkbox"
                      checked={selected.includes(a.userId)}
                      onChange={() => toggle(a.userId)}
                      className="size-3.5 accent-primary"
                    />
                    <span className="truncate font-medium">{a.name}</span>
                    <span className="truncate font-mono text-muted-foreground">{a.userId}</span>
                    {!a.hasRefreshToken && (
                      <Badge variant="outline" className="ml-auto shrink-0 text-[10px]">
                        无续期凭证
                      </Badge>
                    )}
                  </label>
                ))}
                {accounts.length === 0 && (
                  <p className="px-2 py-4 text-center text-xs text-muted-foreground">
                    账号库为空，没有可导出的账号。
                  </p>
                )}
              </div>
            </div>

            <DialogFooter>
              <Button disabled={busy || accounts.length === 0} onClick={() => void doExport()}>
                {busy ? <Loader2 className="size-4 animate-spin" /> : <Download className="size-4" />}
                导出{selected.length > 0 ? `选中的 ${selected.length} 个` : "全部"}
              </Button>
            </DialogFooter>
          </TabsContent>

          <TabsContent value="import" className="space-y-3 pt-3">
            <input
              ref={fileInputRef}
              type="file"
              accept="application/json,.json"
              className="hidden"
              onChange={(e) => {
                const file = e.target.files?.[0];
                if (file) void pickFile(file);
              }}
            />
            <Button
              variant="outline"
              className="w-full"
              disabled={busy}
              onClick={() => fileInputRef.current?.click()}
            >
              {busy ? <Loader2 className="size-4 animate-spin" /> : <Upload className="size-4" />}
              选择导出文件
            </Button>

            {preview && (
              <div className="space-y-2">
                <div className="flex flex-wrap items-center gap-2 text-xs">
                  <span>共 {preview.count} 个账号</span>
                  {preview.exportedAt && (
                    <span className="text-muted-foreground">导出于 {preview.exportedAt}</span>
                  )}
                  {overwriteCount > 0 && (
                    <Badge variant="destructive" className="text-[10px]">
                      {overwriteCount} 个会被覆盖
                    </Badge>
                  )}
                </div>
                <Separator />
                <div className="max-h-56 space-y-1 overflow-y-auto">
                  {preview.accounts.map((a) => (
                    <div
                      key={a.userId}
                      className="flex flex-wrap items-center gap-2 rounded-md border border-border/50 px-2 py-1.5 text-xs"
                    >
                      <span className="font-medium">{a.name || "（无备注名）"}</span>
                      <span className="font-mono text-muted-foreground">{a.userId}</span>
                      {a.hasRefreshToken ? (
                        <Badge variant="outline" className="text-[10px]">
                          含续期凭证
                        </Badge>
                      ) : (
                        <Badge variant="outline" className="text-[10px] text-amber-600 dark:text-amber-400">
                          无续期凭证
                        </Badge>
                      )}
                      {a.willOverwrite && (
                        <Badge variant="destructive" className="ml-auto text-[10px]">
                          将覆盖现有账号
                        </Badge>
                      )}
                    </div>
                  ))}
                </div>
                {overwriteCount > 0 && (
                  <Alert variant="destructive">
                    <AlertTriangle className="size-4" />
                    <AlertTitle>覆盖不可撤销</AlertTitle>
                    <AlertDescription>
                      上述账号的现有登录凭证会被文件里的内容替换。如果文件来自较旧的备份，
                      点击导入会让这些账号退回旧登录态。
                    </AlertDescription>
                  </Alert>
                )}
              </div>
            )}

            <DialogFooter>
              <Button variant="outline" onClick={() => onOpenChange(false)}>
                取消
              </Button>
              <Button disabled={busy || !fileText} onClick={() => void doImport()}>
                {busy && <Loader2 className="size-4 animate-spin" />}
                确认导入
              </Button>
            </DialogFooter>
          </TabsContent>
        </Tabs>
      </DialogContent>
    </Dialog>
  );
}
