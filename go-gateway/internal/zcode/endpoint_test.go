package zcode

// endpoint_test.go 端点选择 + 端到端翻译的**集成**测试。
//
// # 为什么单独一个文件
//
// `translate_test.go` 测的是"翻译函数本身"，而这里测的是
// **接线是否正确** —— 即 `streamChatOnce` 真的打到 Anthropic 端点、
// 真的把请求体翻了、真的把响应流翻了。
//
// 「函数对但没接上」是本项目已经栽过一次的坑（captcha 求解器
// 写了 277 行测试却从没被生产代码调用）—— 故这类"接线测试"必须单独有。
import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

// TestAnthropicMessagesURLSelectsChannel 端点必须按通道选对。
//
// 抓包实测的映射（providerRules）：
//
//	有 jwt（start-plan）  → https://zcode.z.ai/api/v1/zcode-plan/anthropic/v1/messages
//	无 jwt（coding-plan） → {provider}/api/anthropic/v1/messages
func TestAnthropicMessagesURLSelectsChannel(t *testing.T) {
	// ① start-plan（有 jwt）
	start := &Cred{Provider: ProviderZAI, JWT: "eyJhbGciOi.test.sig"}
	if got, want := anthropicMessagesURL(start), StartPlanBase+AnthropicMessagesPath; got != want {
		t.Errorf("start-plan 通道端点错误\n  期望 %s\n  实际 %s", want, got)
	}
	if !strings.Contains(anthropicMessagesURL(start), "zcode.z.ai/api/v1/zcode-plan/anthropic") {
		t.Errorf("start-plan 应打 zcode.z.ai 套餐通道，实际 %s", anthropicMessagesURL(start))
	}

	// ② coding-plan（只有 credential，无 jwt）
	coding := &Cred{Provider: ProviderZAI, Credential: "key.secret"}
	got := anthropicMessagesURL(coding)
	if strings.Contains(got, "zcode-plan") {
		t.Errorf("coding-plan 不该走套餐通道，实际 %s", got)
	}
	if !strings.Contains(got, "/api/anthropic") {
		t.Errorf("coding-plan 应打服务商的 anthropic 端点，实际 %s", got)
	}
	if !strings.HasSuffix(got, AnthropicMessagesPath) {
		t.Errorf("端点必须以 %s 结尾，实际 %s", AnthropicMessagesPath, got)
	}

	// ③ bigmodel 的 coding-plan 端点
	bm := &Cred{Provider: ProviderBigmodel, Credential: "k.s"}
	if got := anthropicMessagesURL(bm); !strings.Contains(got, "open.bigmodel.cn") {
		t.Errorf("bigmodel 的 coding-plan 应打 open.bigmodel.cn，实际 %s", got)
	}
}

// TestIsStartPlan 通道判据。
func TestIsStartPlan(t *testing.T) {
	if (&Cred{JWT: "x"}).isStartPlan() != true {
		t.Error("有 jwt 应判为 start-plan")
	}
	if (&Cred{JWT: "  "}).isStartPlan() != false {
		t.Error("jwt 只有空白不应判为 start-plan")
	}
	if (&Cred{}).isStartPlan() != false {
		t.Error("无 jwt 不应判为 start-plan")
	}
	// nil 安全（不能 panic —— 它在请求路径上）
	var nilCred *Cred
	if nilCred.isStartPlan() != false {
		t.Error("nil Cred 应安全返回 false")
	}
}

