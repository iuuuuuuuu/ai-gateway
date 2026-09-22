package server

// unusual_test.go —— 钉住 3012「unusual activity」的熔断。
//
// # 所有者的要求（2026-09-21）
//
//	「2」  ← 在"什么都不做等恢复"与"加自动退避"之间，他选了后者
//
// 要达成的行为：**3012 之后不再换号重试**。
//
// # 旧行为为什么是错的
//
// 3012 的 HTTP 载体是 405，而 `upstream.Classify` 里 405 落进通用
// `ErrClient`；`applyErrorPolicy` 对 `ErrClient` 是"只换号不罚"。
// 于是网关会把池里 **21 个账号挨个打一遍**，每个都吃一次 3012。
//
// 对上游风控而言这是最坏形状：同一出口 IP 在极短时间内用一批不同凭证
// 反复触发同一规则。这与 wafip.go 记的那条同构。
//
// 本文件锁三层：
//
//	① 判据（isUnusualActivity）—— 认得出 3012，且不误伤正常回复
//	② 状态机（unusualBreaker）—— 熔断/延长/复位/窗口过期的语义
//	③ 文案 —— 必须说清"与账号无关、换号无用"，否则用户会去查账号池

import (
	"net/http"
	"strings"
	"testing"
	"time"

	"workbuddy2api/internal/auth"
)

// ---- ① 判据 ----

func TestIsUnusualActivityRecognisesRealResponse(t *testing.T) {
	// 实测原文（zcode/provider.go 与 captcha.go 都记着这一条）
	body := `{"code":3012,"msg":"request has been blocked due to unusual activity."}`
	if !isUnusualActivity(405, body) {
		t.Error("实测的 3012 原文应被识别")
	}
}

func TestIsUnusualActivityMarkerForms(t *testing.T) {
	cases := []struct {
		name   string
		status int
		body   string
		want   bool
		why    string
	}{
		{"原样", 405, `{"code":3012,"msg":"request has been blocked due to unusual activity."}`, true, "实测形态"},
		{"只有文案", 405, `{"msg":"Request has been blocked due to unusual activity."}`, true, "code 可能缺失"},
		{"大写文案", 403, `{"message":"UNUSUAL ACTIVITY detected"}`, true, "大小写不敏感"},
		{"code 是字符串", 405, `{"code":"3012","msg":"blocked"}`, true, "某些通道把码序列化"},
		{"带空格", 405, `{"code": 3012, "msg":"blocked"}`, true, "上游可能美化输出"},

		// ---- 不该误判的 ----
		{"正常回复", 200, `{"choices":[{"message":{"content":"3012 端口"}}]}`, false,
			"HTTP 200 的正文里恰好含 3012 —— 不该熔断"},
		{"别的业务码", 400, `{"code":30120,"msg":"other"}`, false,
			"30120 不是 3012（裸 Contains 会误命中）"},
		{"token 计数含 3012", 200, `{"usage":{"prompt_tokens":3012}}`, false,
			"用量数字恰好是 3012"},
		{"普通 4xx", 400, `{"code":1001,"msg":"bad request"}`, false, "无关错误"},
		{"空体", 405, ``, false, "空体没有 3012 证据（405 本身不足以判定）"},
	}
	for _, c := range cases {
		if got := isUnusualActivity(c.status, c.body); got != c.want {
			t.Errorf("%s：isUnusualActivity(%d, %q) = %v，期望 %v（%s）",
				c.name, c.status, c.body, got, c.want, c.why)
		}
	}
}

// TestIsUnusualActivityIgnoresSuccessStatus 成功状态码一律不判。
//
// 用户在对话里聊"3012"是完全可能的；把那种正常回复判成风控会让网关
// 无故熔断（表现为"聊天聊到一半突然全不可用"）。
func TestIsUnusualActivityIgnoresSuccessStatus(t *testing.T) {
	for _, s := range []int{200, 201, 204, 301, 302} {
		if isUnusualActivity(s, `{"code":3012,"msg":"unusual activity"}`) {
			t.Errorf("状态码 %d 不该触发熔断", s)
		}
	}
}

// ---- ② 状态机 ----

func newTestBreaker(t *testing.T) (*unusualBreaker, *time.Time) {
	t.Helper()
	now := time.Now()
	cur := &now
	b := newUnusualBreaker()
	b.now = func() time.Time { return *cur }
	t.Cleanup(b.reset)
	return b, cur
}

