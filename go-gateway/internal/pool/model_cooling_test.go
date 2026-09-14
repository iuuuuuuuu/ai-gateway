package pool

import (
	"os"
	"path/filepath"
	"testing"
	"time"

	"workbuddy2api/internal/auth"
)

// ---------------------------------------------------------------------------
// 需求（2026-09-15）：区分「余额欠费」与「模型冷却」，并按 uid+model 粒度避开路由
//
// 现场报错（429，上游原始响应体）：
//
//	{"code":"6004","msg":"您的使用量已超出频率限制，将在 2026-09-15 13:25:47 UTC+8 重置，
//	 您也可以切换其他模型继续使用。","requestId":"62b8393f-..."}
//
// 关键点：这是**模型级**限流 —— 上游自己提示"切换其他模型继续使用"，
// 因此不能把整个账号冷却掉（那样会连带浪费该账号其他仍可用的模型额度）。
// ---------------------------------------------------------------------------

// TestCooldownModelIsolatesSingleModel 模型冷却只影响该模型，账号其他模型照常可选。
func TestCooldownModelIsolatesSingleModel(t *testing.T) {
	withNoPickGap(t)
	p := New("")
	p.Add(&auth.Auth{UID: "u1"})
	p.SetCredits("u1", 1000)

	until := time.Now().Add(2 * time.Hour)
	p.CooldownModel("u1", "deepseek-v4.1-flash", until, "deepseek-v4.1-flash 已达频率上限", true)

	// 被冷却的模型 → 选不出来
	if got := p.PickForModel("deepseek-v4.1-flash", nil); got != nil {
		t.Fatalf("该模型已冷却，不应选到账号，got=%+v", got)
	}
	// 其他模型 → 照常可用（这正是"切换其他模型继续使用"的语义）
	if got := p.PickForModel("glm-5.3", nil); got == nil || got.UID != "u1" {
		t.Fatalf("其他模型应仍可选到该账号，got=%+v", got)
	}
	// 不带模型（旧路径）→ 不受模型冷却影响，保持向后兼容
	if got := p.Pick(); got == nil || got.UID != "u1" {
		t.Fatalf("无模型上下文时应退化为原有行为，got=%+v", got)
	}
}

// TestCooldownModelDoesNotAffectAccountHealth 模型冷却不把账号标成"账号级冷却"。
//
// 这条正是需求要区分的两种状态：余额欠费 = 账号级（cooling=true）；
// 模型冷却 = 仅该模型（cooling 仍为 false，但 model_cooling 非空）。
func TestCooldownModelDoesNotAffectAccountHealth(t *testing.T) {
	p := New("")
	p.Add(&auth.Auth{UID: "u1"})
	p.SetCredits("u1", 1000)

	until := time.Now().Add(time.Hour)
	p.CooldownModel("u1", "deepseek-v4.1-flash", until, "reason", true)

	st, ok := p.Status("u1")
	if !ok {
		t.Fatal("no status")
	}
	if st.Cooling {
		t.Errorf("模型冷却不应把账号标成账号级冷却（那是余额欠费的语义）: %+v", st)
	}
	if st.CoolKind != "" {
		t.Errorf("账号级 cool_kind 应为空，实际=%q", st.CoolKind)
	}
	if len(st.ModelCooling) != 1 {
		t.Fatalf("model_cooling 应有 1 条，实际=%d (%+v)", len(st.ModelCooling), st.ModelCooling)
	}
	mc := st.ModelCooling[0]
	if mc.Model != "deepseek-v4.1-flash" {
		t.Errorf("model=%q", mc.Model)
	}
	if mc.RemainingSec < 3500 || mc.RemainingSec > 3700 {
		t.Errorf("remaining_sec=%d，期望约 1 小时", mc.RemainingSec)
	}
	if !mc.ResetAtParsed {
		t.Error("reset_at_parsed 应为 true（到期时间来自上游文案）")
	}
}

// TestModelCoolingCoexistsWithAccountCooldown 两种冷却可同时存在且互不覆盖。
func TestModelCoolingCoexistsWithAccountCooldown(t *testing.T) {
	p := New("")
	p.Add(&auth.Auth{UID: "u1"})

	// 账号级：余额欠费（硬冷却到次日 04:00）
	p.CooldownUntilTomorrow4AM("u1", "余额不足")
	// 模型级：某模型限流
	p.CooldownModel("u1", "glm-5.3", time.Now().Add(3*time.Hour), "glm-5.3 已达频率上限", true)

	st, _ := p.Status("u1")
	if !st.Cooling || st.CoolKind != "hard_credit" {
		t.Errorf("账号级冷却应保留: cooling=%v kind=%q", st.Cooling, st.CoolKind)
	}
	if len(st.ModelCooling) != 1 {
		t.Errorf("模型冷却应同时存在: %+v", st.ModelCooling)
	}
}

