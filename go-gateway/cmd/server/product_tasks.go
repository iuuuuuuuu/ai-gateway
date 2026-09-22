package main

// product_tasks.go 把 ZCode 套餐自动领取**接进网关的启动流程**。
//
// # 所有者要求（2026-09-20）
//
//	「他那个仓库还有个自动领取那个积分包的功能，我们也要接进来」
//	「zcode要按照实际逻辑去做啊，他那个仓库怎么做我们就怎么做」
//
// 第二条纠正了我第一版的做法：我当时按自己的判断把它"保守化"成
// "每天一次"，而参考实现（TriDefender/zcode-api `src/claim/scheduler.ts`）
// 的实际逻辑是 **启动即跑 + 5 分钟轮询 + 动态 hold/cooldown 退避**。
//
// # 实际逻辑实现在哪
//
// `internal/zcode/claim_scheduler.go` —— 那里逐条照搬了参考实现的
// 决策表（hold 硬闸 / cooldown / hold 到套餐截止 / login_required 永久停止）。
// 本文件只做两件事：
//
//	① 提供"手动跑一次"的入口（界面上的「立即领取」仍然可用）
//	② 在 main.go 里启动那个常驻调度器
//
// # 为什么调度器放在 internal/zcode 而不是这里
//
// 业务码分类（1001~1005 / 3001 / 3007 / 401）、hold 状态机、
// 动态间隔计算都是 **ZCode 协议层**的知识，与"网关怎么启动"无关。
// 放这里会让 cmd/server 变成一个巨大的杂糅层。
import (
	"context"
	"fmt"
	"strings"
	"time"

	"workbuddy2api/internal/qoder"
	"workbuddy2api/internal/zcode"
)

// productTaskTimeout 手动触发一次的整轮超时。
//
// 只给**手动**路径用：用户点了「立即领取」在等结果，必须有界。
// 常驻调度器不用这个超时 —— 它按自己的 hold/cooldown 节奏走。
const productTaskTimeout = 3 * time.Minute

// newProductTasksRunner 构造**手动触发**执行体（界面上的「立即领取」）。
//
// 返回 nil 表示"当前构建两个产品都没有"（此时排程与手动入口都跳过，不报错）。
//
// ⚠ 手动路径与自动路径**共用同一套领取逻辑**（`zcode.ClaimScheduler` /
// `qoder.ClaimAllCampaigns`），只是绕过 hold 闸门并加一个总超时 ——
// 两条路径各写一份领取实现必然出现"手动能领、自动领不到"这类分歧。
//
// # ⚠ 2026-09-22 修正：此前只跑 ZCode，**漏了 Qoder**
//
// 所有者要求「qoder改为 早十点,晚九点 两次触发,防止错漏」。
// 而那个时点触发走的就是本执行体，它当时只调 `runZcodeClaimsNow` ——
// 结果是**Qoder 的活动在排程路径上根本不会被领**（手动按钮能领，
// 因为那条路走宿主侧的 `claim_all_campaigns`）。
//
// 现在两个产品都跑：先 Qoder 再 ZCode，**串行**且各自独立计结果 ——
// 一个产品失败不影响另一个（Qoder 挂了不该让 ZCode 也停）。
func newProductTasksRunner(
	zd *zcode.Dispatch,
	sched *zcode.ClaimScheduler,
	qoderAuthDir string,
	qoderEnabled bool,
) func() (string, error) {
	if zd == nil && !qoderEnabled {
		return nil
	}
	return func() (string, error) {
		ctx, cancel := context.WithTimeout(context.Background(), productTaskTimeout)
		defer cancel()

		var parts []string
		// ---- Qoder（先跑：它的活动每天 10:00 重置，越早领越不易过期）----
		if qoderEnabled && strings.TrimSpace(qoderAuthDir) != "" {
			if out, err := qoder.ClaimAllCampaigns(ctx, qoderAuthDir); err != nil {
				// ⚠ 不 return：Qoder 失败不该阻断 ZCode（两个产品互相独立）
				parts = append(parts, fmt.Sprintf("Qoder 领取失败：%v", err))
			} else {
				parts = append(parts, fmt.Sprintf(
					"Qoder 领取 %v 项（失败 %v）",
					out["claimedCount"], out["failedCount"],
				))
			}
		}
		// ---- ZCode ----
		if zd != nil {
			detail, err := runZcodeClaimsNow(ctx, zd, sched)
			if err != nil {
				parts = append(parts, fmt.Sprintf("ZCode 领取失败：%v", err))
			} else if detail != "" {
				parts = append(parts, detail)
			}
		}
		return strings.Join(parts, "；"), nil
	}
}

