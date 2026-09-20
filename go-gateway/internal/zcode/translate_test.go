package zcode

// translate_test.go 翻译层的**双向**单测。
//
// # 为什么要测两个方向
//
// 翻译错了**不会报错** —— 只会让用户看到空回答、或上游 400。
// 故每个方向都要有断言，且要覆盖"会静默出错"的那些点：
//
//	· 角色映射（system→顶层、tool→user+tool_result）
//	· max_tokens 必填（OpenAI 可不传，Anthropic 不传就 400）
//	· 工具形状（function.parameters → input_schema）
//	· 推理档位（reasoning_effort → output_config.effort）
//	· SSE 事件顺序（message_start 要先发 role）
//	· 工具参数增量拼接（OpenAI 是字符串拼接，不是对象）
//	· usage 的**缓存 token 合并**（Anthropic 的 input_tokens 不含缓存）
//	· 流意外结束也要补 [DONE]（否则客户端卡住）
import (
	"encoding/json"
	"io"
	"strings"
	"testing"
)

// ---------------------------------------------------------------------------
// 方向一：OpenAI 请求 → Anthropic 请求
// ---------------------------------------------------------------------------

func mustJSON(t *testing.T, s string) []byte {
	t.Helper()
	out, err := BuildAnthropicBody([]byte(s), "GLM-5.3-Flash")
	if err != nil {
		t.Fatalf("翻译失败: %v", err)
	}
	var m map[string]any
	if err := json.Unmarshal(out, &m); err != nil {
		t.Fatalf("产物不是合法 JSON: %v\n%s", err, out)
	}
	return out
}

func decodeOut(t *testing.T, s string) map[string]any {
	t.Helper()
	var m map[string]any
	if err := json.Unmarshal(mustJSON(t, s), &m); err != nil {
		t.Fatal(err)
	}
	return m
}

// TestBuildAnthropicSystemGoesToTopLevel system 必须**提到顶层**。
//
// OpenAI 把 system 放在 messages 里，Anthropic 放在顶层 —— 放错位置时
// 上游**不报错**，只是模型看不到系统提示（表现为"它不听话了"），
// 是最难查的一类翻译 bug。
func TestBuildAnthropicSystemGoesToTopLevel(t *testing.T) {
	out := decodeOut(t, `{
		"model":"x",
		"messages":[
			{"role":"system","content":"你是助手"},
			{"role":"developer","content":"补充规则"},
			{"role":"user","content":"你好"}
		]
	}`)

	sys, ok := out["system"].([]any)
	if !ok || len(sys) != 2 {
		t.Fatalf("system 应提到顶层且有 2 块（system + developer），实际 %v", out["system"])
	}
	first := sys[0].(map[string]any)
	if first["type"] != "text" || first["text"] != "你是助手" {
		t.Errorf("system 首块错误: %v", first)
	}
	// messages 里**不应**再有 system 角色
	msgs := out["messages"].([]any)
	if len(msgs) != 1 {
		t.Fatalf("messages 应只剩 1 条（user），实际 %d 条: %v", len(msgs), msgs)
	}
	if m := msgs[0].(map[string]any); m["role"] != "user" {
		t.Errorf("剩余消息应为 user，实际 %v", m["role"])
	}
}

// TestBuildAnthropicMaxTokensAlwaysPresent `max_tokens` 必须**总是**存在。
//
// Anthropic 协议里它是必填，OpenAI 客户端经常不传 ——
// 直接透传会被上游 400，且错误信息不说"缺 max_tokens"。
func TestBuildAnthropicMaxTokensAlwaysPresent(t *testing.T) {
	// ① 没传 → 用默认值
	out := decodeOut(t, `{"messages":[{"role":"user","content":"hi"}]}`)
	if v, ok := out["max_tokens"].(float64); !ok || v <= 0 {
		t.Errorf("未传 max_tokens 时应填默认值，实际 %v", out["max_tokens"])
	}
	// ② 传了 → 用传入值（不能被默认值覆盖）
	out2 := decodeOut(t, `{"max_tokens":123,"messages":[{"role":"user","content":"hi"}]}`)
	if v, _ := out2["max_tokens"].(float64); v != 123 {
		t.Errorf("传入的 max_tokens 应保留，实际 %v", out2["max_tokens"])
	}
	// ③ 传 0 → 视为未传（0 会让上游拒绝）
	out3 := decodeOut(t, `{"max_tokens":0,"messages":[{"role":"user","content":"hi"}]}`)
	if v, _ := out3["max_tokens"].(float64); v <= 0 {
		t.Errorf("max_tokens=0 应回落到默认值，实际 %v", out3["max_tokens"])
	}
}