// TestCooldownModelExpiresAndPrunes 到期后自动失效并被清理（不无限增长）。
func TestCooldownModelExpiresAndPrunes(t *testing.T) {
	withNoPickGap(t)
	p := New("")
	p.Add(&auth.Auth{UID: "u1"})
	p.SetCredits("u1", 1000)

	// 已过期的冷却：写入后不应生效。
	p.CooldownModel("u1", "glm-5.3", time.Now().Add(-time.Second), "已过期", true)
	if got := p.PickForModel("glm-5.3", nil); got == nil {
		t.Fatal("已过期的模型冷却不应继续拦截")
	}
	st, _ := p.Status("u1")
	if len(st.ModelCooling) != 0 {
		t.Errorf("已过期的模型冷却不应出现在 status: %+v", st.ModelCooling)
	}

	// 短冷却到期后，pick 的清理逻辑应把它从 map 移除。
	p.CooldownModel("u1", "glm-5.3", time.Now().Add(30*time.Millisecond), "短冷却", true)
	if got := p.PickForModel("glm-5.3", nil); got != nil {
		t.Fatal("冷却期内不应选到")
	}
	time.Sleep(60 * time.Millisecond)
	p.PickForModel("glm-5.3", nil) // 触发 prune

	p.mu.RLock()
	n := len(p.byUID["u1"].modelCools)
	p.mu.RUnlock()
	if n != 0 {
		t.Errorf("过期后应被 prune 清掉，仍有 %d 条", n)
	}
}

// TestCooldownModelUnknownUIDAndEmptyModel 边界：未知 uid / 空模型名不应 panic 或误记。
func TestCooldownModelUnknownUIDAndEmptyModel(t *testing.T) {
	p := New("")
	p.Add(&auth.Auth{UID: "u1"})

	p.CooldownModel("nope", "glm-5.3", time.Now().Add(time.Hour), "x", true) // 未知 uid：忽略
	p.CooldownModel("u1", "", time.Now().Add(time.Hour), "x", true)          // 空模型：忽略
	p.CooldownModel("u1", "glm-5.3", time.Time{}, "x", true)                // 零时刻：忽略

	st, _ := p.Status("u1")
	if len(st.ModelCooling) != 0 {
		t.Errorf("无效入参不应写入冷却: %+v", st.ModelCooling)
	}
}

// TestPickForModelExcludesAllCooledAccounts 全部账号的该模型都被限流时返回 nil
// （让上层报"该模型全部账号受限"，而不是继续撞限流）。
func TestPickForModelExcludesAllCooledAccounts(t *testing.T) {
	withNoPickGap(t)
	p := New("")
	for _, u := range []string{"u1", "u2"} {
		p.Add(&auth.Auth{UID: u})
		p.SetCredits(u, 1000)
		p.CooldownModel(u, "deepseek-v4.1-flash", time.Now().Add(time.Hour), "限流", true)
	}
	if got := p.PickForModel("deepseek-v4.1-flash", nil); got != nil {
		t.Fatalf("该模型全部受限时应返回 nil，got=%+v", got)
	}
	// 换模型仍应可用
	if got := p.PickForModel("glm-5.3", nil); got == nil {
		t.Fatal("其他模型应仍可选")
	}
}

// TestPickByUIDForModel Sticky 路由：绑定号在该模型上被限流时应返回 nil（触发解绑重分配）。
func TestPickByUIDForModel(t *testing.T) {
	p := New("")
	p.Add(&auth.Auth{UID: "u1"})
	p.SetCredits("u1", 1000)
	p.CooldownModel("u1", "glm-5.3", time.Now().Add(time.Hour), "限流", true)

	if got := p.PickByUIDForModel("u1", "glm-5.3"); got != nil {
		t.Fatalf("该模型受限时粘性命中应失效（返回 nil 让上层解绑），got=%+v", got)
	}
	if got := p.PickByUIDForModel("u1", "deepseek-v4.1-flash"); got == nil {
		t.Fatal("其他模型应仍能粘性命中")
	}
	if got := p.PickByUID("u1"); got == nil {
		t.Fatal("不带模型时应保持原有行为")
	}
}

