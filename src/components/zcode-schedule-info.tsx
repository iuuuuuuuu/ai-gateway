import { CalendarClock, Info } from "lucide-react";

import { Alert, AlertDescription } from "@/components/ui/alert";
import { CardContent } from "@/components/ui/card";
import { SettingsFieldRow, SettingsGroup } from "@/components/settings-primitives";

// zcode-schedule-info.tsx —— 把「自动任务什么时候跑」直接写在界面上。
//
// # 为什么需要它（2026-09-22 所有者要求）
//
// 所有者原话：
//
//	「zcode那个自动执行的应该在zcode页面加一个提示,在xxx会自动执行xxx任务」
//	「zcode的那个自动领取的是什么时间跑的?」
//
// 背景：ZCode 的套餐自动领取**在后台跑**，界面上没有任何痕迹 ——
// 用户既不知道它会不会跑、也不知道什么时候跑。结果是：
//
//   · 想确认"到底自动领了没"时只能干等
//   · 以为功能没生效（其实只是还没到时间）
//
// 这里把**真实调度**列出来，值不写死在前端，而是写明来源，
// 免得代码改了而这段文案漂移成假话。
//
// # 数据来源（都是代码里的实情，不是猜的）
//
//   · 套餐自动领取：`go-gateway/cmd/server/main.go` 的
//     `newClaimScheduler` —— **启动即跑一次**，之后每 **5 分钟**轮询，
//     失败有冷却（见该处日志文案「启动即跑，之后每 5m0s 轮询」）
//   · Qoder / ZCode 权益活动：`multi_product_credit_patrol::run_once`
//     —— 每 **20 分钟**一轮，随额度巡检一起（见 `PATROL_INTERVAL`）
//   · 活动重置：**每天 10:00（UTC+8）**（来自上游活动描述，实测）
//
// ⚠ 改这里之前先核对上面三处源码 —— 文案与行为不一致比没有文案更糟：
// 用户会照着它判断"为什么还没领到"。
export function ZcodeAutoClaimScheduleCard() {
  return (
    <SettingsGroup id="settings-zcode-schedule" title="自动执行时机">
      <CardContent className="space-y-0 p-0">
        <SettingsFieldRow
          label="套餐权益自动领取"
          description={
            <>
              网关<strong className="font-medium text-foreground">启动时立刻跑一次</strong>，
              之后每 <strong className="font-medium text-foreground">5 分钟</strong>检查一轮；
              失败会进入冷却，冷却结束自动重试。
            </>
          }
        >
          <span className="inline-flex items-center gap-1.5 text-xs text-muted-foreground">
            <CalendarClock className="size-3.5" />
            约 5 分钟一轮
          </span>
        </SettingsFieldRow>

        <SettingsFieldRow
          label="权益活动（每日额度）"
          description={
            <>
              活动<strong className="font-medium text-foreground">每天 10:00（UTC+8）重置</strong>，
              单条时限约 22 小时。网关每 <strong className="font-medium text-foreground">20 分钟</strong>
              检查一轮并自动领取 —— 不领就会随活动到期作废。
            </>
          }
        >
          <span className="inline-flex items-center gap-1.5 text-xs text-muted-foreground">
            <CalendarClock className="size-3.5" />
            约 20 分钟一轮
          </span>
        </SettingsFieldRow>

        <div className="px-4 pb-3 pt-1 sm:px-5">
          <Alert>
            <Info />
            <AlertDescription>
              自动任务**在后台运行**，无需保持本页面打开；但要<strong className="font-medium">网关处于运行状态</strong>。
              想立刻验证而不等下一轮，可在上面的「立即领取」里手动跑一次。
            </AlertDescription>
          </Alert>
        </div>
      </CardContent>
    </SettingsGroup>
  );
}
