import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { QoderMark } from "@/components/product-marks";

/**
 * Qoder 账号页。
 *
 * **当前是 C1 阶段的占位页**：命名统一先落地（侧边栏与路由可用），
 * 真实功能在 C3（后端）与 C4（本页）实现。
 *
 * 为什么先放占位页而不是隐藏入口：
 *   1. 侧边栏的四项命名要一起看才判断得出是否统一 —— 缺一项就看不准；
 *   2. 所有者能立刻看到命名效果并给反馈，而不必等后端做完；
 *   3. 路由提前占位，避免 C4 时再动 App.tsx（少一次改动 = 少一次冲突面）。
 *
 * 页面上**如实写明**功能尚未接入，不留"点了没反应"的死入口 ——
 * 那会让用户以为是故障而不是未实现（本仓库反复强调的错误方向）。
 */
export default function QoderPage() {
  return (
    <div className="w-full space-y-6 px-5 py-6 sm:px-8 sm:py-8">
      <header className="mb-6">
        <div className="flex items-start gap-3">
          <QoderMark size={34} className="mt-0.5" />
          <div className="min-w-0">
            <h1 className="text-[28px] font-semibold tracking-tight">Qoder 账号</h1>
            <p className="mt-2 text-sm leading-6 text-muted-foreground">
              管理 Qoder 账号的登录态、额度与到期时间，并与 WorkBuddy 账号一起参与网关路由。
            </p>
          </div>
        </div>
      </header>

      <Alert>
        <AlertTitle>功能开发中</AlertTitle>
        <AlertDescription>
          账号页与后端尚未接入，本页目前只是一个入口占位。接入后会支持两种登录方式：
          软件内浏览器授权，以及导入已有的 Qoder 凭证文件。国服与国际版都会支持。
        </AlertDescription>
      </Alert>
    </div>
  );
}
