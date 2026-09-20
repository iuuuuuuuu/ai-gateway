package main

// product_tasks.go 产品日常任务（ZCode 套餐领取）的排程执行体。
//
// # 所有者要求（2026-09-20）
//
//	「他那个仓库还有个自动领取那个积分包的功能，我们也要接进来」
//	「qoder这个活动卡片…而且任务也应该自动执行」
//
// 即"别让我每天手点"。
//
// # ⚠ 与"不做自动抢"的关系（这条很重要，别改错）
//
// `internal/zcode/claim.go` 的文件头写着：
//
//	「ClaimPlan 只被显式调用…**不做定时自动抢** —— 那与"用户点一下"不是
//	  一回事，且会让账号表现出非人类的活动模式」
//
// 那条结论**仍然成立**，它反对的是**抢**：
//
//	抢 = 高频探测（参考实现每 5 分钟）+ 争限量名额（先到先得）+ 失败重试
//
// 而所有者要的是**做**：
//
//	做 = 每天一次 + 幂等（已领回 1003 "already_claimed"）+ **零重试**
//
// 两者的风控画像完全不同：人类也会每天打开客户端看一眼有没有新的可领，
// 但不会每 5 分钟探测一次并在失败后立刻重试。
//
// 故本实现刻意**不照搬**参考实现的调度参数：
//
//	参考实现：pollInterval 5min / cooldown 10min / 失败重试 / holdUntil 抢窗口
//	本实现：  每天一次（schedule.product_tasks_hours，默认 10 点）/ 零重试
//
// # 凭证从哪来
//
// 由宿主通过 `pool.zcode_auth_dir` 透传 —— 网关直接读该目录下的凭证文件，
// 用它调 `billing/preview` + `billing/claim`。
// 这正是"网关自己就能做"的依据：它一直持有这些凭证（对话路径就在用）。
import (
	"context"
	"fmt"
	"log"
	"strings"
	"time"

	"workbuddy2api/internal/zcode"
)

// productTaskTimeout 单个产品的整轮超时。
//
// 取 3 分钟：preview + 逐账号 claim，账号多时也要够用；
// 但不能无限等 —— 排程循环是串行的，卡住会拖掉后面的任务。
const productTaskTimeout = 3 * time.Minute

// newProductTasksRunner 构造产品日常任务的执行体。
//
// 返回 nil 表示"当前构建没有可跑的产品"（此时排程会跳过，不报错）——
// 例如单产品部署下 zcode 未启用。
func newProductTasksRunner(zc *zcode.Dispatch) func() (string, error) {
	if zc == nil {
		return nil
	}
	return func() (string, error) {
		ctx, cancel := context.WithTimeout(context.Background(), productTaskTimeout)
		defer cancel()
		return runZcodeClaims(ctx, zc)
	}
}

