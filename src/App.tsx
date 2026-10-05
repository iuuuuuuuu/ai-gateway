import { useEffect, useState } from "react";
import { BrowserRouter, HashRouter, Navigate, NavLink, Outlet, Route, Routes, useLocation } from "react-router-dom";
import { ArrowUp, Bot, Gauge, Server, Settings, Users } from "lucide-react";

import { cn } from "@/lib/utils";
import * as api from "@/lib/api";
import type { UpdateInfo } from "@/lib/types";
// ★ 五个「XX账号」页（WorkBuddy / Qoder / ZCode / Trae / 豆包）不再各自挂路由：
// 它们现在都是「账号管理」页（AccountHubPage）里的 tab，由那一个页面统一 import。
import AccountHubPage from "@/pages/AccountHubPage";
import GatewayPage from "@/pages/GatewayPage";
import AgentsPage from "@/pages/AgentsPage";
import SettingsPage from "@/pages/SettingsPage";
import CliQuotaPage from "@/pages/CliQuotaPage";
import { StatusDot, AppIconMark } from "@/components/product-marks";
import { UpdateInstallDialog } from "@/components/update-install-dialog";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
// ★ 顶层错误边界：任何页面渲染期异常都不该让整页白屏（2026-10-01 事故）。
import { ErrorBoundary } from "@/components/error-boundary";
import { Toaster } from "@/components/ui/sonner";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import { demoModeEnabled, pagesDemoHostingEnabled } from "@/lib/demo-mode";
import { useCreditAutoRefresh } from "@/lib/use-credit-auto-refresh";
import { useVisibilityInterval } from "@/lib/use-visibility-interval";
import { useWorkbuddyStatusRefresh } from "@/lib/use-workbuddy-status-refresh";
import { useAccountsStore } from "@/stores/accounts";

function UpdateCenter({ running }: { running: boolean | undefined }) {
  const version = useAccountsStore((s) => s.status?.version);
  const [info, setInfo] = useState<UpdateInfo | null>(null);
  const [dialogOpen, setDialogOpen] = useState(false);

  // 首次进入立刻检查一次。周期检查交给下面的 useVisibilityInterval ——
  // UpdateCenter 挂在根组件上、**永不卸载**，用裸 setInterval 的话定时器
  // 会一直存在：窗口收进托盘 / 隐藏时仍在后台每 30 分钟打一次网络请求。
  const [checkNonce, setCheckNonce] = useState(0);
  useEffect(() => {
    let disposed = false;
    api
      .checkUpdate()
      .then((result) => {
        if (!disposed) setInfo(result.ok ? result : null);
      })
      .catch(() => {
        // 左下角只展示可操作的升级状态，网络错误不打扰正常使用。
      });
    return () => {
      disposed = true;
    };
  }, [checkNonce]);

  // immediate: false —— 首次检查已由上面的 effect 负责（它挂载即跑），
  // 这里只做周期轮询，否则启动时会重复打一次 checkUpdate。
  useVisibilityInterval(() => setCheckNonce((n) => n + 1), 30 * 60 * 1000, {
    immediate: false,
  });

  const hasUpdate = Boolean(info?.ok && info.hasUpdate && info.latest);

  return (
    <>
      <section className="mt-auto border-t border-sidebar-border px-2 pt-3 text-xs">
        <div className="flex items-center gap-2 text-[13px] text-sidebar-foreground">
          <StatusDot on={Boolean(running)} />
          <span className="min-w-0 flex-1 truncate">WorkBuddy</span>
          <div className="flex shrink-0 items-center gap-1.5">
            <span className="text-sidebar-foreground/50">v{version || "?"}</span>
            {hasUpdate && (
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button
                    type="button"
                    size="icon"
                    className="size-5 rounded-full p-0"
                    aria-label="更新"
                    onClick={() => setDialogOpen(true)}
                  >
                    <ArrowUp className="size-3" strokeWidth={2.5} aria-hidden="true" />
                  </Button>
                </TooltipTrigger>
                <TooltipContent side="top">更新</TooltipContent>
              </Tooltip>
            )}
          </div>
        </div>
      </section>
      <UpdateInstallDialog
        open={dialogOpen}
        onOpenChange={setDialogOpen}
        update={info}
      />
    </>
  );
}

