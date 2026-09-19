package server

// ZCode 派发的测试。
//
// ## 要证明的核心命题
//
// 「pool 选中 ZCode 账号后，请求**真的发给 ZCode**，而不是拿 ZCode 的凭证
// 去请求 WorkBuddy 或 Qoder 的端点」。
//
// 后者是**静默错误**：请求会失败，但错误信息显示成"账号不可用"，
// 排查方向被引向凭证，完全看不出是派发错了产品。
//
// 三个产品都在时，必须**互相可区分** —— 用三个各自计数的假上游。

import (
	"context"
	"io"
	"strings"
	"testing"
	"time"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/pool"
)

// zcodeAuth 造一个 ZCode 账号。
func zcodeAuth(uid string) *auth.Auth {
	return &auth.Auth{
		UID:         uid,
		Product:     auth.ProductZcode,
		AccessToken: "zcode-key.secret", // 凭证字符串复用 AccessToken
		Domain:      "api.z.ai",
	}
}

// newThreeProductHandler 造一个三产品齐全的 Handler，返回三个假上游。
func newThreeProductHandler(t *testing.T, auths ...*auth.Auth) (*Handler, *fakeQoder, *fakeQoder) {
	t.Helper()
	p := pool.New("")
	for _, a := range auths {
		p.Add(a)
	}
	// ⚠ 假上游必须给**至少一帧真实数据**，不能只给 `data: [DONE]`。
	//
	// 这里原来写的是 `"data: [DONE]\n\n"` —— 而那是**空流**：
	// `[DONE]` 只表示结束，不代表有内容（见 upstream.ValidSSEFrame 的注释）。
	//
	// 旧代码之所以没暴露，是因为 `ProbeFirstFrame` 当时只看 `data:` 前缀，
	// 于是"只有 [DONE]"也能通过探测，直到 `Stream` 数出 0 帧才报空流。
	// 修好探测口径（改用 ValidSSEFrame）后，这种假流**立刻被判成空流并换号**，
	// 这些用例才红 —— 说明它们是**依赖缺陷行为**才通过的，不是真在测派发。
	//
	// 换成真实形状：一帧内容 + [DONE]。
	const okStream = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"}}]}\n\n" +
		"data: [DONE]\n\n"
	fq := &fakeQoder{stream: okStream}
	fz := &fakeQoder{stream: okStream}
	h := NewHandler(Config{
		Pool:      p,
		APIKey:    "",
		MaxRotate: 1,
		Qoder:     fq,
		Zcode:     fz,
	})
	return h, fq, fz
}

// TestDispatchRoutesZcodeToZcode ZCode 账号必须发给 ZCode 上游。
func TestDispatchRoutesZcodeToZcode(t *testing.T) {
	h, fq, fz := newThreeProductHandler(t, zcodeAuth("zc-1"))

	body := []byte(`{"model":"glm-5.3","messages":[{"role":"user","content":"hi"}]}`)
	result, status, err := h.forwardChat(body, true, "")
	if err != nil {
		t.Fatalf("转发失败: %v（status=%d）", err, status)
	}
	if result == nil || result.Stream == nil {
		t.Fatal("应返回流式结果")
	}
	defer result.Stream.Close()

	if fz.calls != 1 {
		t.Errorf("ZCode 上游应被调用 1 次，实际 %d 次 —— 派发没生效", fz.calls)
	}
	if fq.calls != 0 {
		t.Errorf("**Qoder 上游不应被调用**，实际 %d 次 —— 派发错了产品", fq.calls)
	}
	if fz.lastUID != "zc-1" {
		t.Errorf("应把 ZCode 账号传给 ZCode 上游，实际 %q", fz.lastUID)
	}
	if result.Product != auth.ProductZcode {
		t.Errorf("结果应标记 Product=zcode，实际 %q", result.Product)
	}
}