func TestBreakerTripsAndExpires(t *testing.T) {
	b, cur := newTestBreaker(t)

	if tripped, _ := b.tripped(); tripped {
		t.Fatal("初始不该熔断")
	}

	until := b.note3012()
	if tripped, wait := b.tripped(); !tripped {
		t.Fatal("命中 3012 后应熔断")
	} else if wait <= 0 {
		t.Errorf("熔断剩余时长应为正，实际 %v", wait)
	}
	if until.Before(*cur) {
		t.Error("截止时刻应晚于当前时刻")
	}

	// 过了窗口 ⇒ 自动恢复（不靠任何人工干预）
	*cur = cur.Add(unusualWindow + time.Second)
	if tripped, _ := b.tripped(); tripped {
		t.Error("窗口过后应自动恢复 —— 3012 是临时的，不能变成我们自己造成的永久故障")
	}
}

func TestBreakerEscalatesOnRepeatedHits(t *testing.T) {
	b, cur := newTestBreaker(t)

	b.note3012()
	first := b.window

	// 窗口内再次命中 ⇒ 窗口翻倍（逐步退让，而不是每 90 秒规律叩门）
	*cur = cur.Add(10 * time.Second)
	b.note3012()
	if b.window <= first {
		t.Errorf("连续命中应放大窗口：首次 %v，再次 %v", first, b.window)
	}
}

func TestBreakerResetsWindowAfterExpiry(t *testing.T) {
	b, cur := newTestBreaker(t)

	// 连续命中把窗口顶到上限
	for i := 0; i < 12; i++ {
		*cur = cur.Add(time.Second)
		b.note3012()
	}
	if b.window <= unusualWindow {
		t.Fatalf("前置条件不成立：窗口应已放大，实际 %v", b.window)
	}

	// 窗口**过期之后**才再次命中 ⇒ 这是"新一轮风控"，窗口回到基准
	*cur = cur.Add(b.window + time.Minute)
	b.note3012()
	if b.window != unusualWindow {
		t.Errorf("过期后重新命中应把窗口复位到基准 %v，实际 %v —— "+
			"否则一次长风控会把窗口永久顶在上限，之后每次瞬时抖动都要用户等很久",
			unusualWindow, b.window)
	}
}

func TestBreakerWindowCapped(t *testing.T) {
	b, cur := newTestBreaker(t)
	for i := 0; i < 40; i++ {
		*cur = cur.Add(time.Second)
		b.note3012()
	}
	if b.window > unusualMaxWindow {
		t.Errorf("窗口不得超过上限 %v，实际 %v", unusualMaxWindow, b.window)
	}
}

func TestBreakerSuccessClears(t *testing.T) {
	b, _ := newTestBreaker(t)
	b.note3012()
	if tripped, _ := b.tripped(); !tripped {
		t.Fatal("前置条件：应已熔断")
	}

	b.noteSuccess()
	if tripped, _ := b.tripped(); tripped {
		t.Error("上游成功应立刻清除熔断 —— 否则用户在一次短暂抖动后仍被自己网关挡在门外")
	}
	if b.window != unusualWindow {
		t.Errorf("成功应把窗口复位到基准，实际 %v", b.window)
	}
}

// ---- ③ 文案 ----

// TestUnusualMessageSaysNotAccount 文案必须说清"与账号无关、换号无用"。
//
// # 为什么单列一条
//
// 历史上这个错误被包装成 `no_healthy_account` +
// 「all accounts unavailable (cooling/disabled)」，把所有者引向
// "去查账号池 / 换账号"这个**完全无效**的方向。
// 文案里少一句话，用户就会走错路 —— 所以它值得一条测试。
func TestUnusualMessageSaysNotAccount(t *testing.T) {
	msg := unusualActivityMessage(90 * time.Second)

	for _, want := range []string{"3012", "与账号无关", "停止换号"} {
		if !strings.Contains(msg, want) {
			t.Errorf("文案应含 %q，实际：%s", want, msg)
		}
	}
	// 必须给出剩余秒数（用户据此决定等还是换模型）
	if !strings.Contains(msg, "90") {
		t.Errorf("文案应含等待秒数 90，实际：%s", msg)
	}
	// 不该说成封号 —— 那会把用户引向"换账号"
	for _, bad := range []string{"账号被封", "封禁", "账号不可用"} {
		if strings.Contains(msg, bad) {
			t.Errorf("文案不该出现 %q（会误导用户去换账号），实际：%s", bad, msg)
		}
	}
}

