package zcode

// F-09：上游故障注入 mock。
//
// # 这个文件解决什么问题
//
// 网关对上游各类故障的处置是**分散**在 `streamChatOnce` / `NormalizeErrorBody`
// / `StreamChat` 三处的隐式契约：不重试、不改写关键信号、不误判。任何一处改动
// 都可能悄悄破坏它，而症状往往出现在**另一个包**里 —— 3007 会退化成「账号不可用」，
// 3012 的文案没了会让 server 层的产品级熔断失效。这里把上游故障做成一张
// 可复现的场景表，逐条钉住「故障发生时网关必须做什么 / 不能做什么」。
//
// # 与参考实现的关系
//
// 参考实现 `test-bin/recon/zcode-dengyie/tests/mock_upstream/server.py`
// （FastAPI，494 行）用请求头 `x-mock-scenario` / `x-mock-sequence` /
// `x-mock-bind` / `x-mock-sse-chunks` / `x-mock-sse-truncate-at` 在**进程外**
// 注入故障，并用响应头 `x-mock-call-index` 回报调用序号。
//
// 本项目的要求是 `go test` 一条命令可复现、不引入 Python/FastAPI 依赖，故这里用
// `httptest` 在**进程内**重写同一张场景表：
//
//	场景语义 —— 对齐（见 mockScenario* 常量）
//	注入机制 —— 不同：控制头 → 表驱动（一个用例一个 scenario）
//	调用观测 —— 不同：响应头 → mockUpstream 内部计数（callCount / lastRequestBody）
//
// # 纪律（与 captcha_link_test.go 一致）
//
//  1. 端点用 `SetAnthropicBaseForTest` 整体替换，Cred **不带 jwt** ⇒ 走 coding-plan
//     通道（start-plan 那条硬编码 zcode.z.ai，测试不该打真实上游）。
//  2. 本文件**绝不**碰 `SharedCaptchaSolver()` / `SetDir` / `SetExternalSolver`
//     —— 求解器是进程级单例且**没有反注册接口**，一旦设置，后续所有 `Solve()`
//     都会走 `pool.takeOrWait(…, 25s)`，把整包测试拖慢并制造顺序依赖
//     （这个坑记在 logincli_warm_test.go:110-154）。
//  3. 只做测试资产：不改生产逻辑。任一场景断言失败，先回报再改代码。

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
)

// 场景名。语义与参考实现的同名场景对齐。
const (
	// mockScenarioOK 正常回答：完整的 Anthropic SSE 四段。
	mockScenarioOK = "ok"
	// mockScenarioSSETruncate 流**中途断掉**（没有 message_delta / message_stop）。
	mockScenarioSSETruncate = "sse_truncate"
	// mockScenarioGarbageBody HTTP 200 但体不是 SSE（反代/边缘错误页）。
	mockScenarioGarbageBody = "garbage_body"
	// mockScenarioRiskControl3012 上游风控：实测载体是 **405** + 标准业务信封。
	mockScenarioRiskControl3012 = "risk_control_3012"
	// mockScenarioCaptcha3007 上游要求人机验证：400 + 业务码 3007。
	mockScenarioCaptcha3007 = "captcha_3007"
	// mockScenarioQuotaRateLimited 额度不足但状态码是 **429**（实测形状）。
	mockScenarioQuotaRateLimited = "rate_limited_quota_body"
	// mockScenarioServerError 上游 5xx。
	mockScenarioServerError = "server_error"
)

// 上游响应体原文。除 ok / sse_truncate 外的都是**实测形状**逐字复刻。
const (
	mockRiskControlBody = `{"code":3012,"msg":"request has been blocked due to unusual activity."}`
	mockCaptchaBody     = `{"code":3007,"msg":"captcha verify failed"}`
	mockQuotaBody       = `{"error":{"code":"1113","message":"余额不足或无可用资源包,请充值。"}}`
	mockServerErrBody   = `{"error":{"message":"internal server error"}}`
	mockGarbageBody     = `<html><body>502 Bad Gateway</body></html>`
)

