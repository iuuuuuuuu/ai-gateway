package main

// product_tasks_test.go 产品日常任务（ZCode 自动领取）的排期与结果口径回归。
//
// # 守的是什么
//
// 所有者要求「自动领取…我们也要接进来」「任务也应该自动执行」——
// 但**我自己此前的代码注释**明确写着「不做定时自动抢，会让账号表现出
// 非人类的活动模式」。两者看似冲突，实际不冲突：
//
//	抢 = 高频探测（参考实现每 5 分钟）+ 争限量名额 + 失败重试
//	做 = 每天一次 + 幂等（已领回 1003）+ **零重试**
//
// 故这组测试锁的是"做"的那三条约束，**而不是**参考实现的调度参数。
// 将来若有人把间隔改成 5 分钟、或加上重试，这里会红。
import (
	"strings"
	"testing"
	"time"

	"workbuddy2api/internal/zcode"
)

// TestSummarizeZcodeIsReadable 描述文本要让人看懂"这轮做了什么"。
//
// 为什么重要：描述会写进任务记录，而"到底执行了没"正是所有者反复
// 提出的问题（「任务记录里面也没有」「不知道到底执行了没」）。
// 只回一句空字符串与"没跑"无从区分。
func TestSummarizeZcodeIsReadable(t *testing.T) {
	cases := []struct {
		name                     string
		claimed, already, skip, errCount, total int
		wantSubstrings           []string
	}{
		{
			name: "有领取有已领",
			claimed: 1, already: 2, skip: 0, errCount: 0, total: 3,
			wantSubstrings: []string{"3 个账号", "领取 1 个套餐", "2 个今天已领过"},
		},
		{
			name: "全部失败",
			claimed: 0, already: 0, skip: 0, errCount: 2, total: 2,
			wantSubstrings: []string{"2 个账号", "2 个失败"},
		},
		{
			name: "无可领（正常状态）",
			claimed: 0, already: 0, skip: 3, errCount: 0, total: 3,
			wantSubstrings: []string{"3 个账号", "3 个暂无可领"},
		},
	}
	for _, c := range cases {
		got := summarizeZcode(c.claimed, c.already, c.skip, c.errCount, c.total)
		for _, want := range c.wantSubstrings {
			if !strings.Contains(got, want) {
				t.Errorf("%s：描述应含 %q，实际 %q", c.name, want, got)
			}
		}
		// 不该出现"0 个xxx"这类噪音
		if strings.Contains(got, "0 个") {
			t.Errorf("%s：描述不该出现「0 个」这类噪音，实际 %q", c.name, got)
		}
	}
}

// TestPickOfferSkipsUnclaimable 挑选时跳过明显不可领的项。
func TestPickOfferSkipsUnclaimable(t *testing.T) {
	// 空列表
	if got := pickOffer(nil); got != nil {
		t.Errorf("空列表应返回 nil，实际 %+v", got)
	}
	// 只有已结束/已过期的
	ended := []zcode.PlanOffer{
		{PlanID: "plan-a", Status: "expired"},
		{PlanID: "plan-b", Status: "ENDED"},
	}
	if got := pickOffer(ended); got != nil {
		t.Errorf("全是已结束的应返回 nil，实际 %+v", got)
	}
	// 第一个可领的被选中（后面的"已结束"被跳过后仍返回第一条可领的）
	mixed := []zcode.PlanOffer{
		{PlanID: "plan-0", Status: "claimable"},
		{PlanID: "plan-1", Status: "expired"},
	}
	got := pickOffer(mixed)
	if got == nil || got.PlanID != "plan-0" {
		t.Errorf("应选中第一个可领的（plan-0），实际 %+v", got)
	}
	// 空 PlanID 跳过（应落到后面那个有 id 的）
	blank := []zcode.PlanOffer{
		{PlanID: "   ", Status: "claimable"},
		{PlanID: "plan-real", Status: "claimable"},
	}
	if got := pickOffer(blank); got == nil || got.PlanID != "plan-real" {
		t.Errorf("空 planId 应跳过、取后面的 plan-real，实际 %+v", got)
	}
}

// TestProductTasksHoursDefaultsToDailyNotFiveMinutes 「做」不是「抢」。
//
// ⚠ 这条是**风控约束的回归**：参考实现每 5 分钟轮询（它要抢限量名额），
// 而我们刻意每天一次。若有人把默认改成高频，这里会红 ——
// 那不是"优化"，是把账号推向非人类画像。
func TestProductTasksHoursDefaultsToDailyNotFiveMinutes(t *testing.T) {
	c := Default()
	if len(c.Schedule.ProductTasksHours) != 1 {
		t.Fatalf("默认应是**每天一个时点**（而非多时点/高频轮询），实际 %v",
			c.Schedule.ProductTasksHours)
	}
	if c.Schedule.ProductTasksHours[0] != 10 {
		t.Errorf("默认时点应是 10 点（与活跃上报同轮但串行），实际 %v",
			c.Schedule.ProductTasksHours)
	}
	if !c.Schedule.ProductTasksEnabled {
		t.Error("默认应启用（所有者明确要求自动执行；默认关等于没做）")
	}
}

// TestProductTasksHoursValidated 非法时点要被校验拦住。
//
// 排程用 `nextFire(hours)` 算唤醒时点；非法值（如 25）会让它算出
// 永不触发的时刻 —— 那是**静默失效**：配置看起来生效了，任务却从不跑。
func TestProductTasksHoursValidated(t *testing.T) {
	c := Default()
	c.Schedule.ProductTasksHours = []int{25}
	if err := c.validateScheduleHours(); err == nil {
		t.Error("时点 25 非法，应被校验拒绝（否则任务静默不跑）")
	}
	c.Schedule.ProductTasksHours = []int{10}
	if err := c.validateScheduleHours(); err != nil {
		t.Errorf("时点 10 应合法，实际报错: %v", err)
	}
}

// TestProductTaskTimeoutIsBounded 整轮必须有超时上限。
//
// 排程循环是**串行**的：一轮卡住会拖掉后面所有任务（含签到）。
// 故必须有界，且不能大到"等于没有超时"。
func TestProductTaskTimeoutIsBounded(t *testing.T) {
	if productTaskTimeout <= 0 {
		t.Fatal("整轮超时必须为正")
	}
	if productTaskTimeout > 10*time.Minute {
		t.Errorf("整轮超时 %v 过大 —— 串行排程下会拖掉后续任务", productTaskTimeout)
	}
	if productTaskTimeout < 30*time.Second {
		t.Errorf("整轮超时 %v 过小 —— 账号多时会被误判为超时", productTaskTimeout)
	}
}

// TestNewProductTasksRunnerNilWhenNoDispatch 没有 zcode 时返回 nil（排程跳过）。
func TestNewProductTasksRunnerNilWhenNoDispatch(t *testing.T) {
	if fn := newProductTasksRunner(nil); fn != nil {
		t.Error("没有 zcode dispatch 时应返回 nil（排程据此跳过，而不是报错）")
	}
}

// TestShortUIDIsBounded 日志用的短 uid 必须真有界（不是"看起来短"）。
func TestShortUIDIsBounded(t *testing.T) {
	long := "0123456789abcdef0123456789abcdef"
	if got := shortUID(long); len(got) > 8 {
		t.Errorf("shortUID 应截到 ≤8，实际 %q（%d 字符）", got, len(got))
	}
	// 短的原样返回
	if got := shortUID("abc"); got != "abc" {
		t.Errorf("短 uid 应原样返回，实际 %q", got)
	}
}
