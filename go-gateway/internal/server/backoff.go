package server

// backoff.go 轮转换号之间的**退避 + 抖动**。
//
// # 要解决的问题（真实缺陷）
//
// forward.go 的轮转主循环此前是**零延迟**连打：一个账号失败后立刻换下一个账号
// 重发（对 internal/server 的非测试文件 grep `time.Sleep|backoff|jitter` = 0 命中）。
//
// 对上游 WAF 来说，这串请求来自同一个出口 IP、间隔毫秒级、目标账号各不相同 ——
// 这是非常明显的机器特征，正是触发/加重风控的行为。上游实测记录见 wafip.go：
// 「WAF 403 拦的是网关出口 IP 而非账号 —— 3 个账号 1 秒内全 403」。
//
// # 为什么必须带抖动（而不是固定退避）
//
// 固定退避会让**并发的多个请求在同一时刻一起醒来重试**（惊群）：本来各自独立的
// 失败被对齐成一次集中的重试脉冲，反而比不退避更像机器行为。抖动把重试时刻打散，
// 让每个请求的节奏互不相关。
//
// # 取值依据
//
// 与上游 Sliverkiss/workbuddy2api 的 internal/server/backoff.go 对齐：
//
//	rotateBackoffBase = 500ms   首次换号前的等待：够短，用户几乎无感
//	rotateBackoffCap  = 8s      上限：再长也不如直接把失败报回去让客户端重试
//	jitterFraction    = 0.25    ±25% 抖动
//
// # 退避发生的位置（三个约束缺一不可）
//
//   - **第 0 次尝试不退避** —— 正常请求的第一发不该被拖慢。这里天然满足：
//     退避只在「第 i 次失败、准备换第 i+1 个号」时调用，循环第一次迭代走不到。
//   - **最后一次失败后不退避** —— 后面没有换号了，白等只是拖慢错误返回。
//     由调用方（rotateBackoff 闭包）按 MaxRotate 判掉。
//   - **可被 ctx 取消** —— 客户端断开后不该还在为一个没人要的请求空等，
//     否则一次断连会白占一个 goroutine 数秒（见 sleepCtx）。

import (
	"context"
	"fmt"
	"math/rand/v2"
	"sync"
	"time"
)

const (
	// rotateBackoffBase 第 0 次换号前的退避基准（抖动前）。
	rotateBackoffBase = 500 * time.Millisecond
	// rotateBackoffCap 退避上限：指数增长到它就不再增长。
	//
	// 有上限是必须的：换号次数由 MaxRotate 决定，而它是**配置项**（用户可调大），
	// 没有上限时 2^n 会很快涨到分钟级 —— 那已经不是「等一下再试」，
	// 而是让客户端超时。超过 8 秒就不如把失败如实报回去。
	rotateBackoffCap = 8 * time.Second
	// jitterFraction 抖动幅度：±25% 的基准退避量。
	//
	// 这些取值都让 ±25% 落在整数纳秒上（500ms·2^n·0.25 = 125ms·2^n），
	// 故单测可以直接断言上下界的**精确值**，不必留容差。
	jitterFraction = 0.25
)

// rotateRandInt64N 退避抖动的随机源，取 [0,n) 的 int64。
//
// 用可替换的包级变量而不是直接调 math/rand/v2：单测要能**确定性地**断言
// 抖动的上下界（注入恒返回 0 的源即取下界，恒返回 n-1 即取上界）。
// 这与 pool.SetRandomSource 是同一口径 —— 该项目已有先例。
var (
	rotateRandMu     sync.RWMutex
	rotateRandInt64N = rand.Int64N
)

// SetRotateRandomSource 仅供测试注入确定性随机源；生产代码不应调用。
// 传 nil 恢复 math/rand/v2 全局源。
//
// 加锁而不是裸赋值：注入发生在测试里，而读取发生在并发的请求 goroutine 中，
// 裸赋值会被 -race 判成数据竞争（且真有可能读到半个函数值）。
func SetRotateRandomSource(fn func(n int64) int64) {
	if fn == nil {
		fn = rand.Int64N
	}
	rotateRandMu.Lock()
	defer rotateRandMu.Unlock()
	rotateRandInt64N = fn
}