// TestModelCoolingMultipleModelsSorted 多个模型冷却按到期时间升序输出，便于前端展示。
func TestModelCoolingMultipleModelsSorted(t *testing.T) {
	p := New("")
	p.Add(&auth.Auth{UID: "u1"})
	p.CooldownModel("u1", "later", time.Now().Add(3*time.Hour), "later", true)
	p.CooldownModel("u1", "sooner", time.Now().Add(1*time.Hour), "sooner", true)

	st, _ := p.Status("u1")
	if len(st.ModelCooling) != 2 {
		t.Fatalf("应有 2 条，实际 %+v", st.ModelCooling)
	}
	if st.ModelCooling[0].Model != "sooner" {
		t.Errorf("应按到期时间升序，首条=%q", st.ModelCooling[0].Model)
	}
}

// TestModelRateDoesNotTripBreaker 模型限流不喂熔断器。
//
// 模型级限流是配额信号而非账号故障；喂熔断会把整个账号封掉，
// 与"该账号换模型仍可用"的事实相悖。
func TestModelRateDoesNotTripBreaker(t *testing.T) {
	p := New("")
	p.Add(&auth.Auth{UID: "u1"})
	p.SetBreaker(1, time.Hour, time.Hour) // 阈值 1：任何一次失败都会立刻熔断

	p.CooldownModel("u1", "glm-5.3", time.Now().Add(time.Hour), "限流", true)

	if bt, _ := p.breakerUntil("u1"); !bt.IsZero() {
		t.Errorf("模型冷却不应触发熔断，breaker_until=%v", bt)
	}
	st, _ := p.Status("u1")
	if st.BreakerFails != 0 {
		t.Errorf("模型冷却不应累计 breaker_fails，实际=%d", st.BreakerFails)
	}
}

// TestModelCoolingPersistsAcrossReload 模型冷却必须持久化。
//
// 必要性：模型限流的重置时间常达数小时（现场实测 4.6~6.7h），而网关会因
// 切换工作模式 / 应用重启等原因重启。不持久化的话重启即遗忘，立刻重新撞
// 同一批 6004 —— 用户看到的仍是"频繁不可用"，与 issue 现象一致。
func TestModelCoolingPersistsAcrossReload(t *testing.T) {
	dir := t.TempDir()
	fp := filepath.Join(dir, "state.json")

	p := New(fp)
	p.Add(&auth.Auth{UID: "u1"})
	p.SetCredits("u1", 1000)
	until := time.Now().Add(3 * time.Hour).Truncate(time.Second)
	p.CooldownModel("u1", "deepseek-v4.1-flash", until, "deepseek-v4.1-flash 已达频率上限", true)
	p.Flush()

	// 重载后：冷却仍在，且字段完整。
	p2 := New(fp)
	p2.Add(&auth.Auth{UID: "u1"})
	st, ok := p2.Status("u1")
	if !ok {
		t.Fatal("no status")
	}
	if len(st.ModelCooling) != 1 {
		t.Fatalf("重启后模型冷却丢失：%+v", st.ModelCooling)
	}
	mc := st.ModelCooling[0]
	if mc.Model != "deepseek-v4.1-flash" {
		t.Errorf("model=%q", mc.Model)
	}
	if !mc.Until.Equal(until) {
		t.Errorf("until=%s want %s", mc.Until, until)
	}
	if !mc.ResetAtParsed {
		t.Error("reset_at_parsed 应随冷却一起持久化")
	}
	if mc.Reason == "" {
		t.Error("reason 应随冷却一起持久化")
	}
	// 路由仍然避开该模型
	if got := p2.PickForModel("deepseek-v4.1-flash", nil); got != nil {
		t.Errorf("重载后仍应避开该模型，got=%+v", got)
	}
	if got := p2.PickForModel("glm-5.3", nil); got == nil {
		t.Error("重载后其他模型仍应可用")
	}
}

// TestModelCoolingLegacyStateFileCompatible 旧 state.json（无 model_cools 字段）能正常加载。
func TestModelCoolingLegacyStateFileCompatible(t *testing.T) {
	dir := t.TempDir()
	fp := filepath.Join(dir, "state.json")
	// 旧版文件：只有 credits/until，没有 model_cools
	legacy := `{"accounts":{"u1":{"credits":500,"disabled":false,"until":"0001-01-01T00:00:00Z","cool_kind":0}}}`
	if err := os.WriteFile(fp, []byte(legacy), 0o600); err != nil {
		t.Fatal(err)
	}
	p := New(fp)
	p.Add(&auth.Auth{UID: "u1"})
	st, ok := p.Status("u1")
	if !ok || st.Credits != 500 {
		t.Fatalf("旧文件应能正常加载: %+v ok=%v", st, ok)
	}
	if len(st.ModelCooling) != 0 {
		t.Errorf("旧文件无模型冷却，应为空: %+v", st.ModelCooling)
	}
	if st.Cooling {
		t.Error("旧文件的零值 until 不应被判为冷却中")
	}
}