// runZcodeClaims 跑一轮 ZCode 套餐领取。
//
// # 为什么返回"描述字符串"而不是只返回 error
//
// 描述会写进任务记录（`产品日常任务`），用户要能看懂"这一轮做了什么"：
//
//	"ZCode：3 个账号，领取 1 个套餐，2 个今天已领过"
//
// 只回 error 的话，成功时用户看到一条空记录，与"没跑"无从区分 ——
// 而"到底执行了没"正是所有者最关心的问题（见任务留痕那轮反馈）。
func runZcodeClaims(ctx context.Context, zc *zcode.Dispatch) (string, error) {
	creds, failed, err := zc.LoadCreds()
	if err != nil {
		return "", fmt.Errorf("加载 ZCode 凭证失败: %w", err)
	}
	if len(creds) == 0 {
		// 没有凭证不是错误：可能是没登录过 ZCode，或该部署没启用它。
		// 如实说明，让用户知道"任务跑了，但没账号可跑"。
		msg := "ZCode：没有可用凭证，跳过"
		if len(failed) > 0 {
			msg = fmt.Sprintf("ZCode：没有可用凭证（%d 个解析失败），跳过", len(failed))
		}
		return msg, nil
	}

	var claimed, already, skipped, errCount int
	var firstErr error

	// ⚠ **严格串行**（不并发）：这是避风控的核心约束之一。
	//
	// 参考实现同样严格串行（`setTimeout`-in-`finally` 链，绝不重叠）。
	// 并发打同一上游会让"一个账号被多台设备同时操作"，那是最容易被标记的画像。
	for _, cr := range creds {
		if err := ctx.Err(); err != nil {
			// 超时/取消：不当作失败（只是这轮没跑完），但要如实说明。
			return summarizeZcode(claimed, already, skipped, errCount, len(creds)),
				fmt.Errorf("ZCode 领取超时（已处理部分账号）")
		}

		preview, err := zc.FetchPlanPreview(ctx, cr)
		if err != nil {
			errCount++
			if firstErr == nil {
				firstErr = err
			}
			log.Printf("product_tasks zcode %s: preview 失败: %v", shortUID(cr.UID), err)
			continue
		}
		if preview == nil || len(preview.Offers) == 0 {
			// 没有可领套餐是**正常状态**（活动未上线/已领完），不是错误。
			skipped++
			continue
		}

		// 取第一个可领的（见 pickOffer 的选择口径）。
		target := pickOffer(preview.Offers)
		if target == nil {
			skipped++
			continue
		}

		res, err := zc.ClaimPlan(ctx, cr, target.PlanID)
		if err != nil {
			errCount++
			if firstErr == nil {
				firstErr = err
			}
			log.Printf("product_tasks zcode %s: claim %s 失败: %v",
				shortUID(cr.UID), target.PlanID, err)
			// ⚠ **不重试**：失败就等明天那个时点。
			// 这正是与参考实现最重要的差异，也是本功能不扩大风控面的关键 ——
			// 重试风暴是把账号打进风控的典型特征（我们在 ZCode 上经历过）。
			continue
		}
		switch {
		case res.AlreadyClaimed:
			// 幂等命中：**不是失败**（用户昨天领过/今天已领过）。
			already++
		case res.OK:
			claimed++
		default:
			skipped++
		}
	}

	detail := summarizeZcode(claimed, already, skipped, errCount, len(creds))

	// 只有**一个账号都没成功且全是错误**时才当整轮失败 ——
	// 部分失败（如某账号凭证过期）不该把整轮标成 failed：
	// 那会让用户在记录里看到"失败"，而实际上别的账号都领到了。
	if errCount > 0 && claimed == 0 && already == 0 {
		return detail, fmt.Errorf("ZCode 领取全部失败: %w", firstErr)
	}
	return detail, nil
}

// summarizeZcode 拼一句人能看懂的结果描述。
func summarizeZcode(claimed, already, skipped, errCount, total int) string {
	var b strings.Builder
	fmt.Fprintf(&b, "ZCode：%d 个账号", total)
	if claimed > 0 {
		fmt.Fprintf(&b, "，领取 %d 个套餐", claimed)
	}
	if already > 0 {
		fmt.Fprintf(&b, "，%d 个今天已领过", already)
	}
	if skipped > 0 {
		fmt.Fprintf(&b, "，%d 个暂无可领", skipped)
	}
	if errCount > 0 {
		fmt.Fprintf(&b, "，%d 个失败", errCount)
	}
	return b.String()
}

// pickOffer 从可领套餐里挑一个。
//
// # 选择口径：取第一个能领的
//
// 参考实现按 `priority` 最高挑（服务端顺序作 tiebreak），但**我们的
// `PlanOffer` 没有解析 priority**（只有 planId / name / status / endsAt）——
// 那字段目前没被读出来。与其在这里现加一个解析（并因此改上游结构体的形状），
// 不如取第一个可领的：preview 返回的**本来就是"当前可领"的清单**，
// 顺序由服务端给，先出现的即它认为更该领的。
//
// ⚠ 若将来发现同一账号有多个可领套餐且顺序不稳，再补 priority 解析。
// 当前 `Offers` 实测通常 0 或 1 条，取第一个与取 priority 最高等价。
func pickOffer(offers []zcode.PlanOffer) *zcode.PlanOffer {
	for i := range offers {
		pid := strings.TrimSpace(offers[i].PlanID)
		if pid == "" {
			continue
		}
		// status 非空且明显不可领时跳过（上游偶尔把"已结束"也发下来）
		if s := strings.ToLower(strings.TrimSpace(offers[i].Status)); s == "expired" || s == "ended" {
			continue
		}
		return &offers[i]
	}
	return nil
}

// shortUID 日志用的短 uid（完整 uid 又长又占地方）。
func shortUID(uid string) string {
	if len(uid) <= 8 {
		return uid
	}
	return uid[:8]
}