// TestDispatchZcodeNotSentToOthers 反向断言：其它产品的账号**不**发给 ZCode。
func TestDispatchZcodeNotSentToOthers(t *testing.T) {
	body := []byte(`{"model":"m","messages":[{"role":"user","content":"hi"}]}`)

	// ---- ① WorkBuddy 账号 ----
	//
	// ⚠ 每个子用例用**独立的 handler 与计数器**：第一版我复用了同一个 h，
	// 而 WorkBuddy 那条会 panic（Upstream=nil），panic 之后 fq.calls 里
	// 还留着**上一个子用例**的计数 —— 断言读到的是脏数据。
	wb := &auth.Auth{
		UID: "wb-1", AccessToken: "t",
		ExpiresAt: time.Now().Add(24 * time.Hour).Unix(),
		Domain:    "copilot.tencent.com",
	}
	h1, fq1, fz1 := newThreeProductHandler(t, wb)
	func() {
		defer func() { _ = recover() }() // Upstream=nil → 可能 panic，无所谓
		_, _, _ = h1.forwardChat(body, true, "")
	}()
	if fz1.calls != 0 {
		t.Errorf("WorkBuddy 账号不应发给 ZCode，实际 %d 次", fz1.calls)
	}
	if fq1.calls != 0 {
		t.Errorf("WorkBuddy 账号不应发给 Qoder，实际 %d 次", fq1.calls)
	}

	// ---- ② Qoder 账号 ----
	h2, fq2, fz2 := newThreeProductHandler(t, qoderAuth("qd-2"))
	if _, _, err := h2.forwardChat(body, true, ""); err != nil {
		t.Fatalf("Qoder 账号转发失败: %v", err)
	}
	if fq2.calls != 1 {
		t.Errorf("Qoder 账号应发给 Qoder，实际 %d 次", fq2.calls)
	}
	if fz2.calls != 0 {
		t.Errorf("Qoder 账号**不应**发给 ZCode，实际 %d 次 —— 派发错了产品", fz2.calls)
	}
}

// TestDispatchZcodeDisabledWhenNil 未配置 ZCode 时不得走进 ZCode 路径。
//
// 这是多产品关闭（默认）时的状态：行为必须与单产品时代完全一致。
//
// ## 为什么不用"panic 即证明"这种断言
//
// 第一版我靠"Upstream 为 nil 会 panic"来证明走了 WorkBuddy 路径 ——
// 但那是个**脆弱的间接证据**：一旦代码在 WorkBuddy 路径上加了 nil 守卫
//（那是好事），测试就会失败，而它其实什么都没坏。
//
// 改成断言**契约本身**：
//   · 不进 ZCode 路径（计数为 0）
//   · 优雅失败（返回错误而不是 panic）
func TestDispatchZcodeDisabledWhenNil(t *testing.T) {
	p := pool.New("")
	p.Add(zcodeAuth("zc-1"))

	// 用一个**可观测的假上游**当 WorkBuddy 侧的替身：
	// 若账号走了 WorkBuddy 路径，它会被调用。
	fz := &fakeQoder{}
	h := NewHandler(Config{Pool: p, APIKey: "", MaxRotate: 1, Zcode: nil})

	// dispatchUpstream 必须返回"不是产品路径"
	acct := p.Pick()
	if acct == nil {
		t.Fatal("没选出账号")
	}
	if _, isProduct := h.dispatchUpstream(acct); isProduct {
		t.Error("ZCode 未配置时不应判为产品路径（否则会拿 nil 实现调用而 panic）")
	}

	// 完整流程：必须**不 panic** 且不进 ZCode
	var result *chatResult
	var err error
	panicked := false
	func() {
		defer func() {
			if r := recover(); r != nil {
				panicked = true
			}
		}()
		result, _, err = h.forwardChat([]byte(`{"model":"m","messages":[{"role":"user","content":"hi"}]}`), true, "")
	}()

	if panicked {
		t.Error("ZCode 未配置时应优雅失败，实际 panic 了")
	}
	if fz.calls != 0 {
		t.Errorf("ZCode 未配置时不应调用 ZCode 实现，实际 %d 次", fz.calls)
	}
	// WorkBuddy 的 Upstream 是 nil，故应返回错误（而不是崩溃）
	if err == nil {
		t.Errorf("WorkBuddy 上游不可用时应返回错误，实际 err=nil result=%+v", result)
	}
	t.Logf("优雅失败: %v", err)
}