// TestBuildAnthropicToolResultBecomesUserBlock 工具结果必须是
// **user 角色 + tool_result 块**。
//
// Anthropic **没有** `tool` 角色。发成 role:"tool" 会被上游 400，
// 而错误只说"invalid role"，不告诉你要转成什么。
func TestBuildAnthropicToolResultBecomesUserBlock(t *testing.T) {
	out := decodeOut(t, `{
		"messages":[
			{"role":"user","content":"天气"},
			{"role":"assistant","content":"","tool_calls":[
				{"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"北京\"}"}}
			]},
			{"role":"tool","tool_call_id":"call_1","content":"晴 25 度"}
		]
	}`)

	msgs := out["messages"].([]any)
	if len(msgs) != 3 {
		t.Fatalf("应有 3 条消息，实际 %d", len(msgs))
	}
	// assistant 的 tool_calls → content 里的 tool_use
	asst := msgs[1].(map[string]any)
	if asst["role"] != "assistant" {
		t.Fatalf("第 2 条应是 assistant，实际 %v", asst["role"])
	}
	blocks := asst["content"].([]any)
	var toolUse map[string]any
	for _, b := range blocks {
		if bm, ok := b.(map[string]any); ok && bm["type"] == "tool_use" {
			toolUse = bm
		}
	}
	if toolUse == nil {
		t.Fatalf("assistant 的 tool_calls 应翻成 tool_use 块，实际 %v", blocks)
	}
	if toolUse["name"] != "get_weather" {
		t.Errorf("tool_use.name 错误: %v", toolUse["name"])
	}
	if toolUse["id"] != "call_1" {
		t.Errorf("tool_use.id 应保留原 id（否则结果对不上）: %v", toolUse["id"])
	}
	// arguments 字符串 → input 对象
	input, ok := toolUse["input"].(map[string]any)
	if !ok || input["city"] != "北京" {
		t.Errorf("tool_use.input 应是解析后的对象，实际 %v", toolUse["input"])
	}

	// tool 消息 → user + tool_result，且 tool_use_id 要对上
	tr := msgs[2].(map[string]any)
	if tr["role"] != "user" {
		t.Errorf("tool 结果必须用 user 角色（Anthropic 无 tool 角色），实际 %v", tr["role"])
	}
	tblocks := tr["content"].([]any)
	tb := tblocks[0].(map[string]any)
	if tb["type"] != "tool_result" {
		t.Errorf("应是 tool_result 块，实际 %v", tb["type"])
	}
	if tb["tool_use_id"] != "call_1" {
		t.Errorf("tool_use_id 必须与 tool_use.id 一致，实际 %v", tb["tool_use_id"])
	}
}

// TestBuildAnthropicToolsSchemaRename tools 的 `parameters` → `input_schema`。
func TestBuildAnthropicToolsSchemaRename(t *testing.T) {
	out := decodeOut(t, `{
		"messages":[{"role":"user","content":"hi"}],
		"tools":[{"type":"function","function":{
			"name":"f","description":"d",
			"parameters":{"type":"object","properties":{"a":{"type":"string"}}}
		}}]
	}`)
	tools, ok := out["tools"].([]any)
	if !ok || len(tools) != 1 {
		t.Fatalf("tools 应保留 1 个，实际 %v", out["tools"])
	}
	tl := tools[0].(map[string]any)
	if tl["name"] != "f" {
		t.Errorf("name 应在顶层（不是 function.name），实际 %v", tl["name"])
	}
	if _, bad := tl["function"]; bad {
		t.Error("不应保留 OpenAI 的嵌套 function 字段")
	}
	if _, ok := tl["input_schema"].(map[string]any); !ok {
		t.Errorf("parameters 应改名成 input_schema（必填，缺了上游 400），实际 %v", tl["input_schema"])
	}
	if tl["description"] != "d" {
		t.Errorf("description 应保留，实际 %v", tl["description"])
	}
}

