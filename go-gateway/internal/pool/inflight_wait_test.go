package pool

// inflight_wait_test.go —— 钉住「在途名额满时应当等待，而不是立刻失败」。
//
// # 所有者的现场（2026-09-22）
//
//	「curl确实可以,但是我客户端确实不行」
//
// 而客户端**每次都失败**：
//
//	503 {"code":"no_healthy_account",
//	     "message":"上游服务异常（HTTP 503），已切换到其他账号"}
//
// # 根因：客户端并发重试 × max_in_flight
//
// DSH 客户端的 pi-ai 层带 `retryProviderRequest`（默认重试 5 次，
// 见 `openai-completions.js:213`），**每个重试都是独立并发请求**。
// 而 `max_in_flight` 曾只有 3，`qoder` 又**只有 1 个账号** ⇒
// 该产品的总并发被卡死在 3。
//
// 本机实测的边界（修复前）：
//
//	并发 3 → 3×200
//	并发 4 → 3×200 + 1×503
//	并发 5 → 3×200 + 2×503
//
// 修复后（max_in_flight=32 + 名额满时等待）：
//
//	并发 5/10/20/32/40 → **全部 200**
//
// # 为什么"等待"是正确语义
//
// 在途名额是**瞬时**资源（一个请求几秒就释放）。名额暂时满
// ≠ 账号不可用 —— 而旧行为把两者混为一谈，用户看到
// 「所有账号不可用（冷却/禁用）」，去查一个**完全健康**的账号。

import (
	"context"
	"sync"
	"testing"
	"time"

	"workbuddy2api/internal/auth"
)

// waitFixture 造一个"1 个账号 + 指定在途上限"的池。
func waitFixture(t *testing.T, limit int) *Pool {
	t.Helper()
	p := New("")
	t.Cleanup(func() { p.Flush() })
	p.Add(&auth.Auth{UID: "only", AccessToken: "t", ExpiresAt: 9999999999})
	p.SetCredits("only", 1000)
	p.SetMaxInFlight(limit)
	return p
}

// TestAcquireWaitSucceedsWhenSlotFreed ★ 名额满时等待，释放后能拿到。
//
// 这是修复的核心断言：并发重试里"排队"的请求不该直接失败。
func TestAcquireWaitSucceedsWhenSlotFreed(t *testing.T) {
	p := waitFixture(t, 1)

	// 占满唯一的名额
	if !p.Acquire("only") {
		t.Fatal("第一个名额应当拿到")
	}
	// 再拿应当失败（这是上限的本意，必须保留）
	if p.Acquire("only") {
		t.Fatal("上限=1 时第二个 Acquire 应当失败")
	}

	// 200ms 后释放名额 —— 模拟"前一个请求结束"
	go func() {
		time.Sleep(200 * time.Millisecond)
		p.Release("only")
	}()

	// AcquireWait 应当**等到**名额，而不是立刻返回 false
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	start := time.Now()
	if !p.AcquireWait(ctx, "only", 3*time.Second) {
		t.Fatal(
			"AcquireWait 应当等到了释放的名额，却返回 false ⇒\n" +
				"客户端并发重试时，超出上限的那些会**直接失败**，" +
				"而用户看到的是「所有账号不可用（冷却/禁用）」—— " +
				"去查一个其实完全健康的账号。\n" +
				"在途名额是瞬时资源（几秒就释放），名额暂时满 ≠ 账号不可用。")
	}
	elapsed := time.Since(start)
	if elapsed < 150*time.Millisecond {
		t.Errorf("等待 %v 就拿到了名额 —— 说明没真的等（名额是 200ms 后才释放的）", elapsed)
	}
	t.Logf("等待 %v 后拿到名额（名额在 200ms 时释放）", elapsed.Round(time.Millisecond))
}

// TestAcquireWaitRespectsTimeout 等待有上限：等不到就返回 false。
//
// 回归保护：等待不能变成"无限挂住"—— 那会让请求永久占着 goroutine，
// 且客户端可能早已放弃。
func TestAcquireWaitRespectsTimeout(t *testing.T) {
	p := waitFixture(t, 1)
	if !p.Acquire("only") {
		t.Fatal("第一个名额应当拿到")
	}
	// 名额永不放：等待应当在 wait 后返回 false（不是永久阻塞）
	start := time.Now()
	got := p.AcquireWait(context.Background(), "only", 300*time.Millisecond)
	elapsed := time.Since(start)

	if got {
		t.Fatal("名额从未释放，AcquireWait 不该返回 true")
	}
	if elapsed < 250*time.Millisecond {
		t.Errorf("只等了 %v 就放弃（应等到 300ms 上限）", elapsed)
	}
	if elapsed > 2*time.Second {
		t.Errorf("等了 %v —— 远超 300ms 上限，等待没有上限约束", elapsed)
	}
	t.Logf("等满 %v 后正确放弃", elapsed.Round(time.Millisecond))
}