function Layout() {
  // 用于给错误边界做 key：换页时重置，避免上一个页面的错误粘住新页面。
  const location = useLocation();
  const running = useAccountsStore((s) => s.status?.running);
  const hasUnifiedTitleBar =
    api.isDesktop() && typeof navigator !== "undefined" && navigator.userAgent.includes("Macintosh");
  useCreditAutoRefresh();
  useWorkbuddyStatusRefresh();

  return (
    <div className="flex h-screen min-h-0 overflow-hidden bg-background">
      {hasUnifiedTitleBar ? (
        <div
          data-tauri-drag-region
          className="fixed inset-x-0 top-0 z-50 h-8"
          aria-hidden="true"
        />
      ) : null}
      <aside
        className={cn(
          "flex min-h-0 w-[220px] shrink-0 flex-col border-r border-sidebar-border bg-sidebar px-3 pb-4",
          hasUnifiedTitleBar ? "pt-20" : "pt-4",
        )}
      >
        <div className="flex items-center gap-2.5 px-1 pb-5">
          <AppIconMark size={36} className="drop-shadow-sm" />
          <div className="min-w-0">
            <div
              className="truncate text-[15px] leading-5 tracking-[-0.02em] text-sidebar-foreground/90"
              style={{
                fontFamily: '"Bricolage Grotesque Variable", "SF Pro Display", ui-sans-serif, sans-serif',
                fontWeight: 640,
              }}
            >
              AI Gateway
            </div>
            {demoModeEnabled && (
              <Badge variant="secondary" className="mt-1 h-5 border-0 px-1.5 text-[10px] text-sidebar-foreground/60 shadow-none">
                演示模式
              </Badge>
            )}
          </div>
        </div>
        <nav className="flex min-h-0 flex-1 flex-col gap-0.5" aria-label="主导航">
          {/* 「账号管理」：五个账号页（WorkBuddy / Qoder / ZCode / Trae / 豆包）合并后的唯一入口。
              为什么要合并：五个入口并排时，用户要来回点才能看清「到底有哪些号在接流量」，
              而它们本来就是同一类页面（登录一个上游账号）。现在一个页面里用 tab 切换，
              平台开关与各平台的账号状态在同一屏里就能对照。
              旧路径（/、/qoder、/zcode、/trae、/doubao）保留为重定向，见下面的路由表 ——
              老书签、老文档、别人贴过的链接都不会失效。 */}
          <NavLink
            to="/accounts"
            className={({ isActive }) =>
              cn(
                "flex items-center gap-2.5 rounded-lg px-3 py-2.5 text-sm outline-none transition-colors focus-visible:ring-2 focus-visible:ring-sidebar-ring/50",
                isActive
                  ? "bg-foreground/[0.06] font-medium text-foreground"
                  : "text-muted-foreground hover:bg-foreground/[0.04] hover:text-foreground",
              )
            }
          >
            <Users className="size-4" />
            账号管理
          </NavLink>
          {/* 「Token 统计」「积分统计」两条侧栏入口已删除：它们现在是「账号管理」
              WorkBuddy tab 里的二级 tab（业主 m02672），旧路径保留为重定向，见路由表。 */}
          {/* 额度查询：本机 AI CLI（Codex / Claude / Antigravity / Grok / Kimi）
              的登录额度，与 WorkBuddy 账号体系无关 —— 它读的是本机各 CLI
              自己的凭证，故放在「积分统计」之后、与网关功能并列。 */}
          <NavLink
            to="/cli-quota"
            className={({ isActive }) =>
              cn(
                "flex items-center gap-2.5 rounded-lg px-3 py-2.5 text-sm outline-none transition-colors focus-visible:ring-2 focus-visible:ring-sidebar-ring/50",
                isActive
                  ? "bg-foreground/[0.06] font-medium text-foreground"
                  : "text-muted-foreground hover:bg-foreground/[0.04] hover:text-foreground",
              )
            }
          >
            <Gauge className="size-4" />
            额度查询
          </NavLink>
          <NavLink
            to="/gateway"
            className={({ isActive }) =>
              cn(
                "flex items-center gap-2.5 rounded-lg px-3 py-2.5 text-sm outline-none transition-colors focus-visible:ring-2 focus-visible:ring-sidebar-ring/50",
                isActive
                  ? "bg-foreground/[0.06] font-medium text-foreground"
                  : "text-muted-foreground hover:bg-foreground/[0.04] hover:text-foreground",
              )
            }
          >
            <Server className="size-4" />
            兼容网关
          </NavLink>
          <NavLink
            to="/agents"
            className={({ isActive }) =>
              cn(
                "flex items-center gap-2.5 rounded-lg px-3 py-2.5 text-sm outline-none transition-colors focus-visible:ring-2 focus-visible:ring-sidebar-ring/50",
                isActive
                  ? "bg-foreground/[0.06] font-medium text-foreground"
                  : "text-muted-foreground hover:bg-foreground/[0.04] hover:text-foreground",
              )
            }
          >
            <Bot className="size-4" />
            智能体管理
          </NavLink>
          <NavLink
            to="/settings"
            className={({ isActive }) =>
              cn(
                "flex items-center gap-2.5 rounded-lg px-3 py-2.5 text-sm outline-none transition-colors focus-visible:ring-2 focus-visible:ring-sidebar-ring/50",
                isActive
                  ? "bg-foreground/[0.06] font-medium text-foreground"
                  : "text-muted-foreground hover:bg-foreground/[0.04] hover:text-foreground",
              )
            }
          >
            <Settings className="size-4" />
            设置
          </NavLink>
        </nav>
        {api.isWebui() && !demoModeEnabled ? null : <UpdateCenter running={running} />}
      </aside>
      <main
        className={cn(
          "min-w-0 flex-1 overflow-y-auto bg-background overscroll-contain",
          hasUnifiedTitleBar && "pt-16 [&>div]:pt-4",
        )}
      >
        {/*
          ★★ 错误边界包在 <Outlet /> **外侧、main 内侧** —— 位置是刻意的：
            · 包在 main 外 ⇒ 出错时连左侧导航一起消失（2026-10-01 的真实白屏
              就是这样：点「兼容网关」后整页空白，用户连切页自救都做不到）；
            · 包在这里 ⇒ 只有**内容区**被替换成错误页，导航仍在，用户能切走。
          另传 `resetKey`：切换路由时重置边界，避免上一个页面的错误状态粘住新页面
          （React 的错误边界状态不会因 children 变化自动清除）。
          ★★ 注意是 **prop** 不是 `key`：写成 `key={location.pathname}` 会让 React 在每次
          路由变化时卸载重建整个内容子树 —— 「账号管理」的五个平台标签各是一个路由，
          于是 keep-alive（`visited` + `forceMount`）被反复清零，业主报的
          「页面切换保持状态缓存」实测失效。改用 prop 后只有**出错时**才复位。
        */}
        <ErrorBoundary resetKey={location.pathname}>
          <Outlet />
        </ErrorBoundary>
      </main>
    </div>
  );
}