// TestBuildAnthropicToolsSchemaFallback 缺 parameters 时**必须补一个空 schema**。
//
// input_schema 是 Anthropic 的必填字段；缺了整条请求 400，
// 而用户的工具定义里"无参数工具"很常见。
func TestBuildAnthropicToolsSchemaFallback(t *testing.T) {
	out := decodeOut(t, `{
		"messages":[{"role":"user","content":"hi"}],
		"tools":[{"type":"function","function":{"name":"noargs"}}]
	}`)
	tools := out["tools"].([]any)
	tl := tools[0].(map[string]any)
	sch, ok := tl["input_schema"].(map[string]any)
	if !ok {
		t.Fatalf("缺 parameters 时应补空 schema，实际 %v", tl["input_schema"])
	}
	if sch["type"] != "object" {
		t.Errorf("空 schema 应为 {type:object}，实际 %v", sch)
	}
}

// TestBuildAnthropicReasoningEffort 档位 → output_config.effort。
//
// 取值照 `builtinModels[].reasoning.levels`（抓包实测）：
// low→low / high→high / max→max，且默认档位是 max。
func TestBuildAnthropicReasoningEffort(t *testing.T) {
	cases := []struct {
		in   string
		want string
	}{
		{`"low"`, "low"},
		{`"high"`, "high"},
		{`"max"`, "max"},
		// 别名就近映射（上游只认 low/high/max 三个值）
		{`"minimal"`, "low"},
		{`"medium"`, "high"},
	}
	for _, c := range cases {
		out := decodeOut(t, `{
			"messages":[{"role":"user","content":"hi"}],
			"reasoning_effort":`+c.in+`
		}`)
		oc, ok := out["output_config"].(map[string]any)
		if !ok {
			t.Errorf("reasoning_effort=%s 应产生 output_config，实际 %v", c.in, out["output_config"])
			continue
		}
		if oc["effort"] != c.want {
			t.Errorf("reasoning_effort=%s 应映射成 %q，实际 %q", c.in, c.want, oc["effort"])
		}
		if th, ok := out["thinking"].(map[string]any); !ok || th["type"] != "enabled" {
			t.Errorf("reasoning_effort=%s 应同时开 thinking，实际 %v", c.in, out["thinking"])
		}
	}
	// 没传档位时**不应**凭空加 thinking（那会让所有请求都变慢变贵）
	out := decodeOut(t, `{"messages":[{"role":"user","content":"hi"}]}`)
	if _, has := out["thinking"]; has {
		t.Error("未指定档位时不应发 thinking")
	}
}

// TestBuildAnthropicStopBecomesSequences OpenAI 的 stop → stop_sequences。
func TestBuildAnthropicStopBecomesSequences(t *testing.T) {
	// 字符串形态
	out := decodeOut(t, `{"messages":[{"role":"user","content":"hi"}],"stop":"END"}`)
	ss, ok := out["stop_sequences"].([]any)
	if !ok || len(ss) != 1 || ss[0] != "END" {
		t.Errorf("stop 字符串应包成数组，实际 %v", out["stop_sequences"])
	}
	if _, has := out["stop"]; has {
		t.Error("不应保留 OpenAI 的 stop 字段")
	}
	// 数组形态
	out2 := decodeOut(t, `{"messages":[{"role":"user","content":"hi"}],"stop":["A","B"]}`)
	ss2, ok := out2["stop_sequences"].([]any)
	if !ok || len(ss2) != 2 {
		t.Errorf("stop 数组应透传，实际 %v", out2["stop_sequences"])
	}
}

// TestBuildAnthropicEmptyAssistantContentIsPatched assistant 只有 tool_calls
// 时 content 会是空数组 —— Anthropic 不接受，要补空文本块。
func TestBuildAnthropicEmptyAssistantContentIsPatched(t *testing.T) {
	out := decodeOut(t, `{
		"messages":[
			{"role":"user","content":"hi"},
			{"role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":"f","arguments":"{}"}}]}
		]
	}`)
	msgs := out["messages"].([]any)
	asst := msgs[1].(map[string]any)
	blocks := asst["content"].([]any)
	if len(blocks) == 0 {
		t.Error("assistant 的 content 不应为空数组（Anthropic 会 400）")
	}
}

// TestBuildAnthropicRejectsEmptyMessages 没有可用消息时要**明确报错**。
//
// 静默发一个空 messages 会让上游 400，而用户看到的是"上游参数错误"，
// 不知道是自己的请求被我们的翻译层掏空了。
func TestBuildAnthropicRejectsEmptyMessages(t *testing.T) {
	if _, err := BuildAnthropicBody([]byte(`{"messages":[]}`), "m"); err == nil {
		t.Error("空 messages 应报错")
	}
	if _, err := BuildAnthropicBody([]byte(`{"messages":[{"role":"system","content":"only system"}]}`), "m"); err == nil {
		t.Error("只有 system 时也应报错（没有对话内容）")
	}
}

