package main

// product_tasks_runner_test.go ZCode 自动领取在 **cmd/server 这一层**的回归。
//
// # 调度语义的测试在哪
//
// 调度的核心（hold 硬闸 / cooldown / hold 到套餐截止 / login_required
// 永久停止 / 业务码映射）在 `internal/zcode/claim_scheduler_test.go` ——
// 那是**协议层**的知识（业务码 1001~1005、3001、3007、401 的映射），
// 与"网关怎么启动"无关，放那里才能被复用与单独演进。
//
// 本文件只测本层的两个职责：
//
//	① **结果描述的措辞**
//	② 手动路径的超时有界
//
// # 为什么单独测措辞
//
// 描述会写进任务记录，而"到底执行了没、做了什么"正是所有者反复提出的
// 问题（原话：「任务记录里面也没有」「不知道到底执行了没」）。
// 只回空字符串与"没跑"无从区分 —— 故措辞本身是有功能的。
import (
	"context"
	"strings"
	"testing"
	"time"

	"workbuddy2api/internal/zcode"
)

// TestSummarizeZcodeIsReadable 描述文本要让人看懂"这轮做了什么"。
func TestSummarizeZcodeIsReadable(t *testing.T) {
	cases := []struct {
		name                                string
		claimed, already, idle, errs, total int
		wantSubstrings                      []string
	}{
		{
			name:    "有领取也有已领",
			claimed: 1, already: 2, total: 3,
			wantSubstrings: []string{"3 个账号", "领取 1 个套餐", "2 个已领过"},
		},
		{
			name: "全部失败",
			errs: 2, total: 2,
			wantSubstrings: []string{"2 个账号", "2 个失败"},
		},
		{
			name: "暂无可领（正常状态）",
			idle: 3, total: 3,
			wantSubstrings: []string{"3 个账号", "3 个暂无可领"},
		},
		{
			name: "混合",
			claimed: 1, already: 1, idle: 1, errs: 1, total: 4,
			wantSubstrings: []string{"4 个账号", "领取 1 个套餐", "1 个已领过", "1 个暂无可领", "1 个失败"},
		},
	}
	for _, c := range cases {
		got := summarizeZcode(c.claimed, c.already, c.idle, c.errs, c.total)
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

// TestProductTaskTimeoutIsBounded 手动触发必须有超时上限。
//
// 用户在界面上点了「立即领取」在等结果，无界等待等于按钮卡死。
//
// ⚠ 这只约束**手动**路径；常驻调度器不用它 ——
// 后者按参考实现的 hold/cooldown 节奏走（见 claim_scheduler.go）。
func TestProductTaskTimeoutIsBounded(t *testing.T) {
	if productTaskTimeout <= 0 {
		t.Fatal("手动触发超时必须为正")
	}
	if productTaskTimeout > 10*time.Minute {
		t.Errorf("手动触发超时过大（%v），用户会觉得按钮卡死", productTaskTimeout)
	}
	if productTaskTimeout < 30*time.Second {
		t.Errorf("手动触发超时过小（%v），账号多时会被误判为超时", productTaskTimeout)
	}
}

// TestNewProductTasksRunnerNilWhenNoDispatch 两个产品都没有时返回 nil（跳过，不报错）。
//
// ⚠ 2026-09-22 修正：签名多了 Qoder 的两个参数。
// 「关闭」现在有**两个**来源 —— ZCode 无 dispatch、Qoder 开关为 false ——
// 必须**两个都关**才返回 nil；只要有一个产品要跑，就得给出执行体，
// 否则会出现「Qoder 开着但排程什么都不做」的静默失效。
func TestNewProductTasksRunnerNilWhenNoDispatch(t *testing.T) {
	if fn := newProductTasksRunner(nil, nil, "", false); fn != nil {
		t.Error("两个产品都不可用时才应返回 nil（调用方据此跳过，而不是报错）")
	}
	// Qoder 开着（有目录）⇒ 即使是 nil dispatch 也必须给出执行体
	if fn := newProductTasksRunner(nil, nil, "/tmp/qoder", true); fn == nil {
		t.Error("Qoder 开关为 true 时必须返回执行体 —— 否则排程静默不领 Qoder")
	}
}

// TestStartProductTasksNilIsSafe 没有调度器时启动是空操作。
//
// 单产品部署（未启用 ZCode）下 claimSched 为 nil，
// 启动函数必须容忍 —— 否则网关会在启动阶段 panic。
func TestStartProductTasksNilIsSafe(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	startProductTasks(ctx, nil) // 不应 panic
	// 非 nil 但 ctx 已取消：也不该阻塞
	startProductTasks(ctx, zcode.NewClaimScheduler(nil, nil, nil))
}