// mockUpstreamSSE 上游正常回答的 Anthropic SSE。
//
// 与 endpoint_test.go 里的四段同形状，但独立定义：本文件要能按场景**截断**它，
// 不能被另一个文件的常量绑住（改了那边会连带弄坏这里的截断用例）。
//
// includeTail=false 模拟「上游说到一半就断了」：只剩 message_start 与一段文本增量。
func mockUpstreamSSE(includeTail bool) string {
	s := "event: message_start\n" +
		`data: {"type":"message_start","message":{"id":"msg_mock_1","role":"assistant","content":[]}}` + "\n\n" +
		"event: content_block_delta\n" +
		`data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"上游通了"}}` + "\n\n"
	if !includeTail {
		return s
	}
	return s +
		"event: message_delta\n" +
		`data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":10,"output_tokens":5}}` + "\n\n" +
		"event: message_stop\n" +
		`data: {"type":"message_stop"}` + "\n\n"
}

// mockUpstream 可注入故障的假上游：按 scenario 回预设响应，并记录被调用情况。
type mockUpstream struct {
	scenario string
	srv      *httptest.Server

	mu      sync.Mutex
	calls   int
	path    string
	reqBody map[string]any
}

// newMockUpstream 起一个假上游。t.Cleanup 负责关闭。
func newMockUpstream(t *testing.T, scenario string) *mockUpstream {
	t.Helper()
	m := &mockUpstream{scenario: scenario}
	m.srv = httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		raw, _ := io.ReadAll(r.Body)
		var reqBody map[string]any
		_ = json.Unmarshal(raw, &reqBody)

		m.mu.Lock()
		m.calls++
		m.path = r.URL.Path
		m.reqBody = reqBody
		m.mu.Unlock()

		writeMockScenario(t, w, m.scenario)
	}))
	t.Cleanup(m.srv.Close)
	return m
}

// callCount 上游被请求的次数（「该不该重试」这类契约全靠它）。
func (m *mockUpstream) callCount() int {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.calls
}

// lastPath 最后一次请求的路径（验证打的是 Anthropic 端点而不是别的）。
func (m *mockUpstream) lastPath() string {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.path
}

// lastRequestBody 最后一次请求的**翻译后**的 Anthropic 请求体。
func (m *mockUpstream) lastRequestBody() map[string]any {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.reqBody
}

// writeMockScenario 按场景写响应。
//
// ⚠ 每个场景都刻意带上**真实的状态码**：网关的分支大多先看状态码
// （如「只有 400 才可能去求解验证码」），把 3012 写成 200 就测不到真东西。
func writeMockScenario(t *testing.T, w http.ResponseWriter, scenario string) {
	switch scenario {
	case mockScenarioOK:
		w.Header().Set("Content-Type", "text/event-stream")
		w.WriteHeader(http.StatusOK)
		io.WriteString(w, mockUpstreamSSE(true))
	case mockScenarioSSETruncate:
		w.Header().Set("Content-Type", "text/event-stream")
		w.WriteHeader(http.StatusOK)
		io.WriteString(w, mockUpstreamSSE(false))
	case mockScenarioGarbageBody:
		w.Header().Set("Content-Type", "text/html")
		w.WriteHeader(http.StatusOK)
		io.WriteString(w, mockGarbageBody)
	case mockScenarioRiskControl3012:
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusMethodNotAllowed) // 405：实测载体（不是常见的 403/429）
		io.WriteString(w, mockRiskControlBody)
	case mockScenarioCaptcha3007:
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusBadRequest)
		io.WriteString(w, mockCaptchaBody)
	case mockScenarioQuotaRateLimited:
		w.Header().Set("Content-Type", "application/json")
		// ⚠ 忠实复刻上游形状：额度不足挂在 **429** 上，并带 retry-after。
		// 网关刻意**不消费** retry-after（429 的处置是 server 层换号，
		// 见 forward.go 的轮转逻辑）—— 保留它只是为了让形状真实。
		w.Header().Set("Retry-After", "30")
		w.WriteHeader(http.StatusTooManyRequests)
		io.WriteString(w, mockQuotaBody)
	case mockScenarioServerError:
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusInternalServerError)
		io.WriteString(w, mockServerErrBody)
	default:
		// 用 Errorf 而不是 Fatalf：这里在 handler 的 goroutine 上，
		// Fatalf 只会杀掉那个 goroutine，测试会以更难看的方式失败。
		t.Errorf("未知的 mock 场景 %q（是测试写错了，不是网关的问题）", scenario)
		http.Error(w, "unknown mock scenario", http.StatusInternalServerError)
	}
}

