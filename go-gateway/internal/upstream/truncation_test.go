package upstream

// 截断 tool_call 参数的处理测试。
//
// # 缺陷
//
// 流式工具调用的 `arguments` 是**分片拼接**出来的。流若中途断掉
//（上游 reset / 客户端取消 / 上游超时），拼出来就是半截 JSON：
//
//	{"path":"D:\\a.txt","content":"这是一段很长
//
// 喂给客户端 → `JSON.parse` 抛异常 → **整个会话卡死**
//（工具调用既没成功也没失败，agent 循环停在原地等一个永不到来的结果）。
//
// # 两条必须同时成立的原则
//
//	① 残缺的**必须丢**（补 {} 会伪造"无参调用"，可能造成数据损坏）
//	② 合法的无参调用**必须留**（空串/null 是常态，误删会让正常工具失效）
//
// ② 比 ① 更容易写错：把"空参数"当残缺是最自然的实现，而那会
// 误删所有不需要参数的工具调用。

import (
	"strings"
	"testing"
)

// TestIsTruncatedArgumentsEmptyIsNotTruncated 空参数**不是**残缺。
//
// ⚠ 这条是最容易写错的：直觉上"没有参数"看起来像"没收到参数"，
// 但空串是**合法**的无参调用（如 `list_files`）。
// 判成残缺会误删所有无参工具调用 —— 症状是"某些工具突然不工作了"，
// 而排查方向会被引向"模型不调用工具"，完全错位。
func TestIsTruncatedArgumentsEmptyIsNotTruncated(t *testing.T) {
	for _, raw := range []string{"", "   ", "\n\t", "\r\n"} {
		if isTruncatedArguments(raw) {
			t.Errorf("%q 是合法的无参调用，不该判成残缺（误删会让无参工具失效）", raw)
		}
	}
}

// TestIsTruncatedArgumentsNullIsNotTruncated null 也是合法的。
//
// JSON 标准允许 `null`，有些上游对无参调用就发它。
// 若额外校验"必须解析成对象"，会误删这类调用。
func TestIsTruncatedArgumentsNullIsNotTruncated(t *testing.T) {
	if isTruncatedArguments("null") {
		t.Error("null 是合法的 JSON（有些上游用它表示无参调用），不该判成残缺")
	}
}

// TestIsTruncatedArgumentsValidJSONIsNotTruncated 能解析的就是完整的。
func TestIsTruncatedArgumentsValidJSONIsNotTruncated(t *testing.T) {
	valid := []string{
		`{}`,
		`{"path":"a.txt"}`,
		`{"nested":{"a":[1,2,3]}}`,
		`"just a string"`, // 工具参数偶尔是裸字符串
		`123`,
	}
	for _, raw := range valid {
		if isTruncatedArguments(raw) {
			t.Errorf("%q 是合法 JSON，不该判成残缺", raw)
		}
	}
}

// TestIsTruncatedArgumentsHalfJSONIsTruncated 半截 JSON 必须判成残缺。
func TestIsTruncatedArgumentsHalfJSONIsTruncated(t *testing.T) {
	truncated := []string{
		`{"path":"D:\\a.txt","content":"这是一段很长`, // 少了收尾引号与括号
		`{"a":1`,          // 少括号
		`{"a":`,           // 值都没了
		`{"a":"b"`,        // 少一个 }
		`[1,2,`,           // 数组没闭合
		`{"a":"未转义的"引号"}`, // 非法转义
	}
	for _, raw := range truncated {
		if !isTruncatedArguments(raw) {
			t.Errorf("%q 是半截 JSON，必须判成残缺（否则客户端 JSON.parse 抛异常、会话卡死）", raw)
		}
	}
}

// TestDropTruncatedKeepsValidOnes 只丢残缺的，完整的原样保留。
func TestDropTruncatedKeepsValidOnes(t *testing.T) {
	calls := []map[string]any{
		{"function": map[string]any{"name": "read", "arguments": `{"path":"a.txt"}`}},
		{"function": map[string]any{"name": "write", "arguments": `{"path":"b.txt","content":"半截`}},
		{"function": map[string]any{"name": "list", "arguments": ""}},
	}
	kept, dropped := dropTruncatedToolCalls(calls)

	if dropped != 1 {
		t.Errorf("应丢弃 1 个（write），实际 %d", dropped)
	}
	if len(kept) != 2 {
		t.Fatalf("应保留 2 个，实际 %d", len(kept))
	}
	// 保留的必须是 read 与 list，且顺序不变
	if nameOf(kept[0]) != "read" {
		t.Errorf("第 1 个应是 read，实际 %s", nameOf(kept[0]))
	}
	if nameOf(kept[1]) != "list" {
		t.Errorf("第 2 个应是 list（无参调用必须保留），实际 %s", nameOf(kept[1]))
	}
}