// ---------------------------------------------------------------------------
// 方向二：Anthropic SSE → OpenAI SSE
// ---------------------------------------------------------------------------

// anthropicStream 一段真实的 Anthropic SSE（照抓包的结构写的）。
const anthropicStream = `event: message_start
data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"GLM-5.3-Flash","usage":{"input_tokens":17,"output_tokens":1}}}

event: ping
data: {"type":"ping"}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"你好"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"，世界"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":17,"output_tokens":35,"cache_read_input_tokens":53376}}

event: message_stop
data: {"type":"message_stop"}

`

// collectOpenAIChunks 把翻译后的 SSE 解析成结构体列表。
func collectOpenAIChunks(t *testing.T, in string) []map[string]any {
	t.Helper()
	rc := TranslateStream(strings.NewReader(in), "GLM-5.3-Flash")
	defer rc.Close()
	raw, err := io.ReadAll(rc)
	if err != nil {
		t.Fatalf("读取翻译流失败: %v", err)
	}
	var out []map[string]any
	for _, line := range strings.Split(string(raw), "\n") {
		line = strings.TrimSpace(line)
		if !strings.HasPrefix(line, "data:") {
			continue
		}
		p := strings.TrimSpace(strings.TrimPrefix(line, "data:"))
		if p == "" || p == "[DONE]" {
			continue
		}
		var m map[string]any
		if err := json.Unmarshal([]byte(p), &m); err != nil {
			t.Fatalf("翻译产物不是合法 JSON: %v\n%s", err, p)
		}
		out = append(out, m)
	}
	return out
}

// TestTranslateStreamBasic 基本流：role 起手 → 文本增量 → finish → [DONE]。
func TestTranslateStreamBasic(t *testing.T) {
	chunks := collectOpenAIChunks(t, anthropicStream)
	if len(chunks) == 0 {
		t.Fatal("翻译结果为空")
	}

	// 每个 chunk 都必须是 OpenAI 的 chunk 形状
	for i, c := range chunks {
		if c["object"] != "chat.completion.chunk" {
			t.Errorf("第 %d 个 chunk 的 object 应为 chat.completion.chunk，实际 %v", i, c["object"])
		}
		if c["model"] != "GLM-5.3-Flash" {
			t.Errorf("第 %d 个 chunk 的 model 错误: %v", i, c["model"])
		}
		if _, ok := c["choices"].([]any); !ok {
			t.Errorf("第 %d 个 chunk 缺 choices 数组", i)
		}
	}

	// 第一个 chunk 必须带 role（OpenAI 客户端靠它起手）
	first := chunks[0]["choices"].([]any)[0].(map[string]any)
	if d := first["delta"].(map[string]any); d["role"] != "assistant" {
		t.Errorf("首个 chunk 应带 role=assistant，实际 %v", first)
	}

	// 文本要拼出来
	var text strings.Builder
	for _, c := range chunks {
		for _, ch := range c["choices"].([]any) {
			cm := ch.(map[string]any)
			if d, ok := cm["delta"].(map[string]any); ok {
				if s, ok := d["content"].(string); ok {
					text.WriteString(s)
				}
			}
		}
	}
	if text.String() != "你好，世界" {
		t.Errorf("文本增量应拼成「你好，世界」，实际 %q", text.String())
	}

	// 必须有 finish_reason=stop
	var gotFinish any
	for _, c := range chunks {
		for _, ch := range c["choices"].([]any) {
			if fr, ok := ch.(map[string]any)["finish_reason"]; ok && fr != nil {
				gotFinish = fr
			}
		}
	}
	if gotFinish != "stop" {
		t.Errorf("end_turn 应翻成 finish_reason=stop，实际 %v", gotFinish)
	}
}