// mockStreamChat 用假上游跑一次 StreamChat。
//
// 返回翻译后的 OpenAI 流（**已读干并关闭** —— 不读干会让 TranslateStream 的
// 管道 goroutine 卡在写端）；错误场景下 rc 为 nil，out 为空串。
func mockStreamChat(t *testing.T, m *mockUpstream, openAIBody string) (out string, status int, errBody []byte, err error) {
	t.Helper()
	c := New()
	c.SetAnthropicBaseForTest(m.srv.URL + "/api/anthropic")
	cr := &Cred{Provider: ProviderZAI, Credential: "k.s"} // 无 jwt ⇒ coding-plan 通道

	rc, status, errBody, err := c.StreamChat(context.Background(), cr, []byte(openAIBody))
	if rc != nil {
		raw, rerr := io.ReadAll(rc)
		rc.Close()
		if rerr != nil {
			t.Fatalf("读取翻译流失败: %v", rerr)
		}
		out = string(raw)
	}
	return out, status, errBody, err
}

// mockChatBody 客户端请求体（OpenAI 形状，网关会把它翻成 Anthropic 形状）。
const mockChatBody = `{"model":"glm-5.3","messages":[{"role":"user","content":"hi"}],"max_tokens":32}`

// parseMockChunks 解析**翻译后**的 OpenAI 流。
//
// ⚠ 不能用 `translate_test.go` 的 `collectOpenAIChunks` —— 那个 helper 吃的输入是
// **Anthropic** SSE（它内部自己调 `TranslateStream`）。这里拿到的 out 已经是
// OpenAI 产物，再喂给它等于翻译两遍，什么也解析不出来（本文件第一版就栽在这）。
func parseMockChunks(t *testing.T, out string) []map[string]any {
	t.Helper()
	var chunks []map[string]any
	for _, line := range strings.Split(out, "\n") {
		p := strings.TrimSpace(line)
		if !strings.HasPrefix(p, "data:") {
			continue
		}
		p = strings.TrimSpace(strings.TrimPrefix(p, "data:"))
		if p == "" || p == "[DONE]" {
			continue
		}
		var c map[string]any
		if err := json.Unmarshal([]byte(p), &c); err != nil {
			t.Fatalf("翻译产物不是合法 JSON: %v\n%s", err, p)
		}
		chunks = append(chunks, c)
	}
	return chunks
}

// choicesOf 安全取 chunk 的 choices（usage chunk 的 choices 是空数组，
// 也有的 chunk 干脆没有这个字段）。
func choicesOf(c map[string]any) []any {
	if v, ok := c["choices"].([]any); ok {
		return v
	}
	return nil
}