// runZcodeClaimsNow 立刻跑一轮领取（**绕过 hold 闸门**，供手动触发用）。
//
// # 为什么手动要绕过 hold
//
// hold 的语义是"这个账号刚领过/已领完，别白探了"。而用户点「立即领取」
// 的意思是"我现在就要检查一遍" —— 用 hold 拦住他会让他以为按钮坏了。
// 参考实现的 CLI `claim now` 同样是"立刻跑一次"。
//
// ⚠ 但**不绕过幂等**：已领过的仍会回 `already_claimed`，那不会被重复领取。
func runZcodeClaimsNow(ctx context.Context, zc *zcode.Dispatch, sched *zcode.ClaimScheduler) (string, error) {
	creds, failed, err := zc.LoadCreds()
	if err != nil {
		return "", fmt.Errorf("加载 ZCode 凭证失败: %w", err)
	}
	if len(creds) == 0 {
		msg := "ZCode：没有可用凭证，跳过"
		if len(failed) > 0 {
			msg = fmt.Sprintf("ZCode：没有可用凭证（%d 个解析失败），跳过", len(failed))
		}
		return msg, nil
	}

	var claimed, already, idle, errCount int
	var firstErr error

	// ⚠ **严格串行**（不并发）：参考实现同样严格串行
	//（`setTimeout`-in-`finally` 链，绝不重叠）。
	// 并发打同一上游会让"一个账号被多台设备同时操作"，那是最容易被标记的画像。
	for _, cr := range creds {
		if ctx.Err() != nil {
			return summarizeZcode(claimed, already, idle, errCount, len(creds)),
				fmt.Errorf("ZCode 领取超时（已处理部分账号）")
		}

		res := zc.ClaimOnce(ctx, cr, sched)
		switch res.Outcome {
		case zcode.ClaimOutcomeClaimed:
			claimed++
		case zcode.ClaimOutcomeIdle:
			// idle 里既有"没有可领"也有"已领过"，用 Kind 区分。
			if res.Kind == zcode.ClaimFailAlreadyClaimed {
				already++
			} else {
				idle++
			}
		case zcode.ClaimOutcomeFailed, zcode.ClaimOutcomeError:
			errCount++
			if firstErr == nil && res.Msg != "" {
				firstErr = fmt.Errorf("%s", res.Msg)
			}
		default:
			idle++
		}
	}

	detail := summarizeZcode(claimed, already, idle, errCount, len(creds))
	// 只有**一个都没成功且全是错误**时才当整轮失败 ——
	// 部分失败（如某账号凭证过期）不该把整轮标成 failed：
	// 那会让用户在记录里看到"失败"，而实际上别的账号都领到了。
	if errCount > 0 && claimed == 0 && already == 0 && firstErr != nil {
		return detail, fmt.Errorf("ZCode 领取全部失败: %w", firstErr)
	}
	return detail, nil
}

// summarizeZcode 拼一句人能看懂的结果描述。
func summarizeZcode(claimed, already, idle, errCount, total int) string {
	var b strings.Builder
	fmt.Fprintf(&b, "ZCode：%d 个账号", total)
	if claimed > 0 {
		fmt.Fprintf(&b, "，领取 %d 个套餐", claimed)
	}
	if already > 0 {
		fmt.Fprintf(&b, "，%d 个已领过", already)
	}
	if idle > 0 {
		fmt.Fprintf(&b, "，%d 个暂无可领", idle)
	}
	if errCount > 0 {
		fmt.Fprintf(&b, "，%d 个失败", errCount)
	}
	return b.String()
}

// startProductTasks 启动常驻的 ZCode 自动领取调度器。
//
// 与网关自身的 `scheduler.Run` 并列启动（两个独立的循环，互不阻塞）：
//
//	网关排程   —— WorkBuddy 的签到/活跃上报等（按小时时点）
//	本调度器   —— ZCode 套餐领取（按参考实现的 5 分钟轮询 + 动态退避）
//
// ⚠ 为什么不用网关那套"按小时时点"的排程：参考实现的节奏是
// **分钟级轮询**（它要抢限量名额），按小时排会把"抢"变成"捡剩的"。
func startProductTasks(ctx context.Context, sched *zcode.ClaimScheduler) {
	if sched == nil {
		return
	}
	go sched.Run(ctx)
}
