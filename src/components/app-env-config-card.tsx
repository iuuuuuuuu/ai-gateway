import { useCallback, useEffect, useState } from "react";
import { toast } from "sonner";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { SettingsGroup } from "@/components/settings-primitives";
import * as api from "@/lib/api";
import { cn } from "@/lib/utils";

// app-env-config-card.tsx —— 「应用环境」配置卡片（Trae / 豆包 等各平台页共用）。
//
// # 为什么从设置页搬到这里（所有者强约束）
//
// 所有者原话：
//
//	「设置页面的 trae 还有其他关于平台的配置都拆分到对应的 tab 下的配置下，
//	  这点也是强约束，保持统一」
//
// 即「平台专属配置跟着平台走」，与 platform-config-dialog.tsx 是同一套规矩：
// 用户在看某个平台的账号时，该平台的配置就在同一页的「配置」弹窗里，
// 不必再回设置页找。原先这张卡片写死了三个应用（Trae Work / Trae / 豆包），
// 搬到各平台页后每页只关心自己的应用，故由调用方用 `kinds` 传入。
//
// # 为什么需要手动指定路径
//
// 客户端安装位置五花八门（自定义盘符、绿色版），exe 发现链的 6 级回退
// 仍可能在部分机器上落空。此时让用户直接给出路径，比让他反复重装客户端
// 现实得多。

/** 卡片要展示的一个应用。 */
export interface AppEnvKind {
  /** 传给宿主 `appEnvCheck` / `appSetManualPath` 的应用标识，如 `TraeWork`。 */
  kind: string;
  /** 界面上显示的名字，如 `Trae Work`。 */
  label: string;
  /** 输入框 placeholder 里举例用的可执行文件名，如 `Trae CN.exe`。 */
  hint: string;
}

/**
 * 应用环境配置：逐个应用显示安装状态，并允许手动指定安装路径。
 *
 * `kinds` 请传**模块级常量**（引用稳定）—— 它进了 `load` 的依赖数组，
 * 每次渲染新建数组会让 `useEffect` 反复触发。
 *
 * `id` 对应 `SettingsGroup` 的 `aria-labelledby` 锚点，同一页面内必须唯一：
 * 账号管理页的 tab 是 keep-alive 的（`forceMount`），Trae 与豆包两张卡片
 * 可能同时挂在 DOM 里，故各页要传自己的 id。
 */
export function AppEnvConfigCard({
  kinds,
  title = "应用环境",
  description = "自动探测失败时可在此手动指定安装路径。",
  id = "settings-app-env",
}: {
  kinds: readonly AppEnvKind[];
  title?: string;
  description?: string;
  id?: string;
}) {
  const [envs, setEnvs] = useState<Record<string, api.AppEnvStatus | null>>({});
  const [paths, setPaths] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    const results = await Promise.all(
      kinds.map(async (app) => {
        try {
          return [app.kind, await api.appEnvCheck(app.kind)] as const;
        } catch {
          return [app.kind, null] as const;
        }
      }),
    );
    const nextEnvs: Record<string, api.AppEnvStatus | null> = {};
    const nextPaths: Record<string, string> = {};
    for (const [kind, status] of results) {
      nextEnvs[kind] = status;
      nextPaths[kind] = status?.manualPath ?? "";
    }
    setEnvs(nextEnvs);
    setPaths(nextPaths);
  }, [kinds]);

  useEffect(() => {
    void load();
  }, [load]);

  const savePath = async (kind: string) => {
    setBusy(true);
    try {
      await api.appSetManualPath(kind, paths[kind] ?? "");
      toast.success("已保存安装路径");
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <SettingsGroup id={id} title={title}>
      <CardContent className="space-y-0 p-0">
        <p className="px-4 pt-4 text-xs text-muted-foreground sm:px-5">{description}</p>
        {kinds.map((app, index) => {
          const env = envs[app.kind];
          return (
            <div
              key={app.kind}
              className={cn(
                "space-y-2 px-4 py-4 sm:px-5",
                index < kinds.length - 1 && "border-b border-border/60",
              )}
            >
              <div className="flex flex-wrap items-center gap-2">
                <span className="text-sm font-medium">{app.label}</span>
                {env ? (
                  <>
                    <Badge variant={env.installed ? "secondary" : "outline"}>
                      {env.installed ? "已安装" : "未检测到"}
                    </Badge>
                    <Badge variant={env.running ? "secondary" : "outline"}>
                      {env.running ? "运行中" : "未运行"}
                    </Badge>
                    <span className="text-xs text-muted-foreground">
                      快照 {env.snapshotCount} 个
                    </span>
                  </>
                ) : (
                  <Badge variant="outline">检测失败</Badge>
                )}
              </div>
              {env?.exePath && (
                <p className="break-all font-mono text-xs text-muted-foreground">{env.exePath}</p>
              )}
              <div className="flex gap-2">
                <Input
                  value={paths[app.kind] ?? ""}
                  onChange={(e) =>
                    setPaths((prev) => ({ ...prev, [app.kind]: e.target.value }))
                  }
                  placeholder={`手动指定路径，例如 D:\\Programs\\${app.hint}`}
                  className="font-mono text-xs"
                  aria-label={`${app.label} 安装路径`}
                />
                <Button
                  size="sm"
                  variant="outline"
                  disabled={busy}
                  onClick={() => void savePath(app.kind)}
                >
                  保存
                </Button>
              </div>
            </div>
          );
        })}
      </CardContent>
    </SettingsGroup>
  );
}