// TestMockUpstreamOKTranslatesStream 场景 ok：正常回答的完整链路。
//
// 这一条是其余场景的**对照基线** —— 故障用例里那些「不该发生的事」
// （不该重试、不该补附注）只有在正常用例确实做对了时才有意义。
func TestMockUpstreamOKTranslatesStream(t *testing.T) {
	m := newMockUpstream(t, mockScenarioOK)

	out, status, errBody, err := mockStreamChat(t, m, mockChatBody)
	if err != nil {
		t.Fatalf("不该返回 transport 错误: %v", err)
	}
	if status != http.StatusOK {
		t.Fatalf("状态码应为 200，实际 %d（体：%s）", status, errBody)
	}
	if n := m.callCount(); n != 1 {
		t.Errorf("正常回答只该请求上游一次，实际 %d 次", n)
	}
	if p := m.lastPath(); !strings.HasSuffix(p, AnthropicMessagesPath) {
		t.Errorf("应打到 Anthropic 端点 %s，实际 %s", AnthropicMessagesPath, p)
	}
	// forceStream：客户端没写 stream 也要被改成 true（统一走流式路径，
	// 非流式由 AggregateOpenAI 聚合）。漏了它上游会回整包 JSON，
	// 而这里按 SSE 解析 ⇒ 用户看到空白回答。
	if v, _ := m.lastRequestBody()["stream"].(bool); !v {
		t.Errorf("上游请求体的 stream 应为 true（forceStream），实际 %#v", m.lastRequestBody()["stream"])
	}

	chunks := parseMockChunks(t, out)
	var text strings.Builder
	var finish any
	var usage map[string]any
	for _, c := range chunks {
		if u, ok := c["usage"].(map[string]any); ok {
			usage = u
		}
		for _, ch := range choicesOf(c) {
			cm := ch.(map[string]any)
			if d, ok := cm["delta"].(map[string]any); ok {
				if s, ok := d["content"].(string); ok {
					text.WriteString(s)
				}
			}
			if fr, ok := cm["finish_reason"]; ok && fr != nil {
				finish = fr
			}
		}
	}
	if text.String() != "上游通了" {
		t.Errorf("文本增量应拼成「上游通了」，实际 %q", text.String())
	}
	if finish != "stop" {
		t.Errorf("end_turn 应翻成 finish_reason=stop，实际 %v", finish)
	}
	// usage 必须透出（客户端与计费都靠它），且 input_tokens 要原样不合并错。
	if usage == nil {
		t.Error("应下发 usage")
	} else if v, _ := usage["prompt_tokens"].(float64); v != 10 {
		t.Errorf("prompt_tokens 应为 10，实际 %v", usage["prompt_tokens"])
	}
	if !strings.Contains(out, "data: [DONE]") {
		t.Error("正常流必须以 [DONE] 结束")
	}
	// Anthropic 的事件名泄漏到客户端流里会让 OpenAI 客户端 parse 失败。
	for _, leak := range []string{"content_block_delta", "message_start", "message_stop"} {
		if strings.Contains(out, leak) {
			t.Errorf("Anthropic 事件名 %q 泄漏进客户端流：%s", leak, out)
		}
	}

	// ---- 同一条上游回答，非流式客户端也必须拿到正确的聚合结果 ----
	//
	// server 层对 `stream:false` 的请求走 `Dispatch.Aggregate` → `AggregateOpenAI`，
	// 它把上面这条 OpenAI 流重新聚合成整包。这条链路此前没有任何测试
	//（`AggregateOpenAI` 在生产里只被 dispatch.go 调用一次），而它有个反直觉的契约：
	// **永远返回 err==nil**，且结果里**没有 usage 字段**。
	agg, aerr := AggregateOpenAI(strings.NewReader(out), "glm-5.3")
	if aerr != nil {
		t.Fatalf("聚合不该报错（它刻意永远返回 nil，好让上游怪流也能给出部分回答）: %v", aerr)
	}
	if agg["object"] != "chat.completion" {
		t.Errorf("聚合结果的 object 应为 chat.completion，实际 %v", agg["object"])
	}
	choice := choicesOf(agg)[0].(map[string]any)
	msg := choice["message"].(map[string]any)
	if msg["content"] != "上游通了" {
		t.Errorf("聚合后的 content 应为「上游通了」，实际 %v", msg["content"])
	}
	if choice["finish_reason"] != "stop" {
		t.Errorf("聚合后的 finish_reason 应为 stop，实际 %v", choice["finish_reason"])
	}
	if _, ok := agg["usage"]; ok {
		t.Error("聚合结果刻意不含 usage（usage chunk 的 choices 是空数组，会被跳过）—— 若这里变红说明契约变了")
	}
}

// TestMockUpstreamTruncatedSSEStillEndsWithDone 场景 sse_truncate：
// 上游说到一半断掉，网关也必须补 `[DONE]`。
//
// 缺了 `[DONE]` 客户端会一直等 —— 表现为「回答到一半卡住」，
// 而用户完全看不出是上游断了。
func TestMockUpstreamTruncatedSSEStillEndsWithDone(t *testing.T) {
	m := newMockUpstream(t, mockScenarioSSETruncate)

	out, status, errBody, err := mockStreamChat(t, m, mockChatBody)
	if err != nil {
		t.Fatalf("不该返回 transport 错误: %v", err)
	}
	if status != http.StatusOK {
		t.Fatalf("状态码应为 200，实际 %d（体：%s）", status, errBody)
	}
	// 断流**之前**已经拿到的增量必须保住（用户不该看到空白回答）。
	if !strings.Contains(out, "上游通了") {
		t.Errorf("断流前的文本增量丢了：%s", out)
	}
	// ★ 核心：上游没发 message_stop，网关也必须收尾。
	if !strings.HasSuffix(strings.TrimSpace(out), "data: [DONE]") {
		t.Errorf("上游断流时必须以 [DONE] 收尾（否则客户端一直等）：%s", out)
	}
}

// TestMockUpstreamGarbage200BodyYieldsOnlyDone 场景 garbage_body：
// HTTP 200 但体不是 SSE（反代/边缘节点错误页）。
//
// 这类响应最难排查：状态码是 200，网关没有错误可报。契约是
// **只补一个 [DONE]** —— 既不伪造内容，也不让客户端挂住。
func TestMockUpstreamGarbage200BodyYieldsOnlyDone(t *testing.T) {
	m := newMockUpstream(t, mockScenarioGarbageBody)

	out, status, errBody, err := mockStreamChat(t, m, mockChatBody)
	if err != nil {
		t.Fatalf("不该返回 transport 错误: %v", err)
	}
	if status != http.StatusOK {
		t.Fatalf("状态码应为 200，实际 %d（体：%s）", status, errBody)
	}
	if got := strings.TrimSpace(out); got != "data: [DONE]" {
		t.Errorf("200 但体不是 SSE 时，网关只该补一个 [DONE]（不伪造内容、也不让客户端挂住），实际 %q", got)
	}
}

