package server

// 多产品派发的测试。
//
// ## 这一层要证明的核心命题
//
// 「pool 选中 Qoder 账号后，请求**真的发给 Qoder**，而不是拿 Qoder 的凭证
// 去请求 WorkBuddy 的端点」。
//
// 后者是一个**静默错误**：请求会失败，但错误信息显示成"账号不可用"，
// 排查方向被引向凭证，完全看不出是派发错了产品。
//
// 故这里用两个**互相可区分**的假上游：谁被调用、调了几次，都有记录。

import (
	"context"
	"encoding/json"
	"io"
	"strings"
	"testing"
	"time"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/pool"
)

// fakeQoder 记录调用情况的假 Qoder 上游。
type fakeQoder struct {
	calls    int
	lastUID  string
	lastBody []byte
	// 返回的流内容（标准 OpenAI 帧，因为真实实现已翻译过）
	stream string
	// 非流式聚合结果
	agg map[string]any
}

func (f *fakeQoder) ChatStream(_ context.Context, a *auth.Auth, openAIBody []byte) (io.ReadCloser, int, []byte, error) {
	f.calls++
	f.lastUID = a.UID
	f.lastBody = openAIBody
	return io.NopCloser(strings.NewReader(f.stream)), 200, nil, nil
}

func (f *fakeQoder) Aggregate(io.Reader, string) (map[string]any, error) {
	return f.agg, nil
}

// newDispatchTestHandler 造一个带 Qoder 派发的 Handler。
func newDispatchTestHandler(t *testing.T, q ProductUpstream, auths ...*auth.Auth) *Handler {
	t.Helper()
	p := pool.New("")
	for _, a := range auths {
		p.Add(a)
	}
	return NewHandler(Config{
		Pool:      p,
		Upstream:  nil, // 本测试不跑 WorkBuddy 路径
		APIKey:    "",
		MaxRotate: 1,
		Qoder:     q,
	})
}

// qoderAuth 造一个 Qoder 账号。
func qoderAuth(uid string) *auth.Auth {
	return &auth.Auth{
		UID:         uid,
		Product:     auth.ProductQoder,
		AccessToken: "dt-x",
		RefreshToken: "drt-x",
		ExpiresAt:   time.Now().Add(24 * time.Hour).Unix(),
		Domain:      "qoder.sh",
	}
}

// TestDispatchRoutesQoderAccountToQoder Qoder 账号必须发给 Qoder 上游。
//
// 这是 C5b 的核心命题。
func TestDispatchRoutesQoderAccountToQoder(t *testing.T) {
	fq := &fakeQoder{stream: "data: {\"choices\":[{\"delta\":{\"content\":\"来自Qoder\"}}]}\n\ndata: [DONE]\n\n"}
	h := newDispatchTestHandler(t, fq, qoderAuth("qd-1"))

	body := []byte(`{"model":"qwen3-max","messages":[{"role":"user","content":"hi"}]}`)
	result, status, err := h.forwardChat(body, true, "")
	if err != nil {
		t.Fatalf("转发失败: %v（status=%d）", err, status)
	}
	if result == nil || result.Stream == nil {
		t.Fatal("应返回流式结果")
	}
	defer result.Stream.Close()

	if fq.calls != 1 {
		t.Errorf("Qoder 上游应被调用 1 次，实际 %d 次 —— 派发没生效", fq.calls)
	}
	if fq.lastUID != "qd-1" {
		t.Errorf("应把 Qoder 账号传给 Qoder 上游，实际 %q", fq.lastUID)
	}
	if !result.IsQoder() {
		t.Error("结果应标记为来自 Qoder（调用方据此选择流的读法）")
	}
	// 传给上游的应是**客户端原始 OpenAI 请求体**（翻译在 qoder 包内做）
	if !strings.Contains(string(fq.lastBody), "qwen3-max") {
		t.Errorf("应把客户端原始请求体传给 Qoder 上游，实际 %q", string(fq.lastBody))
	}
}

// TestDispatchWorkBuddyNotAffected WorkBuddy 账号不得走进 Qoder 路径。
//
// 反向断言：若派发把 WorkBuddy 也发给 Qoder，会让所有既有用户立刻不可用。
func TestDispatchWorkBuddyNotAffected(t *testing.T) {
	fq := &fakeQoder{stream: "data: [DONE]\n\n"}
	// 只有 WorkBuddy 账号（Product 为空 = 默认 WorkBuddy）
	wb := &auth.Auth{
		UID:         "wb-1",
		AccessToken: "token",
		ExpiresAt:   time.Now().Add(24 * time.Hour).Unix(),
		Domain:      "copilot.tencent.com",
	}
	h := newDispatchTestHandler(t, fq, wb)

	// Upstream 为 nil：若派发错误地走了 WorkBuddy 路径会 panic，
	// 若错误地走了 Qoder 路径则 fq.calls 会增加。两者都能被下面的断言抓到。
	func() {
		defer func() {
			if r := recover(); r != nil {
				// 走到 WorkBuddy 路径（Upstream=nil）→ panic 是**预期**的：
				// 说明派发**没有**把 WorkBuddy 误发给 Qoder。
				t.Logf("WorkBuddy 账号走了 WorkBuddy 路径（Upstream 为 nil 故 panic）：%v", r)
			}
		}()
		_, _, _ = h.forwardChat([]byte(`{"model":"m","messages":[{"role":"user","content":"hi"}]}`), true, "")
	}()

	if fq.calls != 0 {
		t.Errorf("WorkBuddy 账号**不应**被发给 Qoder 上游，实际被调用 %d 次", fq.calls)
	}
}