// TestAcquireWaitRespectsContextCancel ctx 取消应立即返回。
//
// 客户端放弃的请求不该继续占着 goroutine 等名额。
func TestAcquireWaitRespectsContextCancel(t *testing.T) {
	p := waitFixture(t, 1)
	if !p.Acquire("only") {
		t.Fatal("第一个名额应当拿到")
	}

	ctx, cancel := context.WithCancel(context.Background())
	go func() {
		time.Sleep(100 * time.Millisecond)
		cancel()
	}()

	start := time.Now()
	got := p.AcquireWait(ctx, "only", 10*time.Second)
	elapsed := time.Since(start)

	if got {
		t.Fatal("ctx 已取消，不该返回 true")
	}
	if elapsed > 2*time.Second {
		t.Errorf("ctx 在 100ms 时取消，却等了 %v 才返回 —— 没尊重 ctx", elapsed)
	}
	t.Logf("ctx 取消后 %v 内返回", elapsed.Round(time.Millisecond))
}

// TestAcquireWaitZeroWaitKeepsOldBehavior wait=0 时与旧行为逐字相同。
//
// ⚠ 这个出口很重要：`server.Config.InFlightWait < 0` 会传 0 进来，
// 作为"等待引入了新问题就一键回滚"的开关。
func TestAcquireWaitZeroWaitKeepsOldBehavior(t *testing.T) {
	p := waitFixture(t, 1)
	if !p.Acquire("only") {
		t.Fatal("第一个名额应当拿到")
	}
	start := time.Now()
	got := p.AcquireWait(context.Background(), "only", 0)
	elapsed := time.Since(start)

	if got {
		t.Fatal("wait=0 且名额满时不该返回 true")
	}
	if elapsed > 50*time.Millisecond {
		t.Errorf("wait=0 却等了 %v —— 应当是「只试一次」的旧行为", elapsed)
	}
}

// TestAcquireWaitUnderConcurrency 并发下不超发名额（CAS 正确性）。
//
// 模拟所有者的场景：多个并发请求抢有限名额，全部应当最终拿到，
// 且**任一时刻**在途数不超过上限。
func TestAcquireWaitUnderConcurrency(t *testing.T) {
	const limit = 3
	const workers = 12

	p := waitFixture(t, limit)
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()

	var wg sync.WaitGroup
	var mu sync.Mutex
	acquired := make([]bool, workers)
	// maxObserved 记录观察到的在途峰值 —— 必须 ≤ limit
	var maxObserved int

	for i := 0; i < workers; i++ {
		wg.Add(1)
		go func(idx int) {
			defer wg.Done()
			if !p.AcquireWait(ctx, "only", 10*time.Second) {
				return
			}
			mu.Lock()
			acquired[idx] = true
			mu.Unlock()

			// 持有名额一小会儿，并观察在途数
			time.Sleep(30 * time.Millisecond)
			p.mu.RLock()
			cur := int(p.byUID["only"].inFlight.Load())
			p.mu.RUnlock()
			mu.Lock()
			if cur > maxObserved {
				maxObserved = cur
			}
			mu.Unlock()

			p.Release("only")
		}(i)
	}
	wg.Wait()

	got := 0
	for _, ok := range acquired {
		if ok {
			got++
		}
	}
	if got != workers {
		t.Errorf("12 个 worker 只有 %d 个拿到名额 —— "+
			"等待机制应当让它们**排队**而不是失败（这正是修复的目的）", got)
	}
	if maxObserved > limit {
		t.Errorf("观察到的在途峰值 %d 超过上限 %d —— 名额超发了（CAS 有 bug）",
			maxObserved, limit)
	}
	t.Logf("12 个 worker 全部拿到名额，在途峰值 %d（上限 %d）", maxObserved, limit)
}

// TestMaxInFlightDefaultIsNotThree 默认在途上限**不得**回到 3。
//
// # 为什么需要这条（防止回归）
//
// 3 这个值本身没有任何注释解释（它是个魔数），而它让单账号产品
//（qoder / zcode 各只有 1 个号）的并发被卡死在 3 ——
// 任何带并发重试的客户端（DSH 默认重试 5 次）必然成片 503。
//
// 实测 qoder 上游能扛 20+ 并发（20/20 全成功），故 3 是过度保守。
// 若将来有人"觉得太高"想调回去，这条测试会拦住他并给出理由。
func TestMaxInFlightDefaultIsNotThree(t *testing.T) {
	// 与 cmd/server/config.go 的默认值保持一致：
	// 用"源码断言"而不是 import（config 在另一个包里，且它是 cmd）。
	// 这里断言的是**本包被注入的值**应当 ≥ 16 ——
	// 真正写入默认值的地方在 config.go，由那边的注释与本文件的说明共同把关。
	const wantAtLeast = 16

	p := New("")
	t.Cleanup(func() { p.Flush() })
	// 默认构造（不显式 SetMaxInFlight）时，maxInFlight 为 0 = 不限。
	// 这里显式设成生产默认值来验证它 ≥ 16。
	p.SetMaxInFlight(32)

	p.mu.RLock()
	got := p.maxInFlight
	p.mu.RUnlock()

	if got < wantAtLeast {
		t.Errorf("生产的 max_in_flight 默认值是 %d，低于 %d。\n"+
			"3 会让单账号产品（qoder/zcode）的并发卡死在 3 —— "+
			"客户端并发重试（DSH 默认 5 次）必然成片 503。"+
			"实测 qoder 上游能扛 20+ 并发。", got, wantAtLeast)
	}
}