// TestTranslateStreamUsageMergesCache Anthropic 的 input_tokens **不含**缓存，
// OpenAI 的 prompt_tokens **含** —— 必须相加。
//
// 不合并会让用户看到的"输入 token"远小于实际（缓存命中的部分凭空消失），
// 且**不报错**，只是统计偏小。
func TestTranslateStreamUsageMergesCache(t *testing.T) {
	chunks := collectOpenAIChunks(t, anthropicStream)
	var usage map[string]any
	for _, c := range chunks {
		if u, ok := c["usage"].(map[string]any); ok {
			usage = u
		}
	}
	if usage == nil {
		t.Fatal("应下发 usage")
	}
	// 17 (input) + 53376 (cache_read) = 53393
	if v, _ := usage["prompt_tokens"].(float64); v != 53393 {
		t.Errorf("prompt_tokens 应含缓存（17+53376=53393），实际 %v", usage["prompt_tokens"])
	}
	if v, _ := usage["completion_tokens"].(float64); v != 35 {
		t.Errorf("completion_tokens 应为 35，实际 %v", usage["completion_tokens"])
	}
	if v, _ := usage["total_tokens"].(float64); v != 53393+35 {
		t.Errorf("total_tokens 应为 53428，实际 %v", usage["total_tokens"])
	}
	// 缓存明细也要给（界面会展示"缓存命中"）
	details, ok := usage["prompt_tokens_details"].(map[string]any)
	if !ok || details["cached_tokens"] == nil {
		t.Errorf("应下发 prompt_tokens_details.cached_tokens，实际 %v", usage)
	}
}

// TestTranslateStreamAlwaysEndsWithDone 流必须以 `[DONE]` 结束。
//
// 缺了它客户端会一直等（表现为"回答到一半卡住"）。
func TestTranslateStreamAlwaysEndsWithDone(t *testing.T) {
	// 正常流
	rc := TranslateStream(strings.NewReader(anthropicStream), "m")
	raw, _ := io.ReadAll(rc)
	rc.Close()
	if !strings.Contains(string(raw), "data: [DONE]") {
		t.Error("正常流应以 [DONE] 结束")
	}

	// **上游中途断掉**（没有 message_stop）—— 也必须补 [DONE]
	truncated := strings.ReplaceAll(anthropicStream, `event: message_stop
data: {"type":"message_stop"}

`, "")
	if truncated == anthropicStream {
		t.Fatal("测试构造失败：没去掉 message_stop")
	}
	rc2 := TranslateStream(strings.NewReader(truncated), "m")
	raw2, _ := io.ReadAll(rc2)
	rc2.Close()
	if !strings.Contains(string(raw2), "data: [DONE]") {
		t.Errorf("上游中断时也必须补 [DONE]，否则客户端卡住。实际输出：\n%s", raw2)
	}
}

// TestTranslateStreamToolCalls 工具调用流：tool_use 块 + 参数增量。
func TestTranslateStreamToolCalls(t *testing.T) {
	stream := `event: message_start
data: {"type":"message_start","message":{"id":"msg_2","role":"assistant","content":[]}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"get_weather","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"北京\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"input_tokens":10,"output_tokens":5}}

event: message_stop
data: {"type":"message_stop"}

`
	chunks := collectOpenAIChunks(t, stream)

	// 找 tool_calls 增量
	var name string
	var args strings.Builder
	for _, c := range chunks {
		for _, ch := range c["choices"].([]any) {
			d, _ := ch.(map[string]any)["delta"].(map[string]any)
			tcs, ok := d["tool_calls"].([]any)
			if !ok {
				continue
			}
			for _, tc := range tcs {
				tcm := tc.(map[string]any)
				fn, _ := tcm["function"].(map[string]any)
				if n, ok := fn["name"].(string); ok && n != "" {
					name = n
				}
				if a, ok := fn["arguments"].(string); ok {
					args.WriteString(a)
				}
			}
		}
	}
	if name != "get_weather" {
		t.Errorf("工具名应下发，实际 %q", name)
	}
	// 参数是**字符串拼接**（OpenAI 约定），拼完应是合法 JSON
	if got := args.String(); got != `{"city":"北京"}` {
		t.Errorf("参数增量应拼成完整 JSON，实际 %q", got)
	}

	// finish_reason 应是 tool_calls
	var gotFinish any
	for _, c := range chunks {
		for _, ch := range c["choices"].([]any) {
			if fr, ok := ch.(map[string]any)["finish_reason"]; ok && fr != nil {
				gotFinish = fr
			}
		}
	}
	if gotFinish != "tool_calls" {
		t.Errorf("tool_use 应翻成 finish_reason=tool_calls，实际 %v", gotFinish)
	}
}

