package server

import "testing"

// ---------------------------------------------------------------------------
// tool 序列整流
// ---------------------------------------------------------------------------

func rectifyToolCall(id string) map[string]any {
	return map[string]any{
		"id":   id,
		"type": "function",
		"function": map[string]any{
			"name":      "Bash",
			"arguments": "{}",
		},
	}
}

func rectifyToolResult(id, content string) map[string]any {
	return map[string]any{"role": "tool", "tool_call_id": id, "content": content}
}

func rectifySeq(msgs ...map[string]any) []string {
	out := make([]string, 0, len(msgs))
	for _, m := range msgs {
		switch str(m["role"]) {
		case "tool":
			out = append(out, "tool:"+str(m["tool_call_id"]))
		case "assistant":
			if calls := toolCallsOf(m); len(calls) > 0 {
				ids := ""
				for _, c := range calls {
					cm, _ := c.(map[string]any)
					if ids != "" {
						ids += ","
					}
					ids += str(cm["id"])
				}
				out = append(out, "assistant:"+ids)
			} else {
				out = append(out, "assistant:"+str(m["content"]))
			}
		default:
			out = append(out, str(m["role"])+":"+str(m["content"]))
		}
	}
	return out
}

func assertSeq(t *testing.T, got []string, want ...string) {
	t.Helper()
	if len(got) != len(want) {
		t.Fatalf("sequence = %v, want %v", got, want)
	}
	for i := range want {
		if got[i] != want[i] {
			t.Fatalf("sequence[%d] = %q, want %q (full: %v)", i, got[i], want[i], got)
		}
	}
}

// 并发工具完成顺序与调用顺序不一致 → tool 消息按 tool_calls 顺序重排。
func TestRectifyReordersToolResults(t *testing.T) {
	msgs := []map[string]any{
		{"role": "user", "content": "run"},
		{"role": "assistant", "content": nil, "tool_calls": []any{
			rectifyToolCall("c1"), rectifyToolCall("c2"), rectifyToolCall("c3"),
		}},
		rectifyToolResult("c1", "r1"),
		rectifyToolResult("c3", "r3"),
		rectifyToolResult("c2", "r2"),
	}
	out, fixed := rectifyChatToolSequence(msgs)
	if fixed == 0 {
		t.Fatal("expected reorder fix")
	}
	assertSeq(t, rectifySeq(out...),
		"user:run",
		"assistant:c1,c2,c3",
		"tool:c1", "tool:c2", "tool:c3",
	)
}

// assistant 被拆散（同一响应的多个 tool_use 各成一条消息）→ 应答归位。
func TestRectifyMovesSeparatedToolResults(t *testing.T) {
	msgs := []map[string]any{
		{"role": "assistant", "content": nil, "tool_calls": []any{rectifyToolCall("c1")}},
		{"role": "assistant", "content": nil, "tool_calls": []any{rectifyToolCall("c2")}},
		rectifyToolResult("c2", "r2"),
		rectifyToolResult("c1", "r1"),
	}
	out, fixed := rectifyChatToolSequence(msgs)
	if fixed == 0 {
		t.Fatal("expected move fix")
	}
	assertSeq(t, rectifySeq(out...),
		"assistant:c1", "tool:c1",
		"assistant:c2", "tool:c2",
	)
}

// 无应答的 tool_call → 删除；只剩该调用的 assistant → 整条删除。
func TestRectifyDropsUnansweredCall(t *testing.T) {
	msgs := []map[string]any{
		{"role": "user", "content": "run"},
		{"role": "assistant", "content": nil, "tool_calls": []any{
			rectifyToolCall("c1"), rectifyToolCall("c2"),
		}},
		rectifyToolResult("c1", "r1"),
		{"role": "assistant", "content": nil, "tool_calls": []any{rectifyToolCall("c9")}},
		{"role": "user", "content": "next"},
	}
	out, fixed := rectifyChatToolSequence(msgs)
	if fixed == 0 {
		t.Fatal("expected drop fix")
	}
	assertSeq(t, rectifySeq(out...),
		"user:run",
		"assistant:c1",
		"tool:c1",
		"user:next",
	)
}

