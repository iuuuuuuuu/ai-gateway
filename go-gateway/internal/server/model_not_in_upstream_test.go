package server

// 上游**根本没有这个模型**时的处理契约（2026-09-28）。
//
// # 现场（本错误的存在理由）
//
// 用户配了 `qoder:deepseek-v4.1-flash`，而该模型在 Qoder 两个区域的清单里
// **都不存在**。Qoder 上游对不认识的模型 key **不报错**：静默回退到免费通道、
// HTTP 200 正常返回内容 ⇒ 用量统计里出现「deepseek-v4.1-flash · Qoder」的
// **幽灵数据**（那个模型不属于它）。
//
// # 三条契约（与 model_not_in_region_test.go / effort_rejected_test.go 同一套）
//
//	① 状态码用 **400**（请求侧错误，等多久都不会出现），不是 503
//	② 错误码单独成型（model_not_in_upstream），让客户端能识别
//	③ 消息说清"上游没有这个模型"并给出出路（换成上游真实 key），
//	   且**不列白名单** —— 白名单改不了它（与 model_not_in_product 的区别）
//
// ★ 另有一条**核心**约定：**不轮转、不罚账号**。
//
// 这是请求侧错误：同一个请求体发给池里任何账号，上游清单都一样没有它。
// 轮转只是把同一个请求对着每个账号重传一遍；罚号会让好账号被冷却 ——
// 用户改好模型名之后发现"账号又挂了"，故障面被我们自己放大。
import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/qoder"
)

// fakeQoderModelMissing 一个"上游说没有这个模型"的假 Qoder 上游。
//
// 与 dispatch_test.go 的 fakeQoder 的区别：那个永远返回成功流，
// 这个**必定返回** qoder.ModelNotFoundError（真实实现里由清单校验产生）。
type fakeQoderModelMissing struct {
	calls   int
	lastUID string
	// err 返回的错误；nil 时用默认的"上游没有该模型"。
	err error
	// model / available 默认错误里带的模型名与可用清单。
	model     string
	available []string
}

func (f *fakeQoderModelMissing) ChatStream(_ context.Context, a *auth.Auth, _ []byte) (io.ReadCloser, int, []byte, error) {
	f.calls++
	f.lastUID = a.UID
	if f.err != nil {
		return nil, 0, nil, f.err
	}
	return nil, 0, nil, &qoder.ModelNotFoundError{
		Model:     f.model,
		Region:    qoder.RegionCN,
		Available: f.available,
	}
}

func (f *fakeQoderModelMissing) Aggregate(io.Reader, string) (map[string]any, error) {
	return nil, nil
}

// newModelMissingHandler 造一个 3 个 Qoder 账号的 Handler。
//
// MaxRotate=3 是刻意的：若该错误仍走轮转路径，上游会被调 3 次 ——
// 用例据此断言"没有轮转"。
func newModelMissingHandler(t *testing.T, up ProductUpstream) *Handler {
	t.Helper()
	p := testPoolWith(
		qoderAuth("qd-1"),
		qoderAuth("qd-2"),
		qoderAuth("qd-3"),
	)
	return NewHandler(Config{
		Pool:      p,
		Upstream:  nil, // 本测试不跑 WorkBuddy 路径
		APIKey:    "",
		MaxRotate: 3,
		Qoder:     up,
	})
}

// TestModelNotInUpstreamNoRotate 请求侧错误**不该**对着每个账号重传一遍。
func TestModelNotInUpstreamNoRotate(t *testing.T) {
	fq := &fakeQoderModelMissing{model: "deepseek-v4.1-flash", available: []string{"dfmodel"}}
	h := newModelMissingHandler(t, fq)

	body := []byte(`{"model":"qoder:deepseek-v4.1-flash","messages":[{"role":"user","content":"hi"}]}`)
	_, status, err := h.forwardChat(body, true, "")

	if err == nil {
		t.Fatal("上游没有该模型时必须返回错误")
	}
	if status != http.StatusBadRequest {
		t.Errorf("状态码应为 400，实际 %d —— 503 会让客户端以为「稍后重试就好」，"+
			"而这里等多久都不会出现", status)
	}
	if fq.calls != 1 {
		t.Errorf("上游应只被调 1 次（请求侧错误，轮转无意义），实际 %d 次 —— "+
			"说明这个错误仍走了轮转路径，会把同一个请求对着每个账号重传", fq.calls)
	}
}

