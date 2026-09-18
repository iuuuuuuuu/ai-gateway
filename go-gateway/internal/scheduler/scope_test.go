package scheduler

// 养号任务的**作用范围**契约。
//
// 所有者指出：「这个养号任务，不应该是局限于这个账号的吗？」
//
// 他说的对。原实现只有「作用于全部账号」一种语义（`RunTaskByName` 遍历账号池），
// 而入口长在**账号卡片**上 —— 用户在某个号上点「活跃上报」，跑的却是整池，
// 与菜单位置传达的意思相反。
//
// 修正后：
//
//	RunTaskFor(name, "")   → 全部账号（右上角「一键操作」的入口）
//	RunTaskFor(name, uid)  → **只作用于该 uid**（账号卡片菜单的入口）
//
// 作用域通过 ctx 传递，各任务的遍历循环只加一行 `inAccountScope(ctx, st.UID)`。
// 这样排程路径（不带作用域）的行为逐字不变，不需要为每个任务再写一份实现。
//
// 本文件锁住三件事：作用域生效、排程不受影响、区域不符时给得出原因。

import (
	"context"
	"strings"
	"testing"

	"workbuddy2api/internal/auth"
)

// TestRunTaskForScopesToSingleAccount 指定 uid 时只有该账号被上报。
func TestRunTaskForScopesToSingleAccount(t *testing.T) {
	rec := &activityRecorder{streak: 1}
	s := newActivityScheduler(t, rec, 1,
		&auth.Auth{UID: "u1", AccessToken: "tok"},
		&auth.Auth{UID: "u2", AccessToken: "tok"},
		&auth.Auth{UID: "u3", AccessToken: "tok"},
	)

	res, err := s.RunTaskFor(TaskNameActivity, "u2")
	if err != nil {
		t.Fatalf("RunTaskFor 失败: %v", err)
	}
	if !res.Ran {
		t.Fatalf("应执行成功，实际 ran=%v skip=%v msg=%v", res.Ran, res.Skip, res.Message)
	}

	seen := []string{}
	for _, ev := range rec.snapshot() {
		if uid, ok := ev["userId"].(string); ok {
			seen = append(seen, uid)
		}
	}
	if len(seen) != 1 || seen[0] != "u2" {
		t.Errorf("指定 u2 时只应有 u2 被上报，实际 %v —— "+
			"跑成整池就是所有者报的那个语义错误", seen)
	}
}

// TestRunTaskForEmptyUIDMeansAllAccounts 不传 uid 时作用于全部账号。
//
// 这条与上一条**成对**才有意义：只测「单个生效」而不测「全部仍生效」，
// 就可能把「全部账号」这个入口一起改坏而无人发现。
func TestRunTaskForEmptyUIDMeansAllAccounts(t *testing.T) {
	rec := &activityRecorder{streak: 1}
	s := newActivityScheduler(t, rec, 1,
		&auth.Auth{UID: "u1", AccessToken: "tok"},
		&auth.Auth{UID: "u2", AccessToken: "tok"},
		&auth.Auth{UID: "u3", AccessToken: "tok"},
	)

	res, err := s.RunTaskFor(TaskNameActivity, "")
	if err != nil {
		t.Fatalf("RunTaskFor 失败: %v", err)
	}
	if !res.Ran {
		t.Fatalf("应执行成功，实际 ran=%v skip=%v", res.Ran, res.Skip)
	}

	seen := map[string]bool{}
	for _, ev := range rec.snapshot() {
		if uid, ok := ev["userId"].(string); ok {
			seen[uid] = true
		}
	}
	for _, uid := range []string{"u1", "u2", "u3"} {
		if !seen[uid] {
			t.Errorf("不传 uid 时应作用于全部账号，但 %s 没被上报（实际 %v）", uid, seen)
		}
	}
}

