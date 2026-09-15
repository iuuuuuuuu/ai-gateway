package pool

import (
	"testing"
	"time"

	"workbuddy2api/internal/auth"
)

// ---------------------------------------------------------------------------
// 「单一模型 + 积分轮转」模式
//
// 语义：始终只用**一个**账号，把它烧到不可用才换下一个；换的仍是按到期日
// 排序的下一个。与负载均衡（同档内分摊、多号并行）形成对比。
// ---------------------------------------------------------------------------

func rotationPool(t *testing.T) *Pool {
	t.Helper()
	old := minPickGap
	minPickGap = 0
	t.Cleanup(func() { minPickGap = old })

	p := New("")
	// 三个账号，到期日递增；故意打乱加入顺序，验证排序不是靠插入序。
	for _, a := range []struct {
		uid  string
		days int
	}{
		{"late", 30},
		{"soon", 10},
		{"mid", 20},
	} {
		p.Add(&auth.Auth{UID: a.uid, SoonestExpireAt: time.Now().Add(time.Duration(a.days) * 24 * time.Hour).Unix()})
		p.SetCredits(a.uid, 1000)
	}
	return p
}

// TestRotationSticksToSameAccount 轮转模式必须**反复选中同一个账号**，
// 而不是像负载均衡那样在候选间分摊。
func TestRotationSticksToSameAccount(t *testing.T) {
	p := rotationPool(t)
	p.SetRotation(true)

	first := p.PickForModel("m1", nil)
	if first == nil {
		t.Fatal("应能选出账号")
	}
	// 到期最早的是 "soon"
	if first.UID != "soon" {
		t.Fatalf("应选到期最早的 soon，实际 %s", first.UID)
	}
	// 连续 50 次都必须还是它（负载均衡下这里会散开）
	for i := 0; i < 50; i++ {
		got := p.PickForModel("m1", nil)
		if got == nil || got.UID != "soon" {
			t.Fatalf("第 %d 次应仍为 soon，实际 %v", i, got)
		}
	}
}

// TestRotationSwitchesWhenAccountUnavailable 账号不可用时才换下一个，
// 且换的是「按到期日排序的下一个」。
func TestRotationSwitchesWhenAccountUnavailable(t *testing.T) {
	p := rotationPool(t)
	p.SetRotation(true)

	if got := p.PickForModel("m1", nil); got == nil || got.UID != "soon" {
		t.Fatalf("首选应为 soon，实际 %v", got)
	}
	// 让 soon 账号级冷却 → 应轮到 mid
	p.Cooldown("soon", CoolSoft, time.Hour, "测试占用")
	if got := p.PickForModel("m1", nil); got == nil || got.UID != "mid" {
		t.Fatalf("soon 不可用后应轮到 mid，实际 %v", got)
	}
	// 再让 mid 也冷却 → 应轮到 late
	p.Cooldown("mid", CoolSoft, time.Hour, "测试占用")
	if got := p.PickForModel("m1", nil); got == nil || got.UID != "late" {
		t.Fatalf("mid 不可用后应轮到 late，实际 %v", got)
	}
}

// TestRotationSwitchesOnModelCooldown 该模型被限流时，轮转模式**换账号而不换模型**
// —— 这正是「单一模型」的语义：模型锁定不变，账号轮转。
func TestRotationSwitchesOnModelCooldown(t *testing.T) {
	p := rotationPool(t)
	p.SetRotation(true)

	if got := p.PickForModel("only-model", nil); got == nil || got.UID != "soon" {
		t.Fatalf("首选应为 soon，实际 %v", got)
	}
	// soon 在 only-model 上被限流 → 应换到 mid（而不是换模型）
	p.CooldownModel("soon", "only-model", time.Now().Add(time.Hour), "限流", true)
	if got := p.PickForModel("only-model", nil); got == nil || got.UID != "mid" {
		t.Fatalf("该模型限流后应换账号到 mid，实际 %v", got)
	}
	// 但换个模型时，soon 仍是可用的（模型级冷却只封该模型）
	p.rotationUID = "" // 清掉锁定，验证候选池本身
	if got := p.PickForModel("other-model", nil); got == nil || got.UID != "soon" {
		t.Fatalf("换模型后 soon 应重新可用（模型冷却只封该模型），实际 %v", got)
	}
}

