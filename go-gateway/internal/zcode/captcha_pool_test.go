package zcode

// captcha_pool_test.go —— 钉住 param 池的行为（所有者明确要求的功能）。
//
// # 背景（所有者原话）
//
//	「而且要有一个池子,备用,不然多并发一下就不够用,还会拉长延迟」
//
// 上面这句是他对**架构**的要求。后来他又报了一个具体的性能问题：
//
//	「我首次对话就耗费了29秒，你需要优化这个」
//
// 那 29 秒的主要来源是**重复劳动**：预热已经在解了，用户的请求却另起一次求解
//（两次都在建 WebView2 窗口、都在等 SDK 下载），互相争抢且谁也没快多少。
//
// 本文件钉住池的三条契约：
//
//	① 取用是零等待的（池里有就直接给）
//	② 池空时**等已在进行的补货**，而不是让调用方另起一次
//	③ 绝不发出过期的 param（那会回 3007，比"现解一个"更糟）
//
// 这些用**假求解函数**测（不起 node、不发 HTTP）—— 池是纯数据结构，
// 与求解方式无关。

import (
	"sync/atomic"
	"testing"
	"time"
)

// newTestPool 造一个用给定函数"求解"的池。
func newTestPool(fn func() (string, error)) *captchaPool {
	p := &captchaPool{}
	p.solve = fn
	return p
}

// TestPoolTakeIsZeroWait 池里有值 ⇒ 取用不阻塞。
//
// 这是池的**存在意义**：单次求解约 1 秒（外部）到 3 秒（本地），
// 并发 N 个请求若都要现场解，延迟就是 N × 单次耗时。池化后绝大多数取用是瞬时的。
func TestPoolTakeIsZeroWait(t *testing.T) {
	var calls atomic.Int32
	p := newTestPool(func() (string, error) {
		calls.Add(1)
		return "P", nil
	})
	// 手动放两枚新鲜的
	now := time.Now()
	p.items = []pooledParam{{param: "A", born: now}, {param: "B", born: now}}

	start := time.Now()
	got := p.take(time.Now())
	elapsed := time.Since(start)

	if got == "" {
		t.Fatal("池里有新鲜值时不该返回空")
	}
	if elapsed > 50*time.Millisecond {
		t.Errorf("取用应零等待，实际耗时 %v", elapsed)
	}
	// 取用会触发**异步**补货 —— 但不能阻塞取用本身（上面已断言）。
	// 给后台一点时间，避免它在下个用例里干扰计数。
	time.Sleep(120 * time.Millisecond)
}

// TestPoolNeverReturnsExpired param 过期后**绝不返回**（返回空串让调用方现解）。
//
// # 为什么这条重要
//
// param 有约 90 秒寿命。发出一个已过期的值，上游回 3007 ——
// 那比"池空、现解一个"更糟：现解至少有机会成功，而发过期值是**确定的失败**，
// 还会让用户看到"验证码失败"这种误导性错误。
func TestPoolNeverReturnsExpired(t *testing.T) {
	p := newTestPool(func() (string, error) { return "", nil })
	// 放一枚**已过期**的（born 早于 TTL）
	p.items = []pooledParam{{param: "STALE", born: time.Now().Add(-captchaParamTTL - time.Second)}}

	if got := p.take(time.Now()); got != "" {
		t.Errorf("过期值不得返回，实际返回了 %q", got)
	}
}

// TestPoolExpiresAtBoundary 边界：刚好在 TTL 内可用，超出即弃。
func TestPoolExpiresAtBoundary(t *testing.T) {
	p := newTestPool(func() (string, error) { return "", nil })
	now := time.Now()
	// 差一点点到 TTL —— 仍算有效
	p.items = []pooledParam{{param: "FRESH", born: now.Add(-captchaParamTTL + time.Second)}}
	if got := p.take(now); got != "FRESH" {
		t.Errorf("TTL 内应可用，实际 %q", got)
	}
	// 刚过 TTL —— 必须丢弃
	p.items = []pooledParam{{param: "STALE", born: now.Add(-captchaParamTTL - time.Millisecond)}}
	if got := p.take(now); got != "" {
		t.Errorf("刚过 TTL 应丢弃，实际返回 %q", got)
	}
}