// TestRunTaskForUnknownAccountReportsReason 账号不在池中时回报原因，不是静默跑空。
//
// 单账号触发时用户盯着结果看：什么都不发生比一句说明糟糕得多。
func TestRunTaskForUnknownAccountReportsReason(t *testing.T) {
	rec := &activityRecorder{streak: 1}
	s := newActivityScheduler(t, rec, 1, &auth.Auth{UID: "u1", AccessToken: "tok"})

	res, err := s.RunTaskFor(TaskNameActivity, "not-in-pool")
	if err != nil {
		t.Fatalf("账号不存在是**预期状态**，不该返回 error：%v", err)
	}
	if res.Ran {
		t.Error("账号不在池中时不该报 ran=true")
	}
	if res.Skip != "account_not_found" {
		t.Errorf("skip=%q want account_not_found", res.Skip)
	}
	if !strings.Contains(res.Message, "账号池") {
		t.Errorf("说明应指出账号不在池中，实际 %q", res.Message)
	}
	// 关键：不该向上游发出任何请求。
	if n := len(rec.snapshot()); n != 0 {
		t.Errorf("账号不存在时不该打上游，实际 %d 次请求", n)
	}
}

// TestRunTaskForRegionMismatchReportsReason 区域不符时**提前**给出原因。
//
// trial 只跑国际版；活跃上报/夜猫子/开学季只跑国服。单账号触发时若只是
// 静默跳过（遍历循环里 continue），用户点了按钮会看到「已执行」却什么都没发生。
func TestRunTaskForRegionMismatchReportsReason(t *testing.T) {
	rec := &activityRecorder{streak: 1}
	// 国服账号 + trial（trial 只对国际版成立）。
	s := newActivityScheduler(t, rec, 1,
		&auth.Auth{UID: "cn", AccessToken: "tok", Domain: "copilot.tencent.com"},
	)

	res, err := s.RunTaskFor(TaskNameTrial, "cn")
	if err != nil {
		t.Fatalf("区域不符是预期状态，不该返回 error：%v", err)
	}
	if res.Ran {
		t.Error("国服账号跑 trial 不该报 ran=true")
	}
	if res.Skip != "region_mismatch" {
		t.Errorf("skip=%q want region_mismatch", res.Skip)
	}
	if !strings.Contains(res.Message, "国际版") {
		t.Errorf("说明应指出该任务是国际版专属，实际 %q", res.Message)
	}

	// 反向：国际版账号跑 trial 应放行（区域判据不能把两边都挡掉）。
	rec2 := &activityRecorder{streak: 1}
	s2 := newActivityScheduler(t, rec2, 1,
		&auth.Auth{UID: "intl", AccessToken: "tok", Domain: "www.workbuddy.ai"},
	)
	res2, err := s2.RunTaskFor(TaskNameTrial, "intl")
	if err != nil {
		t.Fatalf("国际版跑 trial 不该报错: %v", err)
	}
	if !res2.Ran {
		t.Errorf("国际版账号跑 trial 应放行，实际 skip=%v msg=%v", res2.Skip, res2.Message)
	}
}

// TestInAccountScopeDefaultsToAll 无作用域时恒为 true（排程路径不受影响）。
//
// 这是本机制与既有排程共存的关键：排程不设作用域，行为必须逐字不变。
func TestInAccountScopeDefaultsToAll(t *testing.T) {
	// 用 context.Background() 而不是 t.Context()：后者要 Go 1.24+，
	// 而本模块的 go 指令是 1.22（vet 会直接报错）。
	ctx := context.Background()
	if !inAccountScope(ctx, "any-uid") {
		t.Error("无作用域时应恒为 true —— 否则排程会一个账号都不跑")
	}

	scoped := withAccountScope(ctx, "u-target")
	if !inAccountScope(scoped, "u-target") {
		t.Error("作用域内的账号应为 true")
	}
	if inAccountScope(scoped, "u-other") {
		t.Error("作用域外的账号应为 false")
	}
	// 空串作用域等价于无作用域（宿主可能传空）。
	if !inAccountScope(withAccountScope(ctx, ""), "any-uid") {
		t.Error("空作用域应等价于「全部账号」")
	}
}