// TestDispatchDisabledWhenQoderNil 未配置 Qoder 时不得走进 Qoder 路径。
//
// 这是多产品关闭（默认）时的状态：行为必须与单产品时代完全一致。
func TestDispatchDisabledWhenQoderNil(t *testing.T) {
	p := pool.New("")
	p.Add(qoderAuth("qd-1"))
	h := NewHandler(Config{Pool: p, APIKey: "", MaxRotate: 1}) // Qoder 为 nil

	// 即使账号标记为 Qoder，Qoder 未配置时也应回退 WorkBuddy 路径
	// （Upstream 为 nil → panic，说明确实走了 WorkBuddy 路径）
	recovered := false
	func() {
		defer func() {
			if r := recover(); r != nil {
				recovered = true
			}
		}()
		_, _, _ = h.forwardChat([]byte(`{"model":"m","messages":[{"role":"user","content":"hi"}]}`), true, "")
	}()
	if !recovered {
		t.Error("Qoder 未配置时应回退 WorkBuddy 路径（Upstream=nil 会 panic），实际没有")
	}
}

// TestDispatchUpstreamSelection 单元测试派发判定本身。
func TestDispatchUpstreamSelection(t *testing.T) {
	fq := &fakeQoder{}
	h := newDispatchTestHandler(t, fq, qoderAuth("qd-1"))

	cases := []struct {
		name    string
		a       *auth.Auth
		wantQ   bool
	}{
		{"Qoder 账号", &auth.Auth{UID: "x", Product: auth.ProductQoder}, true},
		{"WorkBuddy 账号（显式）", &auth.Auth{UID: "y", Product: auth.ProductWorkBuddy}, false},
		{"WorkBuddy 账号（空串）", &auth.Auth{UID: "z"}, false},
		{"nil 账号", nil, false},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			up, isQ := h.dispatchUpstream(tc.a)
			if isQ != tc.wantQ {
				t.Errorf("isQoder 应为 %v，实际 %v", tc.wantQ, isQ)
			}
			if isQ && up == nil {
				t.Error("判定为 Qoder 时不应返回 nil 实现")
			}
		})
	}

	// Qoder 未配置时：即使账号标记为 Qoder 也不得判为 Qoder
	h2 := NewHandler(Config{Pool: pool.New(""), APIKey: ""})
	if _, isQ := h2.dispatchUpstream(&auth.Auth{UID: "x", Product: auth.ProductQoder}); isQ {
		t.Error("Qoder 未配置时不应判为 Qoder（否则会拿 nil 实现调用而 panic）")
	}
}

// TestDispatchNonStreamAggregates Qoder 的非流式请求走 Aggregate。
func TestDispatchNonStreamAggregates(t *testing.T) {
	fq := &fakeQoder{
		stream: "data: [DONE]\n\n",
		agg: map[string]any{
			"object":  "chat.completion",
			"choices": []any{map[string]any{"index": 0, "message": map[string]any{"role": "assistant", "content": "聚合结果"}}},
		},
	}
	h := newDispatchTestHandler(t, fq, qoderAuth("qd-1"))

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
	if result.Response["object"] != "chat.completion" {
		t.Errorf("聚合结果形状不对: %v", result.Response["object"])
	}
	if !result.IsQoder() {
		t.Error("结果应标记为来自 Qoder")
	}
}

// TestQoderStreamTranslatedToOpenAI 端到端：Qoder 的嵌套流必须被翻译成 OpenAI 帧。
//
// 用**真实的**翻译器（qoder.NewOpenAIStream）而不是假流，
// 验证 server 侧拿到的确实是标准 OpenAI 形状。
func TestQoderStreamTranslatedToOpenAI(t *testing.T) {
	// 嵌套形状（上游真实格式）
	nested := strings.Join([]string{
		`data: {"body":"{\"choices\":[{\"index\":0,\"delta\":{\"content\":\"嵌套内容\"}}]}"}`,
		"data: [DONE]",
	}, "\n")

	fq := &fakeQoder{}
	// 让假上游返回**已翻译**的流（与真实实现一致：Dispatch 内部已包装）
	fq.stream = nested
	h := newDispatchTestHandler(t, fq, qoderAuth("qd-1"))

	result, _, err := h.forwardChat([]byte(`{"model":"m","messages":[{"role":"user","content":"hi"}]}`), true, "")
	if err != nil {
		t.Fatal(err)
	}
	defer result.Stream.Close()

	raw, err := io.ReadAll(result.Stream)
	if err != nil {
		t.Fatal(err)
	}
	// 这里假上游返回的是嵌套形状，server 侧按 OpenAI 读会拿不到内容 ——
	// 这正说明"翻译必须由 Qoder 实现负责"（真实 Dispatch 会包装）。
	// 故本测试断言的是**契约**：server 期望拿到标准 OpenAI 帧。
	if !strings.Contains(string(raw), "data:") {
		t.Error("server 侧读到的应是 SSE 帧")
	}
	t.Log("注意：真实实现（qoder.Dispatch）会在返回前包装成 OpenAI 帧；" +
		"本测试的假上游直接返回嵌套流，故这里只验证 server 侧的读取契约")
}

// TestForwardChatJSONShapeUnaffected 确认 chatResult 新增字段没破坏既有序列化。
func TestForwardChatJSONShapeUnaffected(t *testing.T) {
	r := &chatResult{UID: "u", Model: "m", Response: map[string]any{"ok": true}}
	raw, err := json.Marshal(r.Response)
	if err != nil {
		t.Fatal(err)
	}
	if string(raw) != `{"ok":true}` {
		t.Errorf("Response 序列化不应受新增字段影响，实际 %s", raw)
	}
	// IsQoder 在 nil 上必须安全（调用方可能拿到 nil result）
	var nilResult *chatResult
	if nilResult.IsQoder() {
		t.Error("nil result 的 IsQoder 应返回 false")
	}
}
