package server

import (
	"testing"
)

// ---------------------------------------------------------------------------
// Bug B/C：Anthropic tool_use 内容块的协议顺序与 stop_reason 口径
// ---------------------------------------------------------------------------

// anthropicBlockEvents 从 Anthropic SSE 事件里抽出内容块相关事件，校验协议顺序：
// 每个 content_block_delta 都必须落在已 content_block_start 过的 index 上。
func assertBlocksOrdered(t *testing.T, events []map[string]any) {
	t.Helper()
	started := map[int]bool{}
	for _, e := range events {
		switch str(e["type"]) {
		case "content_block_start":
			started[numOf(e["index"])] = true
		case "content_block_delta":
			idx := numOf(e["index"])
			if !started[idx] {
				t.Errorf("content_block_delta 落在未 start 的 index=%d 上（Anthropic 协议违规）", idx)
			}
		case "content_block_stop":
			idx := numOf(e["index"])
			if !started[idx] {
				t.Errorf("content_block_stop 落在未 start 的 index=%d 上", idx)
			}
		}
	}
}

// 上游允许 function.arguments 先于 function.name 到达（分片顺序由上游写入决定）。
// 修复前：name 未知时块无法开启，但参数分支没有守卫，直接发了 input_json_delta
// → 事件顺序变成 [delta, START, delta, stop]，客户端按协议拒绝该工具块。
// 修复后：参数先暂存，name 到达并发出 start 后再补发，顺序恒为 start→delta。
func TestMessagesToolArgsBeforeNameKeepsOrder(t *testing.T) {
	sse := chatSSE(
		// 先到 arguments（name 未知）。
		`{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"cmd\":"}}]}}]}`,
		// 再到 id + name。
		`{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_x","function":{"name":"Bash"}}]}}]}`,
		// 后续参数。
		`{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"ls\"}"}}]}}]}`,
		`{"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}`,
	)
	events := postStream(t, sse, "/v1/messages",
		`{"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}`)

	assertBlocksOrdered(t, events)

	// 必须真的开出了一个 tool_use 块，且参数内容完整（不因重排而丢）。
	var order []string
	args := ""
	stop := ""
	for _, e := range events {
		switch str(e["type"]) {
		case "content_block_start":
			order = append(order, "start")
			if cb, ok := e["content_block"].(map[string]any); ok {
				if str(cb["type"]) != "tool_use" {
					t.Errorf("块类型=%q want tool_use", str(cb["type"]))
				}
				if str(cb["name"]) != "Bash" {
					t.Errorf("工具名=%q want Bash", str(cb["name"]))
				}
			}
		case "content_block_delta":
			order = append(order, "delta")
			if d, ok := e["delta"].(map[string]any); ok {
				args += str(d["partial_json"])
			}
		case "message_delta":
			if d, ok := e["delta"].(map[string]any); ok {
				stop = str(d["stop_reason"])
			}
		}
	}
	t.Logf("事件顺序=%v 参数=%q stop_reason=%q", order, args, stop)

	if order[0] != "start" {
		t.Errorf("首个块事件应为 start，实际 %v", order)
	}
	if args != `{"cmd":"ls"}` {
		t.Errorf("工具参数不完整或有误: %q want %q", args, `{"cmd":"ls"}`)
	}
	if stop != "tool_use" {
		t.Errorf("stop_reason=%q want tool_use", stop)
	}
}

// 上游只发 arguments（或只有 index）却始终没给 name 时，块永远开不起来，
// 客户端一个 tool_use 块都收不到。修复前 stopReason() 只看 toolOrder 非空
// 就回 tool_use，等于让客户端去执行一个它没收到的工具，只能卡住。
// 修复后：没有任何块真正开启时退化为 end_turn。
func TestMessagesUnnamedToolCallDoesNotClaimToolUse(t *testing.T) {
	sse := chatSSE(
		`{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_y","function":{"arguments":"{\"a\":1}"}}]}}]}`,
		`{"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}`,
	)
	events := postStream(t, sse, "/v1/messages",
		`{"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}`)

	assertBlocksOrdered(t, events)

	starts, deltas, stop := 0, 0, ""
	for _, e := range events {
		switch str(e["type"]) {
		case "content_block_start":
			starts++
		case "content_block_delta":
			deltas++
		case "message_delta":
			if d, ok := e["delta"].(map[string]any); ok {
				stop = str(d["stop_reason"])
			}
		}
	}
	t.Logf("start=%d delta=%d stop_reason=%q", starts, deltas, stop)

	if starts == 0 && stop == "tool_use" {
		t.Errorf("没有任何 tool_use 块却回 stop_reason=tool_use（客户端会卡住）")
	}
	if starts == 0 && deltas > 0 {
		t.Errorf("发出了 %d 个孤儿 delta 却没有 start", deltas)
	}
}

// 正常顺序（name 先到）不得因本次修复而回归：参数仍要完整下发。
func TestMessagesNormalToolCallStillWorks(t *testing.T) {
	sse := chatSSE(
		`{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_z","function":{"name":"Read","arguments":"{\"p\":"}}]}}]}`,
		`{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.txt\"}"}}]}}]}`,
		`{"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}`,
	)
	events := postStream(t, sse, "/v1/messages",
		`{"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}`)

	assertBlocksOrdered(t, events)

	args, stop, blocks := "", "", 0
	for _, e := range events {
		switch str(e["type"]) {
		case "content_block_start":
			blocks++
		case "content_block_delta":
			if d, ok := e["delta"].(map[string]any); ok {
				args += str(d["partial_json"])
			}
		case "message_delta":
			if d, ok := e["delta"].(map[string]any); ok {
				stop = str(d["stop_reason"])
			}
		}
	}
	if blocks != 1 {
		t.Errorf("工具块数=%d want 1", blocks)
	}
	if args != `{"p":"a.txt"}` {
		t.Errorf("参数=%q want %q", args, `{"p":"a.txt"}`)
	}
	if stop != "tool_use" {
		t.Errorf("stop_reason=%q want tool_use", stop)
	}
}

// Responses 适配层同源缺陷：arguments.delta 不得先于 output_item.added。
func TestResponsesToolArgsBeforeNameKeepsOrder(t *testing.T) {
	sse := chatSSE(
		`{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"cmd\":"}}]}}]}`,
		`{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_x","function":{"name":"Bash"}}]}}]}`,
		`{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"ls\"}"}}]}}]}`,
		`{"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}`,
	)
	events := postStream(t, sse, "/v1/responses", `{"model":"m","stream":true,"input":"hi"}`)

	added := map[int]bool{}
	args := ""
	for _, e := range events {
		switch str(e["type"]) {
		case "response.output_item.added":
			if item, ok := e["item"].(map[string]any); ok && str(item["type"]) == "function_call" {
				added[numOf(e["output_index"])] = true
			}
		case "response.function_call_arguments.delta":
			idx := numOf(e["output_index"])
			if !added[idx] {
				t.Errorf("arguments.delta 落在未 added 的 output_index=%d 上", idx)
			}
			args += str(e["delta"])
		}
	}
	if args != `{"cmd":"ls"}` {
		t.Errorf("工具参数不完整: %q want %q", args, `{"cmd":"ls"}`)
	}
}
