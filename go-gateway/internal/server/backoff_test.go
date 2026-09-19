package server

// backoff_test.go 轮转换号退避的断言。
//
// 这些用例守护的是「零延迟连打」这个真实缺陷不再复发：
// 任何一次换号都必须隔着一段**有下界、有上界、且不同请求不重合**的等待。

import (
	"context"
	"errors"
	"testing"
	"time"
)

// withRotateRand 注入确定性随机源，返回恢复函数。
//
// 注入是**必须**的：backoffAfter 带 ±25% 抖动，不注入就只能断言一个宽区间，
// 而宽区间恰好覆盖不住「抖动写反了」「span 差一」这类真实错误 ——
// 那正是本项目 pool.SetRandomSource 存在的同款理由。
func withRotateRand(t *testing.T, fn func(n int64) int64) {
	t.Helper()
	SetRotateRandomSource(fn)
	t.Cleanup(func() { SetRotateRandomSource(nil) })
}

// TestBackoffAfterBoundsAndMonotonic 断言 n=0/1/2 的退避量落在上下界之间，且严格递增。
//
// 上下界的**精确值**：base(n) = 500ms·2^n，抖动 ±25% ⇒ [0.75·base, 1.25·base]。
// 注入「恒取下界」与「恒取上界」两个源分别断言，比只断言单调性更强 ——
// 后者在「抖动被写成 0」时仍会通过（那样就退化成固定退避，正是惊群的成因）。
func TestBackoffAfterBoundsAndMonotonic(t *testing.T) {
	// base 分别为 500ms / 1s / 2s，抖动后下界 375ms / 750ms / 1.5s、上界 625ms / 1.25s / 2.5s。
	cases := []struct {
		n            int
		lower, upper time.Duration
	}{
		{0, 375 * time.Millisecond, 625 * time.Millisecond},
		{1, 750 * time.Millisecond, 1250 * time.Millisecond},
		{2, 1500 * time.Millisecond, 2500 * time.Millisecond},
	}

	// 下界：随机源恒返回 0 → d - amp。
	withRotateRand(t, func(n int64) int64 { return 0 })
	var lowers []time.Duration
	for _, c := range cases {
		got := backoffAfter(c.n)
		if got != c.lower {
			t.Errorf("n=%d 下界：backoffAfter = %s，期望 %s", c.n, got, c.lower)
		}
		lowers = append(lowers, got)
	}
	for i := 1; i < len(lowers); i++ {
		if lowers[i] <= lowers[i-1] {
			t.Errorf("n=%d 的退避（%s）未严格大于 n=%d 的（%s）：指数增长失效",
				i, lowers[i], i-1, lowers[i-1])
		}
	}

	// 上界：随机源恒返回 n-1 → d + amp。
	//
	// ⚠ 断言精确上界（而不是「<= 上界」）是有意的：span 少算 1 就会让
	// 上界缩水 1ns，`<=` 那种写法永远发现不了。
	withRotateRand(t, func(n int64) int64 { return n - 1 })
	var uppers []time.Duration
	for _, c := range cases {
		got := backoffAfter(c.n)
		if got != c.upper {
			t.Errorf("n=%d 上界：backoffAfter = %s，期望 %s", c.n, got, c.upper)
		}
		uppers = append(uppers, got)
	}
	for i := 1; i < len(uppers); i++ {
		if uppers[i] <= uppers[i-1] {
			t.Errorf("n=%d 的退避（%s）未严格大于 n=%d 的（%s）：指数增长失效",
				i, uppers[i], i-1, uppers[i-1])
		}
	}
}

// TestBackoffJitterSpreadsRetries 断言抖动**真的**把重试时刻打散。
//
// 这是本模块的存在理由：固定退避会让并发请求在同一刻一起醒来重试（惊群），
// 制造出比不退避更明显的机器特征。若抖动被误删（例如有人把 amp 写成 0），
// 本用例必须失败。
func TestBackoffJitterSpreadsRetries(t *testing.T) {
	// 用一个确定性但取值分散的伪源：n=0/1/2 时分别取首、中、末。
	seq := []int64{0, 1, 2}
	i := 0
	withRotateRand(t, func(n int64) int64 {
		v := seq[i%len(seq)]
		i++
		if v >= n {
			v = n - 1
		}
		return v
	})

	// 同一 n 在抖动下应能取到**不同**的值（否则就是固定退避）。
	seen := map[time.Duration]bool{}
	for k := 0; k < 8; k++ {
		seen[backoffAfter(2)] = true
	}
	if len(seen) < 2 {
		t.Errorf("抖动未生效：8 次 backoffAfter(2) 只得到 %d 个不同取值 %v", len(seen), seen)
	}
}

