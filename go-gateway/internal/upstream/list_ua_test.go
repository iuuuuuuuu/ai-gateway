package upstream

// 成长任务列表的 User-Agent 契约。
//
// 这是 2026-09-18 用真实账号 + 逐 UA 对照**实测**发现的既有缺陷：
//
//	Go-http-client/1.1（默认）→ 18 项，无「小程序限定」任务
//	WorkBuddy / CLI 客户端 UA → 18 项，同样无
//	小程序 UA                 → 10 项，含小程序任务但少了 8 个其它任务
//	**不发 UA**               → 20 项，含小程序任务 **且** 保留全部其它任务
//
// 上游按 UA 判定「请求来自哪个客户端平台」并据此裁剪清单；我们此前不设 UA，
// Go 的 net/http 自动补上 `Go-http-client/1.1`，被当作未知平台 → 少 2 个任务
//（school_season、Sequential_Tasks_1）。
//
// 后果很隐蔽：**不报错**，只是任务列表里少了两项，界面上表现为
// 「这个任务不存在」。若不是为了接校园日去查任务清单，这个缺陷不会被发现。
//
// 本用例锁住两件事：
//  1. 列表请求**不得**出现 Go 默认 UA（修复的本质）
//  2. 其它 growth 端点（如 accept）**保持原样**（不做无谓的改动面扩大）

import (
	"net/http"
	"testing"

	"workbuddy2api/internal/auth"
)

// TestListGrowthTasksSuppressesDefaultUserAgent 列表请求不得带 Go 默认 UA。
func TestListGrowthTasksSuppressesDefaultUserAgent(t *testing.T) {
	var captured *http.Request
	c := testClient(rtFunc(func(r *http.Request) (*http.Response, error) {
		captured = r
		return jsonResp(200, `{"code":0,"data":{"tasks":[]}}`), nil
	}))
	a := &auth.Auth{UID: "u1", AccessToken: "tok", Domain: "copilot.tencent.com"}

	if _, err := c.ListGrowthTasks(a); err != nil {
		t.Fatalf("ListGrowthTasks 失败: %v", err)
	}
	if captured == nil {
		t.Fatal("没有发出请求")
	}

	// 空串是**修复后的正确值**：Go 对 Set("") 不会回填默认 UA。
	// 若这里读到 "Go-http-client/1.1"，说明修复被移除或写法改回了「不设」。
	if got := captured.Header.Get("User-Agent"); got != "" {
		t.Errorf("列表请求的 User-Agent 应为空（实测空 UA 才能拿到全部 20 项），实际 %q", got)
	}
	// 键必须**存在且为空**（而不是缺失）——两者在 Go 里语义不同：
	// 缺失时 net/http 会自动补默认 UA，空值则原样发出空串。
	if _, ok := captured.Header["User-Agent"]; !ok {
		t.Error("User-Agent 键必须存在（值为空串）；仅「不设置」会被 net/http 回填默认值")
	}
}

// TestOtherGrowthEndpointsKeepDefaultUA 其它 growth 端点不受影响。
//
// 刻意只对列表抑制 UA：签到 / 旅行 / 领奖 / 报名等端点没有这个行为，
// 全局去掉会改变它们的指纹 —— 属无谓的改动面扩大。
// 本用例锁住「没有顺手改成全局」。
func TestOtherGrowthEndpointsKeepDefaultUA(t *testing.T) {
	var captured *http.Request
	c := testClient(rtFunc(func(r *http.Request) (*http.Response, error) {
		captured = r
		return jsonResp(200, `{"code":0,"data":{"results":[]}}`), nil
	}))
	a := &auth.Auth{UID: "u1", AccessToken: "tok", Domain: "copilot.tencent.com"}

	if _, err := c.AcceptGrowthTasks(a, []string{"chat_5"}); err != nil {
		t.Fatalf("AcceptGrowthTasks 失败: %v", err)
	}
	if captured == nil {
		t.Fatal("没有发出请求")
	}
	// accept 走普通 growthJSON：键不应被显式设为空串。
	if got := captured.Header.Get("User-Agent"); got == "" {
		if _, explicit := captured.Header["User-Agent"]; explicit {
			t.Error("accept 端点不该被抑制 UA —— 抑制只应用于任务列表")
		}
	}
}

// TestListGrowthTasksParsesMiniProgramTasks 列表能解析出小程序限定任务。
//
// 与上一条互为补充：UA 修复的目的是「拿得到」，这条确认「拿得到之后解析得对」。
func TestListGrowthTasksParsesMiniProgramTasks(t *testing.T) {
	body := `{"code":0,"data":{"tasks":[
	  {"task_code":"school_season","title":"参与「校园日」有奖活动","accept_status":"not_accepted",
	   "reward_credit":100,"reward_energy":5,"valid_end":"2026-09-24T23:59:00+08:00"},
	  {"task_code":"Sequential_Tasks_1","title":"完成 1 次对话","accept_status":"accepted",
	   "reward_credit":100,"reward_energy":5}
	]}}`
	c := testClient(rtFunc(func(r *http.Request) (*http.Response, error) {
		return jsonResp(200, body), nil
	}))
	a := &auth.Auth{UID: "u1", AccessToken: "tok", Domain: "copilot.tencent.com"}

	tasks, err := c.ListGrowthTasks(a)
	if err != nil {
		t.Fatalf("ListGrowthTasks 失败: %v", err)
	}
	if len(tasks) != 2 {
		t.Fatalf("应解析出 2 项，实际 %d", len(tasks))
	}
	byCode := map[string]GrowthTask{}
	for _, x := range tasks {
		byCode[x.Code] = x
	}
	ss, ok := byCode["school_season"]
	if !ok {
		t.Fatal("解析结果里没有 school_season")
	}
	if ss.Credit != 100 {
		t.Errorf("school_season 奖励积分应为 100，实际 %d", ss.Credit)
	}
	if !ss.NeedsAccept() {
		t.Error("school_season 未报名，NeedsAccept 应为 true")
	}
}