// 带正文的 assistant 只剩被删调用时：保留正文，删除 tool_calls 字段。
func TestRectifyKeepsAssistantTextWhenCallsDropped(t *testing.T) {
	msgs := []map[string]any{
		{"role": "assistant", "content": "let me check", "tool_calls": []any{rectifyToolCall("gone")}},
		{"role": "user", "content": "hi"},
	}
	out, fixed := rectifyChatToolSequence(msgs)
	if fixed == 0 {
		t.Fatal("expected drop fix")
	}
	assertSeq(t, rectifySeq(out...), "assistant:let me check", "user:hi")
	if _, ok := out[0]["tool_calls"]; ok {
		t.Fatal("tool_calls should be removed")
	}
}

// 孤儿 tool 消息（无对应调用）→ 删除。
func TestRectifyDropsOrphanToolMessage(t *testing.T) {
	msgs := []map[string]any{
		{"role": "user", "content": "run"},
		rectifyToolResult("orphan", "stale"),
		{"role": "user", "content": "next"},
	}
	out, fixed := rectifyChatToolSequence(msgs)
	if fixed == 0 {
		t.Fatal("expected orphan drop")
	}
	assertSeq(t, rectifySeq(out...), "user:run", "user:next")
}

// 同一 tool_call_id 的重复应答只保留第一条。
func TestRectifyDropsDuplicateToolResult(t *testing.T) {
	msgs := []map[string]any{
		{"role": "assistant", "content": nil, "tool_calls": []any{rectifyToolCall("c1")}},
		rectifyToolResult("c1", "first"),
		rectifyToolResult("c1", "duplicate"),
	}
	out, fixed := rectifyChatToolSequence(msgs)
	if fixed == 0 {
		t.Fatal("expected duplicate drop")
	}
	assertSeq(t, rectifySeq(out...), "assistant:c1", "tool:c1")
	if out[1]["content"] != "first" {
		t.Fatalf("kept result = %v, want first", out[1]["content"])
	}
}

// 配对完好的序列零改动（幂等）。
func TestRectifyKeepsHealthySequence(t *testing.T) {
	msgs := []map[string]any{
		{"role": "user", "content": "run"},
		{"role": "assistant", "content": nil, "tool_calls": []any{rectifyToolCall("c1"), rectifyToolCall("c2")}},
		rectifyToolResult("c1", "r1"),
		rectifyToolResult("c2", "r2"),
		{"role": "user", "content": "next"},
	}
	out, fixed := rectifyChatToolSequence(msgs)
	if fixed != 0 {
		t.Fatalf("healthy sequence changed: fixed=%d", fixed)
	}
	if &out[0] != &msgs[0] {
		t.Fatal("healthy sequence should be returned as-is")
	}
}

// 无 tool 相关消息的普通请求不做任何处理。
func TestRectifySkipsPlainMessages(t *testing.T) {
	msgs := []map[string]any{
		{"role": "system", "content": "s"},
		{"role": "user", "content": "hi"},
	}
	_, fixed := rectifyChatToolSequence(msgs)
	if fixed != 0 {
		t.Fatalf("plain messages changed: fixed=%d", fixed)
	}
}

// ---------------------------------------------------------------------------
// Anthropic → Chat 转换与整流串联
// ---------------------------------------------------------------------------