// TestMockUpstream3012KeepsTheSignalTheBreakerNeeds 场景 risk_control_3012。
//
// server 层的 3012 处置（`internal/server/unusual.go`）判据有两重：
// 响应体里**顶层**的 `code:3012`，或 `unusual activity` 文案。而 zcode 通道上
// 到达 server 层的 respBody 是 `NormalizeErrorBody` 的产物 —— 业务码被搬进了
// `error.code`（不再是顶层），所以**文案是唯一存活的依托**。
//
// ⚠ 已知脆弱点（未修，属 server 包判据）：顶层 code 判据在 zcode 通道上是死代码，
// 上游一旦改写这句英文文案，产品级熔断就只剩 90 秒冷却的兜底。本用例把
// 「文案必须原样透出」钉住，任何想动归一化的改动都会在这里变红。
func TestMockUpstream3012KeepsTheSignalTheBreakerNeeds(t *testing.T) {
	m := newMockUpstream(t, mockScenarioRiskControl3012)

	out, status, errBody, err := mockStreamChat(t, m, mockChatBody)
	if err != nil {
		t.Fatalf("不该返回 transport 错误: %v", err)
	}
	if out != "" {
		t.Errorf("错误响应不该被当成可读流交给客户端（server 层会当成功流解析）：%q", out)
	}
	// 405 是上游原文，网关原样保留（升级成 503 是 server 层的事）。
	if status != http.StatusMethodNotAllowed {
		t.Errorf("状态码应保留上游的 405，实际 %d", status)
	}
	// 3012 与账号无关：换号/重试只会加重风控，故一次都不该多打。
	if n := m.callCount(); n != 1 {
		t.Errorf("3012 之后不该重试上游（换号对风控无效且会加重它），实际请求 %d 次", n)
	}

	body := string(errBody)
	if !strings.Contains(strings.ToLower(body), "unusual activity") {
		t.Fatalf("归一化后 3012 的判据文案必须存活 —— server 层的产品级熔断全靠它：%s", body)
	}
	var doc map[string]any
	if err := json.Unmarshal(errBody, &doc); err != nil {
		t.Fatalf("错误体应是合法 JSON，实际 %s", body)
	}
	e, ok := doc["error"].(map[string]any)
	if !ok {
		t.Fatalf("错误体应包成 OpenAI 的 {error:{…}} 形状，实际 %s", body)
	}
	if code, _ := e["code"].(float64); int(code) != 3012 {
		t.Errorf("error.code 应保留业务码 3012，实际 %#v（%s）", e["code"], body)
	}

	// ⚠ 反向：3012 不能被误判成「需要人机验证」。判据是宽匹配，误判会白耗
	// 一次求解（起 Node、约 3 秒、且会上游限流），并把用户引向错误方向。
	if IsCaptchaRequiredBody(errBody) {
		t.Errorf("3012 被误判成人机验证 —— 会白耗一次求解配额：%s", body)
	}
	if strings.Contains(body, "网关附注") {
		t.Errorf("3012 不该被追加验证码附注：%s", body)
	}
}

// TestMockUpstreamRiskSignalsDoNotCrossContaminate 两种风控信号不能互相串味。
//
// 它们来自上游两个完全不同的机制，处置也完全不同：
//
//	3007 → 用户去官方客户端做一次验证（换号无效）
//	3012 → 产品级熔断 90 秒（与账号无关）
//
// 串味的代价是不对称的：把 3007 读成 3012 会**整个产品**停摆 90 秒；
// 把 3012 读成 3007 会白耗一次求解。两个方向都在这里钉住。
//
// ⚠ 3007 的完整处置链路（改写后判据仍命中、附注写进 message、不重试上游）
// 已由 `captcha_link_test.go` 的 `TestCaptchaSignalSurvivesClientRewrite` 覆盖，
// 本用例只补**串味**这一面，不重复。
func TestMockUpstreamRiskSignalsDoNotCrossContaminate(t *testing.T) {
	// ① 3007 的响应体里不该出现 3012 的判据文案。
	m3007 := newMockUpstream(t, mockScenarioCaptcha3007)
	_, status, body3007, err := mockStreamChat(t, m3007, mockChatBody)
	if err != nil {
		t.Fatalf("3007 场景不该返回 transport 错误: %v", err)
	}
	if status != http.StatusBadRequest {
		t.Errorf("3007 的状态码应保留 400，实际 %d", status)
	}
	if strings.Contains(strings.ToLower(string(body3007)), "unusual activity") {
		t.Errorf("3007 的响应体里出现了 3012 的判据文案 —— server 层会误熔断整个产品 90 秒：%s", body3007)
	}

	// ② 3012 的响应体不该被判成「需要人机验证」。
	m3012 := newMockUpstream(t, mockScenarioRiskControl3012)
	_, _, body3012, err := mockStreamChat(t, m3012, mockChatBody)
	if err != nil {
		t.Fatalf("3012 场景不该返回 transport 错误: %v", err)
	}
	if IsCaptchaRequiredBody(body3012) {
		t.Errorf("3012 被判成需要人机验证 —— 会白耗一次求解配额：%s", body3012)
	}
}