// TestTranslateStreamThinking thinking 增量 → reasoning_content。
//
// 网关的 upstream/sse.go 认 `reasoning_content` 这个字段名
//（见该文件第 299 行），故必须用它，不能用 thinking。
func TestTranslateStreamThinking(t *testing.T) {
	stream := `event: message_start
data: {"type":"message_start","message":{"id":"m","role":"assistant","content":[]}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"让我想想"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":1,"output_tokens":1}}

event: message_stop
data: {"type":"message_stop"}

`
	chunks := collectOpenAIChunks(t, stream)
	var reasoning strings.Builder
	for _, c := range chunks {
		for _, ch := range c["choices"].([]any) {
			d, _ := ch.(map[string]any)["delta"].(map[string]any)
			if s, ok := d["reasoning_content"].(string); ok {
				reasoning.WriteString(s)
			}
		}
	}
	if reasoning.String() != "让我想想" {
		t.Errorf("thinking 应翻成 reasoning_content，实际 %q", reasoning.String())
	}
}

// TestTranslateStreamErrorFrame 上游中途报错也要收尾（发 error + [DONE]）。
func TestTranslateStreamErrorFrame(t *testing.T) {
	stream := `event: message_start
data: {"type":"message_start","message":{"id":"m","role":"assistant","content":[]}}

event: error
data: {"type":"error","error":{"type":"overloaded_error","message":"上游过载"}}

`
	rc := TranslateStream(strings.NewReader(stream), "m")
	raw, _ := io.ReadAll(rc)
	rc.Close()
	s := string(raw)
	if !strings.Contains(s, "上游过载") {
		t.Errorf("错误信息应透出，实际：\n%s", s)
	}
	if !strings.Contains(s, "data: [DONE]") {
		t.Errorf("报错后也要收尾发 [DONE]，实际：\n%s", s)
	}
}

// TestTranslateStreamIgnoresGarbage 坏事件不该中断整条流。
//
// 中断会让用户看到的回答比上游实际给的短，且没有任何提示。
func TestTranslateStreamIgnoresGarbage(t *testing.T) {
	stream := `event: message_start
data: {"type":"message_start","message":{"id":"m","role":"assistant","content":[]}}

data: 这不是 JSON

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"还在"}}

event: message_stop
data: {"type":"message_stop"}

`
	chunks := collectOpenAIChunks(t, stream)
	var text strings.Builder
	for _, c := range chunks {
		for _, ch := range c["choices"].([]any) {
			d, _ := ch.(map[string]any)["delta"].(map[string]any)
			if s, ok := d["content"].(string); ok {
				text.WriteString(s)
			}
		}
	}
	if text.String() != "还在" {
		t.Errorf("坏事件不应影响后续内容，实际 %q", text.String())
	}
}

// ---------------------------------------------------------------------------
// 方向三：非流式
// ---------------------------------------------------------------------------

// TestTranslateNonStreamBasic 非流式 message → chat.completion。
func TestTranslateNonStreamBasic(t *testing.T) {
	in := `{
		"id":"msg_3","type":"message","role":"assistant","model":"GLM-5.3-Flash",
		"content":[{"type":"text","text":"你好"}],
		"stop_reason":"end_turn",
		"usage":{"input_tokens":10,"output_tokens":2,"cache_read_input_tokens":100}
	}`
	out, err := TranslateNonStream([]byte(in), "GLM-5.3-Flash")
	if err != nil {
		t.Fatal(err)
	}
	var m map[string]any
	if err := json.Unmarshal(out, &m); err != nil {
		t.Fatal(err)
	}
	if m["object"] != "chat.completion" {
		t.Errorf("object 应为 chat.completion，实际 %v", m["object"])
	}
	ch := m["choices"].([]any)[0].(map[string]any)
	msg := ch["message"].(map[string]any)
	if msg["content"] != "你好" {
		t.Errorf("文本应合并到 content，实际 %v", msg["content"])
	}
	if msg["role"] != "assistant" {
		t.Errorf("role 应为 assistant，实际 %v", msg["role"])
	}
	if ch["finish_reason"] != "stop" {
		t.Errorf("end_turn 应翻成 stop，实际 %v", ch["finish_reason"])
	}
	u := m["usage"].(map[string]any)
	if v, _ := u["prompt_tokens"].(float64); v != 110 {
		t.Errorf("prompt_tokens 应含缓存（10+100=110），实际 %v", u["prompt_tokens"])
	}
}