// TestBackoffBaseCapAndNoOverflow 断言封顶生效，且极大 n 不会溢出成负数。
//
// 溢出是真会踩到的坑：MaxRotate 是**配置项**，用户调大后 `base << n`
// 会翻成负数，而 time.Timer 拿到负时长会**立即触发** ——
// 表现为「退避静默失效」，正好把本缺陷原样放回来。
func TestBackoffBaseCapAndNoOverflow(t *testing.T) {
	// 触顶前逐级翻倍：500ms → 1s → 2s → 4s → 8s(触顶)。
	want := []time.Duration{
		500 * time.Millisecond,
		1 * time.Second,
		2 * time.Second,
		4 * time.Second,
		rotateBackoffCap,
		rotateBackoffCap,
	}
	for n, w := range want {
		if got := backoffBase(n); got != w {
			t.Errorf("backoffBase(%d) = %s，期望 %s", n, got, w)
		}
	}
	// 极大 n：必须稳定停在 cap，且为正。
	for _, n := range []int{10, 31, 63, 64, 1000} {
		got := backoffBase(n)
		if got != rotateBackoffCap {
			t.Errorf("backoffBase(%d) = %s，期望封顶在 %s", n, got, rotateBackoffCap)
		}
		if got <= 0 {
			t.Errorf("backoffBase(%d) = %s：溢出成非正数，退避会静默失效", n, got)
		}
	}
	// 负数 n 按第 0 次处理（调用方下标算错时不该 panic 或返回 0）。
	if got := backoffBase(-1); got != rotateBackoffBase {
		t.Errorf("backoffBase(-1) = %s，期望按 n=0 处理为 %s", got, rotateBackoffBase)
	}
	// 封顶后叠加抖动仍在合理范围（6s~10s），且始终为正。
	withRotateRand(t, func(n int64) int64 { return 0 })
	if got := backoffAfter(10); got != 6*time.Second {
		t.Errorf("backoffAfter(10) 下界 = %s，期望 6s", got)
	}
	withRotateRand(t, func(n int64) int64 { return n - 1 })
	if got := backoffAfter(10); got != 10*time.Second {
		t.Errorf("backoffAfter(10) 上界 = %s，期望 10s", got)
	}
}

// TestSleepCtxCancellable 断言退避**可被 ctx 取消**。
//
// 为什么这条必须有：客户端断开后我们若还在空等，一次断连就白占一个
// goroutine 数秒；轮转越靠后（退避越长）浪费越大。
func TestSleepCtxCancellable(t *testing.T) {
	// 已取消的 ctx：立即返回 false，不等满。
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	start := time.Now()
	if sleepCtx(ctx, 5*time.Second) {
		t.Error("ctx 已取消时 sleepCtx 应返回 false")
	}
	if d := time.Since(start); d > 500*time.Millisecond {
		t.Errorf("ctx 已取消却等了 %s，应立即返回", d)
	}

	// 未取消的 ctx：睡满并返回 true。
	if !sleepCtx(context.Background(), 10*time.Millisecond) {
		t.Error("ctx 未取消时 sleepCtx 应返回 true")
	}

	// d<=0：不退避，直接放行（不依赖 ctx 状态）。
	if !sleepCtx(ctx, 0) {
		t.Error("d<=0 时不该等，应返回 true")
	}

	// 睡眠途中取消：应提前醒来。
	ctx2, cancel2 := context.WithCancel(context.Background())
	go func() {
		time.Sleep(20 * time.Millisecond)
		cancel2()
	}()
	start = time.Now()
	if sleepCtx(ctx2, 5*time.Second) {
		t.Error("睡眠途中被取消应返回 false")
	}
	if d := time.Since(start); d > time.Second {
		t.Errorf("取消后仍等了 %s，应及时醒来", d)
	}
}

// TestRotateCanceledErrWrapsContext 断言中止轮转的错误带上了可读说明。
//
// 直接透出 ctx.Err() 只有 "context canceled"，在日志与客户端文案里
// 看不出是「退避时客户端走了」还是「上游调用被取消」。
func TestRotateCanceledErrWrapsContext(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	err := rotateCanceledErr(ctx)
	if !errors.Is(err, context.Canceled) {
		t.Errorf("应可用 errors.Is 认出 context.Canceled，实际 %v", err)
	}
	if msg := err.Error(); len(msg) == 0 || msg == "context canceled" {
		t.Errorf("文案应说明是退避被中止，实际 %q", msg)
	}
}
