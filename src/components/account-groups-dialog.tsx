import { useCallback, useEffect, useState } from "react";
import { Loader2, Pencil, Plus, Trash2, Users } from "lucide-react";
import { toast } from "sonner";

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
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import * as api from "@/lib/api";
import type { AccountGroup, GroupApp } from "@/lib/api";
import { cn } from "@/lib/utils";

/**
 * 可选的标记色。
 *
 * 用**语义名**而不是十六进制：具体色值由主题 token 决定，浅色/深色主题下
 * 各自取合适的明度。写死 `#3b82f6` 会在深色主题里显得刺眼，而且无法随主题调整。
 */
const GROUP_COLORS = [
  { value: "slate", label: "灰" },
  { value: "blue", label: "蓝" },
  { value: "green", label: "绿" },
  { value: "amber", label: "琥珀" },
  { value: "violet", label: "紫" },
  { value: "red", label: "红" },
] as const;

/** 色名 → 徽标 class（全部走主题 token，不硬编码色值）。 */
export function groupColorClass(color: string): string {
  switch (color) {
    case "blue":
      return "border-sky-500/40 bg-sky-500/10 text-sky-700 dark:text-sky-300";
    case "green":
      return "border-emerald-500/40 bg-emerald-500/10 text-emerald-700 dark:text-emerald-300";
    case "amber":
      return "border-amber-500/40 bg-amber-500/10 text-amber-700 dark:text-amber-300";
    case "violet":
      return "border-violet-500/40 bg-violet-500/10 text-violet-700 dark:text-violet-300";
    case "red":
      return "border-red-500/40 bg-red-500/10 text-red-700 dark:text-red-300";
    default:
      return "border-border bg-muted text-muted-foreground";
  }
}

/**
 * 分组管理弹窗（Trae / 豆包共用）。
 *
 * 两个应用的账号库彼此独立，因此分组也按 `app` 分域：同一个 uid 在两边
 * 毫无关系，共用一个分组表会让「Trae 的 A 组」把豆包同名 uid 也圈进去。
 */