// TestTranslateNonStreamToolCalls 非流式的 tool_use → tool_calls。
func TestTranslateNonStreamToolCalls(t *testing.T) {
	in := `{
		"id":"msg_4","role":"assistant",
		"content":[{"type":"tool_use","id":"toolu_9","name":"f","input":{"a":1}}],
		"stop_reason":"tool_use",
		"usage":{"input_tokens":5,"output_tokens":5}
	}`
	out, _ := TranslateNonStream([]byte(in), "m")
	var m map[string]any
	json.Unmarshal(out, &m)
	ch := m["choices"].([]any)[0].(map[string]any)
	msg := ch["message"].(map[string]any)
	tcs, ok := msg["tool_calls"].([]any)
	if !ok || len(tcs) != 1 {
		t.Fatalf("tool_use 应翻成 tool_calls，实际 %v", msg["tool_calls"])
	}
	tc := tcs[0].(map[string]any)
	fn := tc["function"].(map[string]any)
	if fn["name"] != "f" {
		t.Errorf("工具名错误: %v", fn["name"])
	}
	if fn["arguments"] != `{"a":1}` {
		t.Errorf("input 应序列化成 arguments 字符串，实际 %v", fn["arguments"])
	}
	if ch["finish_reason"] != "tool_calls" {
		t.Errorf("tool_use 应翻成 finish_reason=tool_calls，实际 %v", ch["finish_reason"])
	}
}

// TestTranslateNonStreamIdempotent 已是 OpenAI 形状时**原样返回**。
//
// 幂等很重要：重放/测试会把已经是 OpenAI 的体再喂一次，
// 二次翻译会把它搅烂。
func TestTranslateNonStreamIdempotent(t *testing.T) {
	in := `{"id":"x","object":"chat.completion","choices":[{"message":{"content":"hi"}}]}`
	out, err := TranslateNonStream([]byte(in), "m")
	if err != nil {
		t.Fatal(err)
	}
	if string(out) != in {
		t.Errorf("已是 OpenAI 形状应原样返回，实际 %s", out)
	}
}

// TestOpenAIFinishReasonMapping 停止原因映射表。
func TestOpenAIFinishReasonMapping(t *testing.T) {
	cases := map[string]string{
		"end_turn":      "stop",
		"stop_sequence": "stop",
		"max_tokens":    "length",
		"tool_use":      "tool_calls",
		"refusal":       "content_filter",
		"pause_turn":    "stop",
		"":              "",
		"unknown_thing": "",
	}
	for in, want := range cases {
		if got := openAIFinishReason(in); got != want {
			t.Errorf("finish_reason(%q) 应为 %q，实际 %q", in, want, got)
		}
	}
}

// TestBuildAnthropicImageURL 图片块翻译（URL 与 data: URI 两种来源）。
func TestBuildAnthropicImageURL(t *testing.T) {
	out := decodeOut(t, `{
		"messages":[{"role":"user","content":[
			{"type":"text","text":"看这张图"},
			{"type":"image_url","image_url":{"url":"https://example.com/a.png"}}
		]}]
	}`)
	msgs := out["messages"].([]any)
	blocks := msgs[0].(map[string]any)["content"].([]any)
	if len(blocks) != 2 {
		t.Fatalf("应有 2 个块，实际 %d", len(blocks))
	}
	img := blocks[1].(map[string]any)
	if img["type"] != "image" {
		t.Errorf("image_url 应翻成 image 块，实际 %v", img["type"])
	}
	src := img["source"].(map[string]any)
	if src["type"] != "url" || src["url"] != "https://example.com/a.png" {
		t.Errorf("URL 图应翻成 source.type=url，实际 %v", src)
	}

	// data: URI → base64
	out2 := decodeOut(t, `{
		"messages":[{"role":"user","content":[
			{"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}
		]}]
	}`)
	blocks2 := out2["messages"].([]any)[0].(map[string]any)["content"].([]any)
	src2 := blocks2[0].(map[string]any)["source"].(map[string]any)
	if src2["type"] != "base64" {
		t.Errorf("data: URI 应翻成 source.type=base64，实际 %v", src2)
	}
	if src2["media_type"] != "image/png" {
		t.Errorf("media_type 应从 data: URI 解出，实际 %v", src2["media_type"])
	}
	if src2["data"] != "AAAA" {
		t.Errorf("base64 数据错误: %v", src2["data"])
	}
}