// TestModelNotInUpstreamDoesNotPenalizeAccount ★ 核心契约：**不罚账号**。
//
// 这是请求侧错误，账号完全无辜。罚号会让好账号被冷却 ——
// 用户改好模型名之后发现"账号又挂了"。
func TestModelNotInUpstreamDoesNotPenalizeAccount(t *testing.T) {
	fq := &fakeQoderModelMissing{model: "deepseek-v4.1-flash", available: []string{"dfmodel"}}
	h := newModelMissingHandler(t, fq)

	body := []byte(`{"model":"qoder:deepseek-v4.1-flash","messages":[{"role":"user","content":"hi"}]}`)
	result, _, err := h.forwardChat(body, true, "")
	if err == nil {
		t.Fatal("上游没有该模型时必须返回错误")
	}
	if result == nil || result.UID == "" {
		t.Fatal("失败结果里应带 UID（调用方据此记录日志与展示）")
	}

	// 被选中的那个账号必须**毫发无损**
	st, ok := h.cfg.Pool.Status(result.UID)
	if !ok {
		t.Fatalf("池里应有账号 %q", result.UID)
	}
	if st.Cooling {
		t.Errorf("账号 %q 被冷却了 —— 这是**请求侧**错误（模型名不存在），"+
			"账号无辜；罚号会让用户改好模型名之后发现「账号又挂了」", result.UID)
	}
	if st.ErrTotal != 0 {
		t.Errorf("账号 %q 被记了错误（ErrTotal=%d）—— 请求侧错误不该记账，"+
			"否则好账号会被逐步冷却", result.UID, st.ErrTotal)
	}
	if st.Disabled {
		t.Errorf("账号 %q 被禁用了 —— 请求侧错误绝不该停用账号", result.UID)
	}
}

// TestModelNotInUpstreamStatusAndCode 走完整 HTTP 链路：400 + 独立错误码。
func TestModelNotInUpstreamStatusAndCode(t *testing.T) {
	fq := &fakeQoderModelMissing{model: "deepseek-v4.1-flash", available: []string{"dfmodel"}}
	h := newModelMissingHandler(t, fq)

	body := `{"model":"qoder:deepseek-v4.1-flash","messages":[{"role":"user","content":"hi"}]}`
	req := httptest.NewRequest(http.MethodPost, "/v1/chat/completions", strings.NewReader(body))
	req.Header.Set("Content-Type", "application/json")
	w := httptest.NewRecorder()
	// 走 ServeHTTP（路由器）而不是直接调 h.chatCompletions ——
	// 后者是小写方法，跨包不可见；且走路由才能覆盖真实入口链路
	//（与 model_not_in_region_test.go 同一做法）。
	h.ServeHTTP(w, req)

	if w.Code != http.StatusBadRequest {
		t.Errorf("状态码应为 400，实际 %d", w.Code)
	}

	var env struct {
		Error struct {
			Code    string `json:"code"`
			Message string `json:"message"`
		} `json:"error"`
	}
	if err := json.Unmarshal(w.Body.Bytes(), &env); err != nil {
		t.Fatalf("响应不是预期 JSON：%v\n%s", err, w.Body.String())
	}
	if env.Error.Code != "model_not_in_upstream" {
		t.Errorf("错误码应为 model_not_in_upstream，实际 %q —— "+
			"落进 no_healthy_account 会让用户去查账号池；"+
			"混成 model_not_in_product 会让他反复去白名单里加一个上游不存在的模型",
			env.Error.Code)
	}
}

// TestModelNotInUpstreamMessageIsActionable 消息要说清原因并给出**正确的**出路。
func TestModelNotInUpstreamMessageIsActionable(t *testing.T) {
	fq := &fakeQoderModelMissing{
		model:     "deepseek-v4.1-flash",
		available: []string{"dfmodel", "qmodel_preview"},
	}
	h := newModelMissingHandler(t, fq)

	body := `{"model":"qoder:deepseek-v4.1-flash","messages":[{"role":"user","content":"hi"}]}`
	req := httptest.NewRequest(http.MethodPost, "/v1/chat/completions", strings.NewReader(body))
	req.Header.Set("Content-Type", "application/json")
	w := httptest.NewRecorder()
	h.ServeHTTP(w, req)

	var env struct {
		Error struct {
			Message string `json:"message"`
		} `json:"error"`
	}
	_ = json.Unmarshal(w.Body.Bytes(), &env)
	msg := env.Error.Message
	if msg == "" {
		t.Fatal("失败响应应带可读消息")
	}

	// 必须带模型名 —— 用户要知道是哪个模型出的问题
	if !strings.Contains(msg, "deepseek-v4.1-flash") {
		t.Errorf("消息应含模型名，实际 %q", msg)
	}
	// 必须说清区域（两区清单不同）
	if !strings.Contains(msg, "国服") {
		t.Errorf("消息应含区域名，实际 %q", msg)
	}
	// 必须列出上游**实际可用**的模型（用户下一个问题必然是"那有什么"）
	if !strings.Contains(msg, "dfmodel") {
		t.Errorf("消息应列出上游可用模型，实际 %q", msg)
	}
	// ★ 关键：必须说清**白名单改不了它** —— 否则用户会去配置页
	// 加一个上游根本不存在的模型，然后发现"加了还是不行"
	if !strings.Contains(msg, "白名单") {
		t.Errorf("消息应说明这条模型白名单改不了它，实际 %q", msg)
	}
	// 不该出现「账号全部不可用」这类误导性说法
	if strings.Contains(msg, "账号全部不可用") {
		t.Errorf("消息不该说账号不可用（那是账号池耗尽的措辞，会误导排查方向），实际 %q", msg)
	}
}