// TestRotationRecoversWhenOriginalComesBack 原账号恢复后是否会被重新选中。
//
// 语义选择：**不会自动切回**。轮转模式的定义是「烧到不可用才换下一个」，
// 若原账号恢复就切回，会造成 A/B 之间反复横跳，违背「把一个烧干净再走」的初衷。
func TestRotationRecoversWhenOriginalComesBack(t *testing.T) {
	p := rotationPool(t)
	p.SetRotation(true)

	p.PickForModel("m1", nil) // 锁到 soon
	p.Cooldown("soon", CoolSoft, time.Hour, "临时")
	if got := p.PickForModel("m1", nil); got == nil || got.UID != "mid" {
		t.Fatalf("应轮到 mid，实际 %v", got)
	}
	// 手动复活 soon（模拟冷却到期）—— 同包测试可直接改私有状态。
	// coolKind 只有 CoolHard/CoolSoft 两个值，没有「未冷却」枚举：
	// healthy() 只看 until/breakerUntil 是否到期，故把两个截止清零即可。
	p.mu.Lock()
	if e, ok := p.byUID["soon"]; ok {
		e.until = time.Time{}
		e.breakerUntil = time.Time{}
		e.fails = 0
	}
	p.mu.Unlock()
	// 仍应继续用 mid：不横跳
	if got := p.PickForModel("m1", nil); got == nil || got.UID != "mid" {
		t.Fatalf("原账号恢复后不应横跳回 soon，实际 %v", got)
	}
}

// TestRotationOffFallsBackToBalance 关闭轮转后应回到原有分摊行为。
func TestRotationOffFallsBackToBalance(t *testing.T) {
	p := rotationPool(t)
	p.SetRotation(true)
	p.PickForModel("m1", nil)

	p.SetRotation(false)
	if p.RotationOn() {
		t.Fatal("应已关闭")
	}
	// 负载均衡下同档（此处各号不同档，故只有最早档参与）——
	// 关键是它能选出账号且不再受 rotationUID 锁定。
	got := p.PickForModel("m1", nil)
	if got == nil {
		t.Fatal("关闭轮转后仍应能选出账号")
	}
}

// TestRotationRespectsTried 请求级轮换（tried）在轮转模式下同样生效 ——
// 上层 forward 的重试逻辑依赖它，不能因为新模式而失效。
func TestRotationRespectsTried(t *testing.T) {
	p := rotationPool(t)
	p.SetRotation(true)

	got := p.PickForModel("m1", map[string]bool{"soon": true})
	if got == nil || got.UID != "mid" {
		t.Fatalf("tried 应排除 soon，实际 %v", got)
	}
}

// TestRotationDeterministicOrder 排序必须确定（不随机）：
// 「下一个是谁」在轮转语义下应当可预期。
func TestRotationDeterministicOrder(t *testing.T) {
	// 重复构造多次，首个被选中的账号必须始终是到期最早的那个
	for i := 0; i < 20; i++ {
		p := rotationPool(t)
		p.SetRotation(true)
		if got := p.PickForModel("m1", nil); got == nil || got.UID != "soon" {
			t.Fatalf("第 %d 轮首选应为 soon，实际 %v", i, got)
		}
	}
}

// TestRotationUnknownExpiryGoesLast 到期日未知的账号排最后（与分层口径一致）。
func TestRotationUnknownExpiryGoesLast(t *testing.T) {
	old := minPickGap
	minPickGap = 0
	defer func() { minPickGap = old }()

	p := New("")
	p.Add(&auth.Auth{UID: "unknown"}) // 无 SoonestExpireAt
	p.Add(&auth.Auth{UID: "known", SoonestExpireAt: time.Now().Add(72 * time.Hour).Unix()})
	p.SetCredits("unknown", 1000)
	p.SetCredits("known", 1000)
	p.SetRotation(true)

	if got := p.PickForModel("m1", nil); got == nil || got.UID != "known" {
		t.Fatalf("应优先选有到期日的账号，实际 %v", got)
	}
}

// TestRotationAllUnavailableReturnsNil 全部不可用时返回 nil（或走兜底），
// 不能 panic、也不能返回不可用账号当作正常结果。
func TestRotationAllUnavailableReturnsNil(t *testing.T) {
	p := rotationPool(t)
	p.SetRotation(true)
	for _, uid := range []string{"soon", "mid", "late"} {
		p.Disable(uid, "测试禁用")
	}
	if got := p.PickForModel("m1", nil); got != nil {
		t.Fatalf("全部禁用时应返回 nil，实际 %v", got.UID)
	}
}