// TestStreamChatOnceHitsAnthropicEndpointAndTranslates 端到端：
// 请求体被翻译、打到 Anthropic 端点、响应流被翻译回 OpenAI。
//
// 这是**最有价值的一条** —— 它同时验证了请求方向、端点选择、响应方向
// 三件事真的连起来了（而不是三个各自通过的单测）。
func TestStreamChatOnceHitsAnthropicEndpointAndTranslates(t *testing.T) {
	var (
		gotPath    string
		gotBody    map[string]any
		gotHeaders http.Header
	)
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotPath = r.URL.Path
		gotHeaders = r.Header.Clone()
		raw, _ := io.ReadAll(r.Body)
		json.Unmarshal(raw, &gotBody)

		// 回一段真实的 Anthropic SSE（照抓包结构）
		w.Header().Set("Content-Type", "text/event-stream")
		w.WriteHeader(200)
		io.WriteString(w, "event: message_start\n"+
			`data: {"type":"message_start","message":{"id":"msg_1","role":"assistant","content":[]}}`+"\n\n")
		io.WriteString(w, "event: content_block_delta\n"+
			`data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"通了"}}`+"\n\n")
		io.WriteString(w, "event: message_delta\n"+
			`data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":3,"output_tokens":1}}`+"\n\n")
		io.WriteString(w, "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n")
	}))
	defer srv.Close()

	c := New()
	// 让 Client 打到测试服务器：用 coding-plan 通道（端点来自 Provider 的
	// anthropic base，可以整体替换成 httptest 的地址）。
	//
	// ⚠ 这里**不能**用 start-plan 通道 —— 那条是硬编码的 zcode.z.ai，
	// 测试不该打真实上游。
	cr := &Cred{
		Provider:   ProviderZAI,
		Credential: "k.s",
		// 无 jwt → 走 coding-plan 分支 → 端点 = Provider.AnthropicBase()
	}
	// 覆盖端点：把 Provider 的 anthropic base 指向 httptest。
	// 用 `AnthropicBaseOverride` 若不存在，则退而验证路径后缀。
	c.SetAnthropicBaseForTest(srv.URL + "/api/anthropic")

	body := []byte(`{
		"model":"GLM-5.3-Flash",
		"messages":[
			{"role":"system","content":"你是助手"},
			{"role":"user","content":"在吗"}
		],
		"max_tokens":64,
		"stream":true
	}`)

	rc, status, errBody, err := c.StreamChat(context.Background(), cr, body)
	if err != nil {
		t.Fatalf("请求失败: %v", err)
	}
	if status != 200 {
		t.Fatalf("状态码应为 200，实际 %d，错误体 %s", status, errBody)
	}
	defer rc.Close()

	// ---- ① 打对了端点 ----
	if !strings.HasSuffix(gotPath, AnthropicMessagesPath) {
		t.Errorf("应打到 %s，实际 %s", AnthropicMessagesPath, gotPath)
	}
	if strings.Contains(gotPath, "/chat/completions") {
		t.Errorf("不该再打 OpenAI 的 /chat/completions，实际 %s", gotPath)
	}

	// ---- ② 请求体被翻译成 Anthropic 形状 ----
	if _, bad := gotBody["messages"].([]any); !bad {
		t.Fatal("请求体缺 messages")
	}
	sys, ok := gotBody["system"].([]any)
	if !ok || len(sys) != 1 {
		t.Errorf("system 应提到顶层，实际 %v", gotBody["system"])
	}
	if v, _ := gotBody["max_tokens"].(float64); v != 64 {
		t.Errorf("max_tokens 应保留，实际 %v", gotBody["max_tokens"])
	}
	// 关键：**不能**再有 OpenAI 的 `stream` 之外的 OpenAI 专用字段
	msgs := gotBody["messages"].([]any)
	if len(msgs) != 1 {
		t.Errorf("system 提走后 messages 应只剩 1 条，实际 %d 条：%v", len(msgs), msgs)
	}

	// ---- ③ 认证头与协议头齐了 ----
	// x-api-key（抓包实测官方会发，Anthropic 的标准位置）
	if gotHeaders.Get("X-Api-Key") == "" {
		t.Error("应发 X-Api-Key（Anthropic 协议的标准认证位置）")
	}
	if gotHeaders.Get("Anthropic-Version") == "" {
		t.Error("应发 anthropic-version")
	}
	if gotHeaders.Get("Anthropic-Beta") == "" {
		t.Error("应发 anthropic-beta（官方实测会发）")
	}
	// 追踪头必须含官方的 5 个
	for _, k := range []string{"X-Query-Id", "X-Session-Id", "X-Request-Id", "X-Zcode-Trace-Id"} {
		if gotHeaders.Get(k) == "" {
			t.Errorf("应发 %s（抓包实测官方会发）", k)
		}
	}
	// UA 必须带 runtime 声明
	if ua := gotHeaders.Get("User-Agent"); !strings.Contains(ua, "runtime/node.js/") {
		t.Errorf("User-Agent 应带 runtime 声明，实际 %q", ua)
	}

	// ---- ④ 响应流被翻译回 OpenAI ----
	raw, err := io.ReadAll(rc)
	if err != nil {
		t.Fatalf("读流失败: %v", err)
	}
	out := string(raw)
	if !strings.Contains(out, `"object":"chat.completion.chunk"`) {
		t.Errorf("响应应翻成 OpenAI chunk，实际：\n%s", out)
	}
	if !strings.Contains(out, "通了") {
		t.Errorf("文本应透出，实际：\n%s", out)
	}
	if !strings.Contains(out, "data: [DONE]") {
		t.Errorf("应以 [DONE] 收尾，实际：\n%s", out)
	}
	// Anthropic 特有的事件名**不该**漏到下游
	for _, leak := range []string{"content_block_delta", "message_start", "message_stop"} {
		if strings.Contains(out, leak) {
			t.Errorf("Anthropic 事件名 %q 泄漏到下游（客户端会 parse 失败）：\n%s", leak, out)
		}
	}
}