// TestModelNotInUpstreamMessageNoListWhenEmpty 清单为空时不列清单、不编造。
func TestModelNotInUpstreamMessageNoListWhenEmpty(t *testing.T) {
	msg := modelNotInUpstreamMessage(qoder.RegionIntl, "ghost-model", nil)
	if !strings.Contains(msg, "ghost-model") {
		t.Errorf("消息应含模型名，实际 %q", msg)
	}
	if !strings.Contains(msg, "国际版") {
		t.Errorf("消息应含区域名，实际 %q", msg)
	}
	// 没有清单时不该出现"上游当前可用"那句（不编造）
	if strings.Contains(msg, "上游当前可用") {
		t.Errorf("清单为空时不该列出可用模型（不编造），实际 %q", msg)
	}
}

// TestModelNotInUpstreamMessageTruncates 清单过长时截断（与既有口径一致）。
func TestModelNotInUpstreamMessageTruncates(t *testing.T) {
	many := make([]string, 20)
	for i := range many {
		many[i] = "m" + string(rune('a'+i))
	}
	msg := modelNotInUpstreamMessage(qoder.RegionCN, "ghost", many)
	if !strings.Contains(msg, "等 20 个") {
		t.Errorf("超过 12 个时应截断并注明总数，实际 %q", msg)
	}
	if strings.Contains(msg, "mt") {
		t.Errorf("第 13 个之后的模型不该出现在消息里，实际 %q", msg)
	}
}

// TestThreeProtocolConsumersAgree 三个协议入口的映射必须都认识这个新类别。
//
// 它们共用「谁来判定失败类别」这条链，但**码面值各不相同**
// （见 responsesFailure 的注释）。漏掉任何一个都会让那条协议回落到
// no_healthy_account / upstream_error，把请求侧错误说成账号或上游故障。
func TestThreeProtocolConsumersAgree(t *testing.T) {
	ff := &forwardFailure{
		Kind:    FailureModelNotInUpstream,
		Status:  http.StatusBadRequest,
		Message: "上游没有这个模型",
	}

	// chat/completions：独立码
	if code, msg := openAIFailure(ff); code != "model_not_in_upstream" || msg == "" {
		t.Errorf("openAIFailure 应为 (model_not_in_upstream, 非空)，实际 (%q, %q)", code, msg)
	}
	// errorCodeFor 也要认（它是上面那条链之外的第二处出口）
	if code := errorCodeFor(ff); code != "model_not_in_upstream" {
		t.Errorf("errorCodeFor 应为 model_not_in_upstream，实际 %q", code)
	}
	// messages（Anthropic 词汇表）：请求侧 ⇒ invalid_request_error
	if code, _ := anthropicFailure(ff); code != "invalid_request_error" {
		t.Errorf("anthropicFailure 应为 invalid_request_error，实际 %q", code)
	}
	// responses：请求侧 ⇒ invalid_request_error（不是 upstream_error）
	if code, _ := responsesFailure(ff); code != "invalid_request_error" {
		t.Errorf("responsesFailure 应为 invalid_request_error，实际 %q", code)
	}
}

// TestModelNotInUpstreamDistinctFromProduct 两个 400 类别必须**互相可区分**。
//
// 出路相反：一个能靠白名单解决，另一个不能。混用会让用户反复去配置页
// 加一个上游不存在的模型。
func TestModelNotInUpstreamDistinctFromProduct(t *testing.T) {
	up := &forwardFailure{Kind: FailureModelNotInUpstream, Status: 400, Message: "上游没有"}
	prod := &forwardFailure{Kind: FailureModelNotInProduct, Status: 400, Message: "白名单没放行"}

	if openAICode(up) == openAICode(prod) {
		t.Errorf("两类错误必须有不同错误码（出路相反），实际都是 %q", openAICode(up))
	}
	if got := openAICode(up); got != "model_not_in_upstream" {
		t.Errorf("上游不存在应为 model_not_in_upstream，实际 %q", got)
	}
	if got := openAICode(prod); got != "model_not_in_product" {
		t.Errorf("白名单未放行应为 model_not_in_product，实际 %q", got)
	}
}

// openAICode 取 openAIFailure 的错误码（测试用的简写）。
func openAICode(err error) string {
	code, _ := openAIFailure(err)
	return code
}