export default function App() {
  const Router = pagesDemoHostingEnabled ? HashRouter : BrowserRouter;

  return (
    <TooltipProvider delayDuration={250}>
      <Router>
        <Routes>
          <Route element={<Layout />}>
            {/* 「账号管理」：五个平台共用一个页面，平台由 URL 的 tab 段决定。
                默认 tab = workbuddy（/accounts 等价于 /accounts/workbuddy）。
                ★ tab 由 URL 派生而不是组件内的 state：这样刷新、前进/后退、
                  把链接发给别人，看到的都是同一个 tab。 */}
            <Route path="/accounts" element={<AccountHubPage />} />
            <Route path="/accounts/:tab" element={<AccountHubPage />} />
            <Route path="/cli-quota" element={<CliQuotaPage />} />
            <Route path="/gateway" element={<GatewayPage />} />
            <Route path="/agents" element={<AgentsPage />} />
            <Route path="/settings" element={<SettingsPage />} />
            {/* 旧路径全部重定向到对应的 tab —— 用 replace 而不是 push，
                否则用户的「后退」会退回旧地址、再被弹回，来回打转退不出去。 */}
            <Route path="/" element={<Navigate to="/accounts/workbuddy" replace />} />
            <Route path="/qoder" element={<Navigate to="/accounts/qoder" replace />} />
            <Route path="/zcode" element={<Navigate to="/accounts/zcode" replace />} />
            <Route path="/trae" element={<Navigate to="/accounts/trae" replace />} />
            <Route path="/doubao" element={<Navigate to="/accounts/doubao" replace />} />
            {/* 统计页并进 WorkBuddy tab 后，旧路径同样重定向（不带二级 tab 段：
                WorkBuddy tab 内部默认就停在「账号」，用户可在页内切到统计）。 */}
            <Route path="/credit-stats" element={<Navigate to="/accounts/workbuddy" replace />} />
            <Route path="/token-stats" element={<Navigate to="/accounts/workbuddy" replace />} />
            <Route path="*" element={<Navigate to="/accounts/workbuddy" replace />} />
          </Route>
        </Routes>
        <Toaster />
      </Router>
    </TooltipProvider>
  );
}