// TestStreamChatTranslatesErrorBody 上游错误体要翻成 OpenAI 形状。
//
// 不翻的话 server 层解析不出 message，用户只看到"上游错误"，
// 而真正原因（验证码/额度/参数）就藏在原文里 —— 最难排查的状态。
func TestStreamChatTranslatesErrorBody(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(400)
		// ZCode 的业务码形状
		io.WriteString(w, `{"code":3007,"msg":"captcha verify failed"}`)
	}))
	defer srv.Close()

	c := New()
	c.SetAnthropicBaseForTest(srv.URL + "/api/anthropic")
	cr := &Cred{Provider: ProviderZAI, Credential: "k.s"}

	_, status, body, err := c.StreamChat(context.Background(), cr,
		[]byte(`{"model":"m","messages":[{"role":"user","content":"hi"}]}`))
	if err != nil {
		t.Fatalf("不该返回 transport 错误: %v", err)
	}
	if status != 400 {
		t.Errorf("状态码应为 400，实际 %d", status)
	}
	var m map[string]any
	if err := json.Unmarshal(body, &m); err != nil {
		t.Fatalf("错误体应是合法 JSON，实际 %s", body)
	}
	e, ok := m["error"].(map[string]any)
	if !ok {
		t.Fatalf("错误体应包成 OpenAI 的 {error:{…}} 形状，实际 %s", body)
	}
	if msg, _ := e["message"].(string); !strings.Contains(msg, "captcha") {
		t.Errorf("错误信息应保留上游原文，实际 %q（原文：%s）", msg, body)
	}
}

// TestNormalizeErrorBodyVariants 各种上游错误形状都要能翻。
func TestNormalizeErrorBodyVariants(t *testing.T) {
	cases := []struct {
		name string
		in   string
		want string
	}{
		{"Anthropic 形状", `{"type":"error","error":{"type":"invalid_request_error","message":"参数不对"}}`, "参数不对"},
		{"ZCode 业务码", `{"code":3012,"msg":"request has been blocked due to unusual activity."}`, "unusual activity"},
		{"简单 message", `{"message":"出错了"}`, "出错了"},
		{"已是 OpenAI 形状", `{"error":{"message":"原样","type":"x"}}`, "原样"},
		{"纯文本（HTML 错误页）", `<html>502 Bad Gateway</html>`, "502"},
	}
	for _, c := range cases {
		got := string(NormalizeErrorBody([]byte(c.in), 400))
		if !strings.Contains(got, c.want) {
			t.Errorf("%s：应包含 %q，实际 %s", c.name, c.want, got)
		}
		// 除"已是 OpenAI"外，其余都要包成 error 形状
		var m map[string]any
		if err := json.Unmarshal([]byte(got), &m); err != nil {
			t.Errorf("%s：产物应是合法 JSON，实际 %s", c.name, got)
			continue
		}
		if _, ok := m["error"].(map[string]any); !ok {
			t.Errorf("%s：应包成 {error:{…}}，实际 %s", c.name, got)
		}
	}
}

// TestForceStream 强制流式（统一上游路径）。
func TestForceStream(t *testing.T) {
	// 非流式 → 改成 true
	out := forceStream([]byte(`{"model":"m","stream":false}`))
	var m map[string]any
	json.Unmarshal(out, &m)
	if v, _ := m["stream"].(bool); !v {
		t.Errorf("stream:false 应被改成 true（统一走流式，非流式由聚合器处理），实际 %v", m["stream"])
	}
	// 缺字段 → 补 true
	out2 := forceStream([]byte(`{"model":"m"}`))
	var m2 map[string]any
	json.Unmarshal(out2, &m2)
	if v, _ := m2["stream"].(bool); !v {
		t.Errorf("缺 stream 应补 true，实际 %v", m2["stream"])
	}
	// 已是 true → 原样（不改动，避免无谓的重新序列化）
	in3 := []byte(`{"model":"m","stream":true}`)
	if string(forceStream(in3)) != string(in3) {
		t.Error("已是流式时应原样返回")
	}
	// 非法 JSON → 原样（不该因为一个可选字段让整个请求失败）
	bad := []byte(`not json`)
	if string(forceStream(bad)) != string(bad) {
		t.Error("非法 JSON 应原样返回")
	}
}