func TestUnusualMessageRoundsUpSubSecond(t *testing.T) {
	// 剩余不足 1 秒时不能显示"约 0 秒"（看起来像没生效）
	msg := unusualActivityMessage(200 * time.Millisecond)
	if strings.Contains(msg, "约 0 秒") {
		t.Errorf("不足 1 秒应向上取整为 1，实际：%s", msg)
	}
}

// ---- ④ 端到端：熔断必须**真的**止住换号 ----

// resetUnusual 清空全局熔断（隔离用例之间的状态）。
func resetUnusual(t *testing.T) {
	t.Helper()
	unusual.reset()
	t.Cleanup(unusual.reset)
}

// TestUnusualActivityStopsRotation 第一个账号吃到 3012 ⇒ **立刻停**，不再换号。
//
// # 这是所有者要的核心效果
//
// 他的选择是「3012 后自动退避一段时间不重试」，而旧行为是：
//
//	3012（405）→ Classify 判成通用 ErrClient → applyErrorPolicy 的 default
//	分支「只换号不罚」→ **把池里所有账号挨个打一遍**，每个都吃一次 3012
//
// 用例里放 3 个账号、MaxRotate=3（正是旧行为会全部打完的配置），
// 断言上游只被打了 **1 次**。
func TestUnusualActivityStopsRotation(t *testing.T) {
	resetUnusual(t)
	withRotateRand(t, func(n int64) int64 { return 0 })

	calls := 0
	up := newFakeUpstream(t, func(authz string) (int, string, bool) {
		calls++
		// 实测原文：HTTP 405 + 标准业务信封
		return 405, `{"code":3012,"msg":"request has been blocked due to unusual activity."}`, false
	})
	p := testPoolWith(
		&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999},
		&auth.Auth{UID: "u2", AccessToken: "at2", ExpiresAt: 9999999999},
		&auth.Auth{UID: "u3", AccessToken: "at3", ExpiresAt: 9999999999},
	)
	p.SetCredits("u1", 3000)
	p.SetCredits("u2", 2000)
	p.SetCredits("u3", 1000)
	h := NewHandler(Config{Pool: p, Upstream: up, MaxRotate: 3, SoftCooldown: time.Minute})

	req, rec := chatReq(`{"model":"glm-5.2","messages":[]}`)
	h.ServeHTTP(rec, req)

	if calls != 1 {
		t.Errorf("上游被打了 %d 次，期望 **1** 次 —— 3012 与账号无关，"+
			"换号只会让上游看到更多凭证被打（正是要消除的放大行为）", calls)
	}
	if rec.Code != http.StatusServiceUnavailable {
		t.Errorf("code=%d，期望 503", rec.Code)
	}
	body := rec.Body.String()
	if !strings.Contains(body, "unusual_activity") {
		t.Errorf("应回 unusual_activity 错误码（不能是 no_healthy_account —— "+
			"那会把用户引向查账号池），body=%s", body)
	}
	if !strings.Contains(body, "与账号无关") || !strings.Contains(body, "停止换号") {
		t.Errorf("文案应说明「与账号无关」与「停止换号」，body=%s", body)
	}

	// ⚠ 账号是好的：3012 与账号无关，绝不能冷却/禁用任何账号。
	// 罚号会让好账号在风控过去后仍被冷却，把"上游临时风控"变成
	// "我们自己造成的持续故障"。
	for _, uid := range []string{"u1", "u2", "u3"} {
		st, ok := p.Status(uid)
		if !ok {
			t.Fatalf("账号 %s 不在池里", uid)
		}
		if st.Disabled {
			t.Errorf("账号 %s 被禁用了 —— 3012 与账号无关", uid)
		}
		if st.Cooling {
			t.Errorf("账号 %s 被冷却了（%s）—— 3012 与账号无关", uid, st.CoolKind)
		}
	}
}