// TestDropTruncatedAllDroppedReturnsNil 全被丢弃时返回 nil（不是空切片）。
//
// 理由：nil 在 JSON 里让 `tool_calls` 字段整体消失，而空切片是 `[]`。
// 客户端对 `tool_calls: []` 有时会走进"有工具调用但为空"的分支，
// 与"没有工具调用"是不同的语义。
func TestDropTruncatedAllDroppedReturnsNil(t *testing.T) {
	calls := []map[string]any{
		{"function": map[string]any{"name": "a", "arguments": `{"x":`}},
		{"function": map[string]any{"name": "b", "arguments": `{"y"`}},
	}
	kept, dropped := dropTruncatedToolCalls(calls)
	if dropped != 2 {
		t.Errorf("应丢弃 2 个，实际 %d", dropped)
	}
	if kept != nil {
		t.Errorf("全丢弃时应返回 nil（让字段消失）而不是空切片，实际 %#v", kept)
	}
}

// TestDropTruncatedHandlesAlternateShapes 三种 arguments 形态都要认。
func TestDropTruncatedHandlesAlternateShapes(t *testing.T) {
	// 形态 2：arguments 直接是对象（上游已解析过）→ 不可能残缺
	obj := map[string]any{"function": map[string]any{
		"name": "x", "arguments": map[string]any{"path": "a.txt"},
	}}
	// 形态 3：没有 function 包裹
	bare := map[string]any{"name": "y", "arguments": `{"ok":1}`}
	// 形态 4：显式 null → 按无参处理
	nullArgs := map[string]any{"function": map[string]any{"name": "z", "arguments": nil}}

	kept, dropped := dropTruncatedToolCalls([]map[string]any{obj, bare, nullArgs})
	if dropped != 0 {
		t.Errorf("这三种形态都不该被判成残缺，实际丢弃 %d 个", dropped)
	}
	if len(kept) != 3 {
		t.Errorf("应保留 3 个，实际 %d", len(kept))
	}
}

// TestDropTruncatedEmptyInput 空输入不出错。
func TestDropTruncatedEmptyInput(t *testing.T) {
	kept, dropped := dropTruncatedToolCalls(nil)
	if dropped != 0 || kept != nil {
		t.Errorf("空输入应原样返回，实际 kept=%v dropped=%d", kept, dropped)
	}
}

// TestAggregateDropsTruncatedToolCalls **端到端**：从真实 SSE 流聚合时也丢。
//
// 前面几条测的是纯函数；这条测"它真的被接进聚合路径了"——
// 纯函数写对但忘了接线，是这类修复最常见的失败方式。
func TestAggregateDropsTruncatedToolCalls(t *testing.T) {
	// 流被截断：arguments 拼到一半就 [DONE] 了
	stream := "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{" +
		"\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"path\\\":\\\"a.txt\\\"\"}}]}}]}\n\n" +
		"data: [DONE]\n\n"

	resp, err := Aggregate(strings.NewReader(stream))
	if err != nil {
		t.Fatalf("聚合失败: %v", err)
	}
	msg, _ := resp["choices"].([]any)[0].(map[string]any)["message"].(map[string]any)
	if tcs, ok := msg["tool_calls"]; ok {
		t.Errorf("半截参数的 tool_call 必须被丢弃，实际仍存在：%#v", tcs)
	}
	// finish_reason 也要从 tool_calls 改掉 —— 否则客户端会去找
	// 一个不存在的 tool_calls 字段，可能再次卡住
	finish := resp["choices"].([]any)[0].(map[string]any)["finish_reason"]
	if finish == "tool_calls" {
		t.Error("所有调用被丢弃后 finish_reason 不能还是 tool_calls（客户端会去找不存在的字段）")
	}
}

// TestAggregateKeepsValidToolCalls 合法的工具调用**必须保留**（回归）。
//
// 上面那条测"该丢的丢了"，这条测"不该丢的没丢" ——
// 缺了它，一个"永远丢弃所有 tool_calls"的实现也能让上面那条通过。
func TestAggregateKeepsValidToolCalls(t *testing.T) {
	stream := "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{" +
		"\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"a.txt\\\"}\"}}]}}]}\n\n" +
		"data: [DONE]\n\n"

	resp, err := Aggregate(strings.NewReader(stream))
	if err != nil {
		t.Fatalf("聚合失败: %v", err)
	}
	msg, _ := resp["choices"].([]any)[0].(map[string]any)["message"].(map[string]any)
	tcs, ok := msg["tool_calls"].([]map[string]any)
	if !ok || len(tcs) != 1 {
		t.Fatalf("合法的 tool_call 必须保留，实际 %#v", msg["tool_calls"])
	}
	if nameOf(tcs[0]) != "read" {
		t.Errorf("工具名应是 read，实际 %s", nameOf(tcs[0]))
	}
}

// nameOf 取 tool_call 里的函数名（测试辅助）。
func nameOf(call map[string]any) string {
	if fn, ok := call["function"].(map[string]any); ok {
		if s, ok := fn["name"].(string); ok {
			return s
		}
	}
	if s, ok := call["name"].(string); ok {
		return s
	}
	return ""
}