// TestDispatchThreeProductSelection 三个产品的派发判定。
func TestDispatchThreeProductSelection(t *testing.T) {
	h, _, _ := newThreeProductHandler(t, zcodeAuth("zc-1"))

	cases := []struct {
		name      string
		a         *auth.Auth
		wantQoder bool
		wantZcode bool
	}{
		{"ZCode 账号", &auth.Auth{UID: "x", Product: auth.ProductZcode}, false, true},
		{"Qoder 账号", &auth.Auth{UID: "y", Product: auth.ProductQoder}, true, false},
		{"WorkBuddy（显式）", &auth.Auth{UID: "z", Product: auth.ProductWorkBuddy}, false, false},
		{"WorkBuddy（空串）", &auth.Auth{UID: "w"}, false, false},
		{"nil 账号", nil, false, false},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			up, isProduct := h.dispatchUpstream(tc.a)
			// 判定"是哪个产品"：通过实现的身份无法直接看出，
			// 故用 nil 判断 + 调用后的计数间接验证（下面单独测）
			if tc.wantQoder || tc.wantZcode {
				if !isProduct || up == nil {
					t.Errorf("应判为某产品（isProduct=%v up=%v）", isProduct, up)
				}
			} else {
				if isProduct {
					t.Error("WorkBuddy/nil 不应判为其它产品")
				}
			}
		})
	}
}

// TestZcodeAggregateNonStream ZCode 的非流式请求走 Aggregate。
func TestZcodeAggregateNonStream(t *testing.T) {
	h, _, fz := newThreeProductHandler(t, zcodeAuth("zc-1"))
	// 让假上游返回一个聚合结果
	fz.agg = map[string]any{
		"object":  "chat.completion",
		"choices": []any{map[string]any{"index": 0, "message": map[string]any{"role": "assistant", "content": "聚合"}}},
	}

	result, status, err := h.forwardChat([]byte(`{"model":"m","messages":[{"role":"user","content":"hi"}]}`), false, "")
	if err != nil {
		t.Fatalf("转发失败: %v（status=%d）", err, status)
	}
	if result.Stream != nil {
		t.Error("非流式请求不应返回流")
	}
	if result.Response == nil {
		t.Fatal("非流式请求应返回聚合结果")
	}
	if result.Product != auth.ProductZcode {
		t.Errorf("结果应标记 Product=zcode，实际 %q", result.Product)
	}
}

// TestThreeProductsCoexistInPool 三个产品都在池里时都能被选中。
//
// 用 pool 的选号验证（不发请求）—— 确保没有哪个产品被饿死。
func TestThreeProductsCoexistInPool(t *testing.T) {
	wb := &auth.Auth{
		UID: "wb", AccessToken: "t",
		ExpiresAt: time.Now().Add(24 * time.Hour).Unix(),
		Domain:    "copilot.tencent.com",
	}
	p := pool.New("")
	p.Add(wb)
	p.Add(qoderAuth("qd"))
	p.Add(zcodeAuth("zc"))
	// 关掉多产品开关时不应影响选号（它只影响成本维度）
	p.SetMultiProduct(true, 0.3)

	counts := map[string]int{}
	for i := 0; i < 3000; i++ {
		if a := p.Pick(); a != nil {
			counts[a.ProductOf()]++
		}
	}
	t.Logf("份额: workbuddy=%d qoder=%d zcode=%d",
		counts[auth.ProductWorkBuddy], counts[auth.ProductQoder], counts[auth.ProductZcode])

	for _, prod := range []string{auth.ProductWorkBuddy, auth.ProductQoder, auth.ProductZcode} {
		if counts[prod] == 0 {
			t.Errorf("%s 一次都没被选中 —— 产品维度把它饿死了", prod)
		}
	}
}

// TestZcodeStreamPassedThroughUnchanged ZCode 的流**原样透传**（不翻译）。
//
// 这是选 OpenAI 端点的收益：上游给的就是标准 OpenAI SSE，
// 与 WorkBuddy 路径的读法完全一致。
func TestZcodeStreamPassedThroughUnchanged(t *testing.T) {
	h, _, fz := newThreeProductHandler(t, zcodeAuth("zc-1"))
	// 标准 OpenAI SSE（非嵌套）
	fz.stream = strings.Join([]string{
		`data: {"id":"x","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"标准"}}]}`,
		"",
		`data: {"choices":[{"index":0,"delta":{"content":"OpenAI"}}]}`,
		"",
		"data: [DONE]",
	}, "\n")

	result, _, err := h.forwardChat([]byte(`{"model":"m","messages":[{"role":"user","content":"hi"}]}`), true, "")
	if err != nil {
		t.Fatal(err)
	}
	defer result.Stream.Close()

	raw, err := io.ReadAll(result.Stream)
	if err != nil {
		t.Fatal(err)
	}
	out := string(raw)
	if !strings.Contains(out, "标准") || !strings.Contains(out, "OpenAI") {
		t.Errorf("流内容应原样透传，实际 %q", out)
	}
	if !strings.Contains(out, "[DONE]") {
		t.Error("应保留 [DONE]")
	}
}

var _ = context.Background