// 混合 text + tool_result 的 user 消息：tool 消息必须紧跟 assistant，正文在后。
func TestAnthropicToChatKeepsToolAdjacentToAssistant(t *testing.T) {
	raw := []byte(`{
		"model":"m",
		"messages":[
			{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{}}]},
			{"role":"user","content":[{"type":"text","text":"继续"},{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}
		]
	}`)
	body, _, err := anthropicToChat(raw)
	if err != nil {
		t.Fatalf("anthropicToChat: %v", err)
	}
	var out map[string]any
	if err := jsonUnmarshal(string(body), &out); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	msgs, _ := out["messages"].([]any)
	got := make([]string, 0, len(msgs))
	for _, mi := range msgs {
		m, _ := mi.(map[string]any)
		got = append(got, str(m["role"]))
	}
	want := []string{"assistant", "tool", "user"}
	if len(got) != len(want) {
		t.Fatalf("roles = %v, want %v", got, want)
	}
	for i := range want {
		if got[i] != want[i] {
			t.Fatalf("roles[%d] = %q, want %q (full: %v)", i, got[i], want[i], got)
		}
	}
}

// 端到端：Claude Desktop 式「乱序 + 缺结果」历史，经转换 + 整流后配对完整。
func TestAnthropicToChatRectifiesBrokenHistory(t *testing.T) {
	raw := []byte(`{
		"model":"m",
		"messages":[
			{"role":"user","content":"build"},
			{"role":"assistant","content":[
				{"type":"tool_use","id":"c1","name":"Bash","input":{"cmd":"a"}},
				{"type":"tool_use","id":"c2","name":"Bash","input":{"cmd":"b"}},
				{"type":"tool_use","id":"c3","name":"Bash","input":{"cmd":"c"}}
			]},
			{"role":"user","content":[{"type":"tool_result","tool_use_id":"c1","content":"done1"}]},
			{"role":"user","content":[{"type":"tool_result","tool_use_id":"c3","content":"done3"}]},
			{"role":"user","content":[{"type":"tool_result","tool_use_id":"c2","content":"done2"}]}
		]
	}`)
	body, _, err := anthropicToChat(raw)
	if err != nil {
		t.Fatalf("anthropicToChat: %v", err)
	}
	var out map[string]any
	if err := jsonUnmarshal(string(body), &out); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	msgs, _ := out["messages"].([]any)
	mapped := make([]map[string]any, 0, len(msgs))
	for _, mi := range msgs {
		m, _ := mi.(map[string]any)
		mapped = append(mapped, m)
	}
	assertSeq(t, rectifySeq(mapped...),
		"user:build",
		"assistant:c1,c2,c3",
		"tool:c1", "tool:c2", "tool:c3",
	)
}

// ---------------------------------------------------------------------------
// message id 唯一性
// ---------------------------------------------------------------------------

func TestNewMessageIDUnique(t *testing.T) {
	seen := make(map[string]bool, 256)
	for i := 0; i < 256; i++ {
		id := newMessageID()
		if len(id) != 28 || id[:4] != "msg_" {
			t.Fatalf("id = %q, want msg_ + 24 hex", id)
		}
		if seen[id] {
			t.Fatalf("duplicate id: %q", id)
		}
		seen[id] = true
	}
}

func TestChatToAnthropicGeneratesUniqueID(t *testing.T) {
	mk := func() map[string]any {
		return map[string]any{
			"id":    "chatcmpl-wb2api",
			"model": "m",
			"choices": []any{
				map[string]any{
					"index":         float64(0),
					"message":       map[string]any{"role": "assistant", "content": "hi"},
					"finish_reason": "stop",
				},
			},
		}
	}
	a := chatToAnthropic(mk(), "")
	b := chatToAnthropic(mk(), "")
	if a["id"] == "chatcmpl-wb2api" {
		t.Fatal("upstream constant id leaked to client")
	}
	if a["id"] == b["id"] {
		t.Fatalf("message id must be unique per response, got %v twice", a["id"])
	}
}

func TestAnthropicStreamStateGeneratesUniqueID(t *testing.T) {
	a := newAnthropicStreamState("m")
	b := newAnthropicStreamState("m")
	if a.messageID == "msg_wb2api" || a.messageID == b.messageID {
		t.Fatalf("stream message id not unique: %q vs %q", a.messageID, b.messageID)
	}
}