// TestMockUpstreamQuotaBodyBeatsRateLimit 场景 rate_limited_quota_body。
//
// 上游把「额度不足」挂在 **429** 上，于是它天然长得像限流。分类器必须靠
// 业务码/文案把它认成额度问题 —— 报成限流会让用户以为「等一会儿就好」，
// 而实际要换账号或买资源包。
//
// ⚠ `Classify` 本身的判定顺序已由 `zcode_test.go` 的
// `TestClassifyQuotaBeatsRateLimit` 钉住；本用例补的是**端到端那一面**：
// 走完 `streamChatOnce` → `NormalizeErrorBody` 之后，额度信号是否还活着。
// （「函数对但接线断了」是本项目栽过的坑。）
func TestMockUpstreamQuotaBodyBeatsRateLimit(t *testing.T) {
	m := newMockUpstream(t, mockScenarioQuotaRateLimited)

	_, status, body, err := mockStreamChat(t, m, mockChatBody)
	if err != nil {
		t.Fatalf("不该返回 transport 错误: %v", err)
	}
	if status != http.StatusTooManyRequests {
		t.Errorf("状态码应保留上游的 429，实际 %d", status)
	}
	if n := m.callCount(); n != 1 {
		t.Errorf("429 的处置在 server 层（换号），client 层不该自己重试，实际请求 %d 次", n)
	}

	// 改写链路不能吃掉额度信号。
	if !strings.Contains(string(body), "1113") || !strings.Contains(string(body), "余额不足") {
		t.Fatalf("归一化后额度信号必须存活（否则会退化成「限流」误导用户）：%s", body)
	}
	// 已是 OpenAI 形状 ⇒ 逐字透传（归一化不该重排/重写已经正确的错误体）。
	if string(body) != mockQuotaBody {
		t.Errorf("已是 OpenAI 形状的错误体应原样透传：\n  期望 %s\n  实际 %s", mockQuotaBody, body)
	}

	kind := Classify(status, string(body))
	if kind != ErrNoResourcePack {
		t.Errorf("429 + 额度文案应归为 ErrNoResourcePack（该换号/买资源包），实际 %v", kind)
	}
	if kind == ErrQuotaExhausted {
		t.Error("不该报「额度已耗尽」—— 实测同一账号的额度就在那儿，是这条通道没有资源包")
	}
}

// TestMockUpstreamServerErrorClassifiedAsProviderDown 场景 server_error。
//
// 上游 5xx 是「服务端的问题」，必须与「请求/账号的问题」区分开：
// 归错会让用户去改一个根本没坏的凭证，或者让网关去罚一个无辜的账号。
func TestMockUpstreamServerErrorClassifiedAsProviderDown(t *testing.T) {
	m := newMockUpstream(t, mockScenarioServerError)

	_, status, body, err := mockStreamChat(t, m, mockChatBody)
	if err != nil {
		t.Fatalf("不该返回 transport 错误: %v", err)
	}
	if status != http.StatusInternalServerError {
		t.Errorf("状态码应保留上游的 500，实际 %d", status)
	}
	if kind := Classify(status, string(body)); kind != ErrProviderDown {
		t.Errorf("上游 5xx 应归为 ErrProviderDown，实际 %v（体：%s）", kind, body)
	}
	if IsCaptchaRequiredBody(body) {
		t.Errorf("5xx 不该被判成人机验证：%s", body)
	}
	if strings.Contains(string(body), "网关附注") {
		t.Errorf("5xx 不该被追加验证码附注：%s", body)
	}
}