// TestUnusualActivityBreakerBlocksNextRequest 熔断期间**下一个请求**不再打上游。
//
// 与上一条互补：那条证明"一次请求内不换号"，这条证明"后续请求也不打"
//（否则用户重试一次就又把账号打一遍，放大依旧）。
func TestUnusualActivityBreakerBlocksNextRequest(t *testing.T) {
	resetUnusual(t)
	withRotateRand(t, func(n int64) int64 { return 0 })

	calls := 0
	up := newFakeUpstream(t, func(authz string) (int, string, bool) {
		calls++
		return 405, `{"code":3012,"msg":"request has been blocked due to unusual activity."}`, false
	})
	p := testPoolWith(
		&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999},
		&auth.Auth{UID: "u2", AccessToken: "at2", ExpiresAt: 9999999999},
	)
	p.SetCredits("u1", 3000)
	p.SetCredits("u2", 2000)
	h := NewHandler(Config{Pool: p, Upstream: up, MaxRotate: 2, SoftCooldown: time.Minute})

	req1, rec1 := chatReq(`{"model":"glm-5.2","messages":[]}`)
	h.ServeHTTP(rec1, req1)
	after1 := calls

	req2, rec2 := chatReq(`{"model":"glm-5.2","messages":[]}`)
	h.ServeHTTP(rec2, req2)

	if calls != after1 {
		t.Errorf("熔断期间的第二个请求仍打了上游（%d → %d 次）—— "+
			"用户重试一次就又把账号打一遍，放大依旧", after1, calls)
	}
	if rec2.Code != http.StatusServiceUnavailable {
		t.Errorf("熔断期应回 503，实际 %d", rec2.Code)
	}
	if !strings.Contains(rec2.Body.String(), "unusual_activity") {
		t.Errorf("第二个请求也应回 unusual_activity，body=%s", rec2.Body.String())
	}
}

// TestUnusualActivityBusiness405StillRotates 普通 405 仍按原语义换号。
//
// 关键回归保护：本特性只该拦 **3012**。上游还有别的 405（方法不允许等），
// 把它一起熔断会让一个请求侧的小毛病冻结整个网关 90 秒 ——
// 而且表现为"偶发地一大段时间全都不可用"，用户完全看不出原因。
func TestUnusualActivityBusiness405StillRotates(t *testing.T) {
	resetUnusual(t)
	withRotateRand(t, func(n int64) int64 { return 0 })

	calls := 0
	up := newFakeUpstream(t, func(authz string) (int, string, bool) {
		calls++
		return 405, `{"code":9999,"msg":"method not allowed"}`, false
	})
	p := testPoolWith(
		&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999},
		&auth.Auth{UID: "u2", AccessToken: "at2", ExpiresAt: 9999999999},
	)
	p.SetCredits("u1", 3000)
	p.SetCredits("u2", 2000)
	h := NewHandler(Config{Pool: p, Upstream: up, MaxRotate: 2, SoftCooldown: time.Minute})

	req, rec := chatReq(`{"model":"glm-5.2","messages":[]}`)
	h.ServeHTTP(rec, req)

	if calls < 2 {
		t.Errorf("普通 405 不该触发熔断，应继续换号（实际只打了 %d 次）", calls)
	}
	if strings.Contains(rec.Body.String(), "unusual_activity") {
		t.Error("普通 405 不该回 unusual_activity")
	}
	if tripped, _ := unusual.tripped(); tripped {
		t.Error("普通 405 不该置位熔断")
	}
}

// TestUnusualActivitySuccessClearsBreaker 上游恢复后请求应能正常通过。
//
// # 为什么这条必须有
//
// 熔断若不能自动解除，就把"上游临时风控"变成了**我们自己造成的永久故障**——
// 用户会看到"明明已经好了，网关还是说在风控"。这正是要避免的次生问题。
func TestUnusualActivitySuccessClearsBreaker(t *testing.T) {
	resetUnusual(t)
	withRotateRand(t, func(n int64) int64 { return 0 })

	// 先真熔断一次
	unusual.note3012()
	if tripped, _ := unusual.tripped(); !tripped {
		t.Fatal("前置条件：应已熔断")
	}
	// 上游此刻已恢复：回一个**真实的 SSE 首帧**。
	//
	// ⚠ 不能回空体 + isStream=true（我第一版这么写，测试直接 panic）：
	// 那条路会走 forward.go 的"空流探测"分支，而空流在探针里是**失败**路径，
	// 与"上游已恢复"的前提相反。要模拟成功就得给出真的数据帧。
	up := newFakeUpstream(t, func(authz string) (int, string, bool) {
		return 200, "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n", true
	})
	p := testPoolWith(&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999})
	p.SetCredits("u1", 3000)
	h := NewHandler(Config{Pool: p, Upstream: up, MaxRotate: 1, SoftCooldown: time.Minute})

	// 复位熔断（模拟"窗口已过"），再发请求 —— 应能成功
	unusual.noteSuccess()
	req, rec := chatReq(`{"model":"glm-5.2","messages":[]}`)
	h.ServeHTTP(rec, req)

	if rec.Code >= 500 {
		t.Errorf("上游已恢复时不该再失败（code=%d），body=%s", rec.Code, rec.Body.String())
	}
	if tripped, _ := unusual.tripped(); tripped {
		t.Error("成功之后熔断应保持清除状态")
	}
}