// TestTakeOrWaitWaitsForInFlightRefill 池空且补货进行中 ⇒ **等它**，而不是立刻返回空。
//
// # 这是本次「首次对话 29 秒」修复的核心断言
//
// 旧行为：池空就立刻走现场求解 —— 而后台预热也在解，两次并行做**同样重**的事
//（各建一个 WebView2 窗口、各等一次 SDK 下载），用户等的是两者的交集而非其一。
//
// 新行为：池空且**已有补货在跑**时等它（上限 captchaWaitForRefill）。
//
// # 为什么不断言"求解调用次数"
//
// 我第一版就是这么写的，结果红了：
//
//	等待方不该触发求解，实际调用了 2 次
//
// 那 2 次是 `take()` 触发的**后台补货**（池低于水位就补货，是设计行为），
// 与"等待方自己求解"完全是两回事。把两者混为一谈，测出来的就不是契约
// 而是实现细节 —— 一旦补货策略调整（比如水位从 3 改成 5），测试就会误报。
//
// 故改测**可观察的行为**：
//
//	① 补货在跑时，takeOrWait **不立刻返回**（证明它在等，而不是返回空）
//	② 补货交付后，它**拿到那个值**
//
// 这两条才是调用方依赖的契约。
func TestTakeOrWaitWaitsForInFlightRefill(t *testing.T) {
	// 求解函数只返回错误：确保**只有**手工交付的那一枚会被拿到 ——
	// 若 takeOrWait 走了"自己求解"的路，它会拿不到任何值并返回空。
	p := newTestPool(func() (string, error) { return "", errString("测试中不真求解") })

	// 模拟"预热补货正在跑"
	p.mu.Lock()
	p.refill = true
	p.mu.Unlock()

	deliver := make(chan struct{})
	go func() {
		<-deliver
		p.mu.Lock()
		p.items = append(p.items, pooledParam{param: "FROM_REFILL", born: time.Now()})
		p.refill = false
		p.mu.Unlock()
	}()

	result := make(chan string, 1)
	go func() {
		result <- p.takeOrWait(time.Now(), 5*time.Second)
	}()

	// ① 补货还在跑时不该返回（返回空就等于"没等"）
	select {
	case got := <-result:
		t.Fatalf("补货进行中时不该立刻返回（拿到 %q）—— 说明没有等待", got)
	case <-time.After(300 * time.Millisecond):
		// 正确：它在等
	}

	// ② 交付后应拿到那一枚
	close(deliver)
	select {
	case got := <-result:
		if got != "FROM_REFILL" {
			t.Errorf("应拿到补货交付的值，实际 %q", got)
		}
	case <-time.After(3 * time.Second):
		t.Fatal("补货交付后 takeOrWait 仍未返回")
	}
}

// TestTakeOrWaitReturnsEmptyWhenNoRefill 没有补货在跑 ⇒ 立刻返回空（让调用方现解）。
//
// 与上一条成对：只测"会等"会让"永远等下去"也算通过。
// 这里必须**立刻**返回空 —— 否则请求会白等 captchaWaitForRefill（25 秒）。
func TestTakeOrWaitReturnsEmptyWhenNoRefill(t *testing.T) {
	p := newTestPool(func() (string, error) { return "X", nil })

	start := time.Now()
	got := p.takeOrWait(time.Now(), 5*time.Second)
	elapsed := time.Since(start)

	if got != "" {
		t.Errorf("池空且无补货时应返回空，实际 %q", got)
	}
	if elapsed > 200*time.Millisecond {
		t.Errorf("无补货时不该等待，实际耗时 %v", elapsed)
	}
}

// TestTakeOrWaitHonorsDeadline 补货迟迟不完成 ⇒ 到点放弃（不能让请求无限挂）。
//
// 请求路径上的等待必须有上限：否则上游/宿主异常时，用户的对话会一直挂着。
func TestTakeOrWaitHonorsDeadline(t *testing.T) {
	p := newTestPool(func() (string, error) { return "", nil })
	// 永久"补货中"（模拟补货卡死）
	p.mu.Lock()
	p.refill = true
	p.mu.Unlock()

	start := time.Now()
	got := p.takeOrWait(time.Now(), 500*time.Millisecond)
	elapsed := time.Since(start)

	if got != "" {
		t.Errorf("补货未产出时应返回空，实际 %q", got)
	}
	if elapsed < 400*time.Millisecond {
		t.Errorf("应等到约 500ms 的上限，实际只等了 %v", elapsed)
	}
	if elapsed > 2*time.Second {
		t.Errorf("等待应受上限约束，实际 %v", elapsed)
	}
}

// TestPoolRefillStopsOnError 补货遇到错误就停（不连续撞失败）。
//
// 失败往往伴随冷却或确定性原因（组件缺失、宿主不可用）。
// 继续循环补货只会刷日志、烧 CPU，且毫无成功的可能。
func TestPoolRefillStopsOnError(t *testing.T) {
	var calls atomic.Int32
	p := newTestPool(func() (string, error) {
		calls.Add(1)
		return "", errString("求解失败")
	})

	p.refillTo(captchaPoolTarget)

	if n := calls.Load(); n != 1 {
		t.Errorf("失败后应停止补货（只试 1 次），实际 %d 次", n)
	}
	if got := p.take(time.Now()); got != "" {
		t.Errorf("失败的补货不该留下任何值，实际 %q", got)
	}
}

// TestPoolRefillRespectsMax 补货不超过硬顶（防止无限增长）。
func TestPoolRefillRespectsMax(t *testing.T) {
	var calls atomic.Int32
	p := newTestPool(func() (string, error) {
		n := calls.Add(1)
		return "P" + string(rune('0'+n)), nil
	})

	// 目标 > 上限时，应以**上限**为准
	p.refillTo(captchaPoolMax + 5)

	if got := len(p.items); got > captchaPoolMax {
		t.Errorf("池不得超过硬顶 %d，实际 %d", captchaPoolMax, got)
	}
}

// errString 一个简单的 error 实现（避免为测试引入 errors 包的别名冲突）。
type errString string

func (e errString) Error() string { return string(e) }