export function AccountGroupsDialog({
  open,
  onOpenChange,
  app,
  onChanged,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  app: GroupApp;
  /** 分组变化后通知父组件重新拉账号列表（成员映射变了）。 */
  onChanged?: () => void;
}) {
  const [groups, setGroups] = useState<AccountGroup[]>([]);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [newName, setNewName] = useState("");
  const [newColor, setNewColor] = useState<string>("slate");
  const [editing, setEditing] = useState<AccountGroup | null>(null);
  const [editName, setEditName] = useState("");
  const [editColor, setEditColor] = useState<string>("slate");
  const [confirmDelete, setConfirmDelete] = useState<AccountGroup | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const res = await api.groupsList(app);
      setGroups(res.groups);
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setLoading(false);
    }
  }, [app]);

  useEffect(() => {
    if (open) void load();
  }, [open, load]);

  const create = async () => {
    const name = newName.trim();
    if (!name) {
      toast.error("请填写分组名");
      return;
    }
    setBusy(true);
    try {
      await api.groupCreate(app, name, newColor);
      setNewName("");
      toast.success(`已创建分组「${name}」`);
      await load();
      onChanged?.();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const saveEdit = async () => {
    if (!editing) return;
    const name = editName.trim();
    if (!name) {
      toast.error("分组名不能为空");
      return;
    }
    setBusy(true);
    try {
      await api.groupUpdate({ app, id: editing.id, name, color: editColor });
      setEditing(null);
      toast.success("分组已更新");
      await load();
      onChanged?.();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const remove = async (group: AccountGroup) => {
    setBusy(true);
    try {
      await api.groupDelete(app, group.id);
      setConfirmDelete(null);
      toast.success(`已删除分组「${group.name}」及其成员归属`);
      await load();
      onChanged?.();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <Dialog open={open} onOpenChange={onOpenChange}>
        <DialogContent className="max-w-lg">
          <DialogHeader>
            <DialogTitle className="flex items-center gap-2">
              <Users className="size-4" />
              {app === "Trae" ? "Trae" : "豆包"}账号分组
            </DialogTitle>
            <DialogDescription>
              分组只影响筛选与批量操作，不改变账号本身。删除分组不会删除账号。
            </DialogDescription>
          </DialogHeader>

          <div className="space-y-4">
            <div className="flex items-end gap-2">
              <div className="flex-1 space-y-1.5">
                <Label htmlFor="group-new-name">新建分组</Label>
                <Input
                  id="group-new-name"
                  value={newName}
                  placeholder="例如：主力账号"
                  maxLength={32}
                  onChange={(e) => setNewName(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") void create();
                  }}
                />
              </div>
              <Select value={newColor} onValueChange={setNewColor}>
                <SelectTrigger className="w-24" aria-label="分组颜色">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {GROUP_COLORS.map((c) => (
                    <SelectItem key={c.value} value={c.value}>
                      {c.label}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
              <Button disabled={busy || !newName.trim()} onClick={() => void create()}>
                <Plus className="size-4" />
                新建
              </Button>
            </div>

            <div className="space-y-2">
              {loading ? (
                <div className="flex items-center gap-2 py-6 text-sm text-muted-foreground">
                  <Loader2 className="size-4 animate-spin" />
                  正在读取分组…
                </div>
              ) : groups.length === 0 ? (
                <p className="py-6 text-center text-sm text-muted-foreground">
                  还没有分组。新建一个后即可在账号行里挑选。
                </p>
              ) : (
                groups.map((g) => (
                  <div
                    key={g.id}
                    className="flex items-center gap-3 rounded-lg border border-border/60 px-3 py-2"
                  >
                    {editing?.id === g.id ? (
                      <>
                        <Input
                          value={editName}
                          className="h-8 flex-1"
                          maxLength={32}
                          onChange={(e) => setEditName(e.target.value)}
                          onKeyDown={(e) => {
                            if (e.key === "Enter") void saveEdit();
                            if (e.key === "Escape") setEditing(null);
                          }}
                        />
                        <Select value={editColor} onValueChange={setEditColor}>
                          <SelectTrigger className="h-8 w-24" aria-label="分组颜色">
                            <SelectValue />
                          </SelectTrigger>
                          <SelectContent>
                            {GROUP_COLORS.map((c) => (
                              <SelectItem key={c.value} value={c.value}>
                                {c.label}
                              </SelectItem>
                            ))}
                          </SelectContent>
                        </Select>
                        <Button size="sm" disabled={busy} onClick={() => void saveEdit()}>
                          保存
                        </Button>
                        <Button size="sm" variant="ghost" onClick={() => setEditing(null)}>
                          取消
                        </Button>
                      </>
                    ) : (
                      <>
                        <Badge
                          variant="outline"
                          className={cn("shrink-0", groupColorClass(g.color))}
                        >
                          {g.name}
                        </Badge>
                        <span className="flex-1 text-xs text-muted-foreground">
                          {g.count} 个账号
                        </span>
                        <Button
                          size="sm"
                          variant="ghost"
                          aria-label={`重命名 ${g.name}`}
                          onClick={() => {
                            setEditing(g);
                            setEditName(g.name);
                            setEditColor(g.color);
                          }}
                        >
                          <Pencil className="size-3.5" />
                        </Button>
                        <Button
                          size="sm"
                          variant="ghost"
                          aria-label={`删除 ${g.name}`}
                          onClick={() => setConfirmDelete(g)}
                        >
                          <Trash2 className="size-3.5 text-destructive" />
                        </Button>
                      </>
                    )}
                  </div>
                ))
              )}
            </div>
          </div>

          <DialogFooter>
            <Button variant="outline" onClick={() => onOpenChange(false)}>
              关闭
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/*
        删除确认用 Dialog 而不是 window.confirm：
        confirm 是同步阻塞的原生弹窗，无法展示「会影响几个账号」这类上下文，
        样式也无法与主题一致。
      */}
      <Dialog open={confirmDelete !== null} onOpenChange={(o) => !o && setConfirmDelete(null)}>
        <DialogContent className="max-w-sm">
          <DialogHeader>
            <DialogTitle>删除分组「{confirmDelete?.name}」？</DialogTitle>
            <DialogDescription>
              {confirmDelete && confirmDelete.count > 0
                ? `该分组下的 ${confirmDelete.count} 个账号会被移出分组，账号本身与登录态不受影响。`
                : "该分组下没有账号，删除后无法恢复。"}
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setConfirmDelete(null)}>
              取消
            </Button>
            <Button
              variant="destructive"
              disabled={busy}
              onClick={() => confirmDelete && void remove(confirmDelete)}
            >
              删除分组
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}

/** 账号行里的分组选择器（未分组 = `none`）。 */
export function GroupSelect({
  groups,
  value,
  disabled,
  onChange,
}: {
  groups: AccountGroup[];
  value: string | null;
  disabled?: boolean;
  onChange: (groupId: string | null) => void;
}) {
  return (
    <Select
      value={value ?? "none"}
      disabled={disabled}
      onValueChange={(v) => onChange(v === "none" ? null : v)}
    >
      <SelectTrigger className="h-8 w-32 text-xs" aria-label="所属分组">
        <SelectValue placeholder="未分组" />
      </SelectTrigger>
      <SelectContent>
        <SelectItem value="none">未分组</SelectItem>
        {groups.map((g) => (
          <SelectItem key={g.id} value={g.id}>
            {g.name}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}
