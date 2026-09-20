// 产品日常任务的**真机验证**（跑一轮领取，看真实响应）。
//
// # 为什么需要它
//
// 调度逻辑已被 15 条单测覆盖（决策表、hold、cooldown、幂等分类），
// 但单测喂的是**我构造的**假响应。真实上游的响应形状、状态码、
// 以及"我们的 claim 请求头是否被接受"只有真机能回答。
//
// # ⚠ 安全约束（这个工具刻意做得很保守）
//
//	① **只跑一轮** —— 不循环、不重试。参考实现每 5 分钟一轮，
//	   而我们这次只发一个 preview + 至多一个 claim。
//	② **只读凭证** —— 不写回、不修改任何账号文件。
//	③ **幂等安全** —— claim 是幂等的：已领过的账号回 `1003 already_claimed`。
//	   实测所有者的 `zcode-v3-start-plan-0817` 已是 active（已领过），
//	   故预期就是收到 1003 —— 那恰好验证了幂等路径，且**零副作用**。
//	④ 默认 `-dry-run`：只查 preview（**不发 claim**）。
//	   要看 claim 的真实结果必须显式加 `-claim`。
//
// # 用法
//
//	go run ./cmd/zcode-claim-probe                    # 只看可领清单（不发 claim）
//	go run ./cmd/zcode-claim-probe -claim             # 真的发一次 claim
//	go run ./cmd/zcode-claim-probe -uid xxx -claim    # 只对某个账号
package main

import (
	"context"
	"flag"
	"fmt"
	"os"
	"time"

	"workbuddy2api/internal/zcode"
)

func main() {
	var (
		dir     = flag.String("dir", "", "凭证目录（空 = 默认 ~/.wb-switch/zcode/auths）")
		uid     = flag.String("uid", "", "只处理该账号（空 = 全部，但每账号仍只跑一轮）")
		doClaim = flag.Bool("claim", false, "**真的发 claim**（默认只查 preview）")
		timeout = flag.Duration("timeout", 60*time.Second, "整轮超时")
	)
	flag.Parse()

	authDir := *dir
	if authDir == "" {
		authDir = zcode.DefaultAuthDir()
	}
	fmt.Printf("凭证目录: %s\n", authDir)

	creds, failed, err := zcode.LoadDir(authDir)
	if err != nil {
		fmt.Printf("✗ 加载凭证失败: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("载入 %d 个凭证（解析失败 %d 个）\n\n", len(creds), len(failed))
	if len(creds) == 0 {
		fmt.Println("没有可用凭证，退出。")
		return
	}

	ctx, cancel := context.WithTimeout(context.Background(), *timeout)
	defer cancel()

	client := zcode.New()
	// ⚠ 刻意**不**调 SetClaimBaseForTest：本工具的目的就是打**真上游**。

	for _, cr := range creds {
		if *uid != "" && cr.UID != *uid {
			continue
		}
		runOne(ctx, client, cr, *doClaim)
	}
}

func runOne(ctx context.Context, client *zcode.Client, cr *zcode.Cred, doClaim bool) {
	fmt.Printf("── 账号 %s ──\n", short(cr.UID))
	fmt.Printf("   provider=%v  deviceMid=%s  jwt=%v\n",
		cr.Provider, mask(cr.DeviceMid), cr.JWT != "")

	// ① preview
	pv, err := client.FetchPlanPreview(ctx, cr)
	if err != nil {
		fmt.Printf("   ✗ preview 失败: %v\n", err)
		if e, ok := err.(*zcode.Error); ok {
			fmt.Printf("     status=%d code=%q msg=%q\n", e.Status, e.Code, e.Msg)
		}
		fmt.Println()
		return
	}
	fmt.Printf("   ✓ preview 成功：%d 个可领套餐（server_time=%d）\n",
		len(pv.Offers), pv.ServerTime)
	for i, o := range pv.Offers {
		ends := "未提供"
		if o.EndsAt > 0 {
			ends = time.Unix(o.EndsAt, 0).Format("2006-01-02 15:04:05")
		}
		fmt.Printf("     [%d] plan_id=%s name=%q status=%q ends_at=%s\n",
			i, o.PlanID, o.Name, o.Status, ends)
	}

	if len(pv.Offers) == 0 {
		fmt.Println("   → 无可领套餐（正常状态：活动未上线或已领完）")
		fmt.Println()
		return
	}

	// ② claim（默认不发）
	if !doClaim {
		fmt.Println("   ⊘ 未加 -claim，跳过领取（dry-run）")
		fmt.Println()
		return
	}

	target := pv.Offers[0]
	fmt.Printf("   → 尝试领取 plan_id=%s ...\n", target.PlanID)
	res, err := client.ClaimPlan(ctx, cr, target.PlanID)
	if err != nil {
		fmt.Printf("   ✗ claim 失败: %v\n", err)
		if e, ok := err.(*zcode.Error); ok {
			fmt.Printf("     status=%d code=%q msg=%q\n", e.Status, e.Code, e.Msg)
		}
		fmt.Println()
		return
	}
	fmt.Printf("   ✓ claim 返回：ok=%v code=%q alreadyClaimed=%v msg=%q\n",
		res.OK, res.Code, res.AlreadyClaimed, res.Msg)
	switch {
	case res.AlreadyClaimed:
		fmt.Println("   → **幂等命中**（之前已领过）—— 这正是调度器判 already_claimed 的那条路径")
	case res.OK:
		fmt.Println("   → 真的领到了")
	}
	fmt.Println()
}

func short(s string) string {
	if len(s) <= 8 {
		return s
	}
	return s[:8] + "…"
}

func mask(s string) string {
	if len(s) <= 8 {
		return s
	}
	return s[:8] + "…"
}
