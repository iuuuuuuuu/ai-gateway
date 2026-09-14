package pool

import (
	"testing"
	"time"

	"workbuddy2api/internal/auth"
)

// issue #5 症状之二回归：软冷却（429）不得放大熔断的指数退避。
//
// 修复前：Cooldown(CoolSoft) 会把 retryCount 递增，导致一次流量高峰把账号
// 按 30m→1h→2h→4h→6h 逐步封死；实测 3 次 429 即封 30 分钟，且 until/coolKind
// 会落盘 → 重启客户端也不恢复（用户反馈「频繁不可用 + 重启无效」）。
// 修复后：429 仍累计 fails（保留"反复失败即熔断"语义），但熔断时长不叠加放大。
func TestIssue5_SoftCooldownDoesNotEscalateBackoff(t *testing.T) {
	p := New("")
	p.Add(&auth.Auth{UID: "u1"})

	var waits []time.Duration
	for round := 0; round < 6; round++ {
		for i := 0; i < defaultBreakerThreshold; i++ {
			p.Cooldown("u1", CoolSoft, time.Millisecond, "429 rate limit")
		}
		p.mu.Lock()
		waits = append(waits, time.Until(p.byUID["u1"].breakerUntil).Round(time.Second))
		retry := p.byUID["u1"].retryCount
		p.mu.Unlock()
		if retry != 0 {
			t.Fatalf("第 %d 轮 429 后 retryCount=%d，软冷却不应放大退避指数", round, retry)
		}
		time.Sleep(3 * time.Millisecond)
	}
	t.Logf("各轮软冷却触发的熔断时长: %v", waits)

	// 每一轮都必须停留在首次熔断时长（defaultBreakerCooldown），不得指数上升。
	for i, w := range waits {
		if w > defaultBreakerCooldown+time.Second {
			t.Errorf("第 %d 轮熔断时长 %v 超过基准 %v（429 不应放大退避）",
				i, w, defaultBreakerCooldown)
		}
	}
}

// 反向保障：真正的连续 5xx 故障仍必须走完整指数退避（原设计意图不回归）。
func TestIssue5_ServerErrorsStillEscalateBackoff(t *testing.T) {
	p := New("")
	p.Add(&auth.Auth{UID: "u1"})

	var waits []time.Duration
	for round := 0; round < 4; round++ {
		for i := 0; i < defaultBreakerThreshold; i++ {
			p.NoteError("u1") // 5xx 路径
		}
		p.mu.Lock()
		waits = append(waits, time.Until(p.byUID["u1"].breakerUntil).Round(time.Minute))
		p.mu.Unlock()
	}
	t.Logf("各轮 5xx 触发的熔断时长: %v", waits)

	if len(waits) >= 2 && waits[1] <= waits[0] {
		t.Errorf("5xx 熔断时长应指数增长: %v", waits)
	}
	if waits[len(waits)-1] < 2*time.Hour {
		t.Errorf("连续 5xx 末轮熔断=%v，应有明显退避放大", waits[len(waits)-1])
	}
}

// 软冷却仍然必须累计 fails 并在达到阈值时触发熔断（原有契约不回归）。
func TestIssue5_SoftCooldownStillFeedsBreaker(t *testing.T) {
	p := New("")
	p.Add(&auth.Auth{UID: "u1"})
	p.SetBreaker(3, 30*time.Minute, 6*time.Hour)

	// 未达阈值：不熔断。
	p.Cooldown("u1", CoolSoft, time.Millisecond, "429")
	p.Cooldown("u1", CoolSoft, time.Millisecond, "429")
	p.mu.Lock()
	if !p.byUID["u1"].breakerUntil.IsZero() {
		p.mu.Unlock()
		t.Fatal("未达阈值不应熔断")
	}
	p.mu.Unlock()

	// 第 3 次达到阈值：必须熔断。
	p.Cooldown("u1", CoolSoft, time.Millisecond, "429")
	p.mu.Lock()
	brk := p.byUID["u1"].breakerUntil
	p.mu.Unlock()
	if brk.IsZero() {
		t.Fatal("达到阈值应触发熔断（软冷却仍是熔断器的失败信号）")
	}
}