// rotateRand 取一个 [0,n) 的随机数。
func rotateRand(n int64) int64 {
	rotateRandMu.RLock()
	fn := rotateRandInt64N
	rotateRandMu.RUnlock()
	return fn(n)
}

// backoffBase 第 n 次换号前的退避基准：min(base · 2^n, cap)，**不含抖动**。
//
// 拆出无抖动的确定性内核，是为了让「指数增长」「封顶」这两条性质可以被
// 直接断言，不必先把随机性绕开（带抖动的 backoffAfter 只负责在它上面叠加）。
func backoffBase(n int) time.Duration {
	if n < 0 {
		n = 0
	}
	d := rotateBackoffBase
	// 用循环而不是 `base << n`：n 由 MaxRotate 决定，而 MaxRotate 是配置项，
	// 直接左移会在 n 较大时溢出成负数（time.Timer 拿到负时长会立即触发，
	// 表现为「退避静默失效」，正是本缺陷要避免的行为）。
	// 循环在触顶后立即停止，故不可能溢出。
	for i := 0; i < n && d < rotateBackoffCap; i++ {
		d *= 2
		if d > rotateBackoffCap {
			d = rotateBackoffCap
		}
	}
	return d
}

// backoffAfter 第 n 次换号前的实际退避量：backoffBase(n) 上叠加 ±jitterFraction 抖动。
//
// 纯函数（除可注入的随机源外无副作用），便于单测确定性地断言上下界。
//
// n 的含义是**已失败尝试的下标**（从 0 起）：第 0 次尝试失败后换号时传 0，
// 得到约 500ms；第 1 次失败后传 1，约 1s；以此类推。
func backoffAfter(n int) time.Duration {
	d := backoffBase(n)
	amp := time.Duration(float64(d) * jitterFraction)
	if amp <= 0 {
		return d
	}
	// 在 [-amp, +amp] 上均匀取值：先取 [0, 2·amp] 的整数，再减去 amp。
	//
	// span = 2·amp + 1 而不是 2·amp：rotateRand 的取值域是 [0, span)，
	// 故 span 必须恰好覆盖 2·amp + 1 个整数（含 0 与 2·amp 两端），
	// 少了这个 +1 就取不到上界 d+amp。span 恒 >= 3（上面已保证 amp > 0），
	// 不会出现 rotateRand(0) 那种越界。
	span := int64(2*amp) + 1
	out := d + time.Duration(rotateRand(span)) - amp
	if out < 0 {
		out = 0
	}
	return out
}

// sleepCtx 睡满 d；ctx 结束（客户端断开 / 请求超时）时立即返回 false。
//
// 返回 false 表示**没睡满**，调用方应据此中止轮转 —— 请求的发起方已经走了，
// 再换号重试只是替一个没人要的请求继续打上游。
//
// ⚠ 这里为什么自己写一个而不是复用 internal/scheduler 或 internal/growtask 里
// 同名的 sleepCtx：那两份分别是「优雅停机」与「任务取消」语义，与这里只是
// 形似；为一个十行函数让 server 依赖调度器/养号任务包，会把依赖方向倒过来。
// （那两个包彼此也没有共用它，是同样的取舍。）
func sleepCtx(ctx context.Context, d time.Duration) bool {
	if d <= 0 {
		return true
	}
	// 先看 ctx 再看定时器：ctx 已经结束时应立即返回，不白等一个完整退避。
	select {
	case <-ctx.Done():
		return false
	default:
	}
	t := time.NewTimer(d)
	defer t.Stop()
	select {
	case <-ctx.Done():
		return false
	case <-t.C:
		return true
	}
}

// rotateCanceledErr 退避等待期间请求上下文结束，轮转中止。
//
// 单独成型而不是直接透出 ctx.Err()：后者在日志与客户端文案里只有
// "context canceled"，看不出是「退避时客户端走了」还是「上游调用被取消」——
// 这两件事的排查方向完全不同（前者无需处理，后者要查上游）。
func rotateCanceledErr(ctx context.Context) error {
	return fmt.Errorf("客户端已断开或请求超时，已中止换号重试：%w", ctx.Err())
}
