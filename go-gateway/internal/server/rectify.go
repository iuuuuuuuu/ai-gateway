package server

// rectify.go 请求整流：修复 Anthropic 客户端历史里的 tool 配对问题。
//
// 背景：Claude Desktop / Claude Code 的会话历史在上下文压缩、并发工具乱序
// 完成、中断恢复等情况下，会出现 assistant.tool_calls 与 role:tool 消息
// 不配对（缺失应答、孤儿结果、顺序错乱）。上游（CodeBuddy）对 tool 序列
// 校验严格，命中即 HTTP 400 code=11148（tool_call_sequence_broken）。
//
// 思路与 CC Switch（MIT）的整流器一致（sanitize_orphan_tool_results /
// drop_incomplete_tool_turns），但本实现直接作用于发往上游的 OpenAI Chat
// 消息序列，规则：
//  1. 无 tool 消息应答的 tool_call → 删除；删空且无正文的 assistant → 整条删除；
//  2. 无对应 tool_call 的孤儿 tool 消息 → 删除；同一 tool_call_id 的重复应答只留一条；
//  3. 应答消息归位到对应 assistant 正后方，并按 tool_calls 顺序稳定重排
//     （覆盖并发乱序与 assistant 被拆散两种形态）。
//
// 整流是幂等的：配对完好的请求原样返回（零拷贝，messages 切片不重建）。

import "sort"

// rectifyChatToolSequence 整流 OpenAI Chat messages 中的 tool 配对。
//
// 返回整流后的消息序列与修复动作计数（0 表示无需修改，调用方可据此打日志）。
// 消息内容一律不伪造：不做占位补齐，只做「删不配对、排对顺序」。
func rectifyChatToolSequence(messages []map[string]any) ([]map[string]any, int) {
	if len(messages) == 0 {
		return messages, 0
	}

	// 第一遍：收集全部 tool_call id 与全部 tool 应答 id。
	called := make(map[string]bool, 8)
	for _, m := range messages {
		if str(m["role"]) != "assistant" {
			continue
		}
		for _, call := range toolCallsOf(m) {
			cm, _ := call.(map[string]any)
			if id := str(cm["id"]); id != "" {
				called[id] = true
			}
		}
	}
	responded := make(map[string]bool, 8)
	for _, m := range messages {
		if str(m["role"]) != "tool" {
			continue
		}
		if id := str(m["tool_call_id"]); id != "" {
			responded[id] = true
		}
	}
	if len(called) == 0 && len(responded) == 0 {
		return messages, 0
	}

	// 第二遍：删除缺失应答的 tool_call、孤儿 tool 消息、重复应答与空 assistant。
	fixed := 0
	used := make(map[string]bool, len(responded))
	out := make([]map[string]any, 0, len(messages))
	for _, m := range messages {
		switch str(m["role"]) {
		case "tool":
			id := str(m["tool_call_id"])
			// 孤儿（无调用）或重复应答（同一 id 第二条起）一律丢弃。
			if id == "" || !called[id] || used[id] {
				fixed++
				continue
			}
			used[id] = true
			out = append(out, m)
		case "assistant":
			calls := toolCallsOf(m)
			if len(calls) == 0 {
				out = append(out, m)
				continue
			}
			kept := make([]any, 0, len(calls))
			seen := make(map[string]bool, len(calls))
			for _, call := range calls {
				cm, _ := call.(map[string]any)
				id := str(cm["id"])
				// 空 id、无应答、重复 id 的调用都删掉。
				if id == "" || !responded[id] || seen[id] {
					fixed++
					continue
				}
				seen[id] = true
				kept = append(kept, call)
			}
			if len(kept) == 0 {
				delete(m, "tool_calls")
				if contentEmpty(m) {
					fixed++
					continue
				}
				out = append(out, m)
				continue
			}
			if len(kept) != len(calls) {
				m["tool_calls"] = kept
			}
			out = append(out, m)
		default:
			out = append(out, m)
		}
	}

	// 第三遍：把应答 tool 消息归位到对应 assistant 正后方，并按 tool_calls
	// 顺序稳定重排。覆盖两种畸形形态：
	//   - 乱序：tool 消息都在紧跟段里，但顺序与调用顺序不一致（并发完成）；
	//   - 隔断：tool 消息落在更远处（如 assistant 被拆散成多条、结果被隔开）。
	// 配对完好且有序的请求在此零改动。
	consumed := make([]bool, len(out))
	rebuilt := make([]map[string]any, 0, len(out))
	for i := 0; i < len(out); i++ {
		if consumed[i] {
			continue
		}
		m := out[i]
		if str(m["role"]) != "assistant" || len(toolCallsOf(m)) == 0 {
			rebuilt = append(rebuilt, m)
			continue
		}
		calls := toolCallsOf(m)
		order := make(map[string]int, len(calls))
		for k, call := range calls {
			cm, _ := call.(map[string]any)
			if id := str(cm["id"]); id != "" {
				if _, dup := order[id]; !dup {
					order[id] = k
				}
			}
		}

		// 紧跟的连续 tool 段（仅收属于本 assistant 调用的应答；
		// 混入的其他应答留给其真正的 assistant 处理）。
		j := i + 1
		for j < len(out) && str(out[j]["role"]) == "tool" && !consumed[j] && orderHas(order, out[j]) {
			j++
		}
		seg := make([]map[string]any, 0, len(calls))
		for k := i + 1; k < j; k++ {
			consumed[k] = true
			seg = append(seg, out[k])
		}
		// 全局查找被隔断的应答（id 精确匹配，第二遍后 id 全局唯一）。
		if len(seg) < len(order) {
			have := make(map[string]bool, len(seg))
			for _, tm := range seg {
				have[str(tm["tool_call_id"])] = true
			}
			for k := 0; k < len(out) && len(seg) < len(order); k++ {
				if k == i || consumed[k] || have[str(out[k]["tool_call_id"])] {
					continue
				}
				if str(out[k]["role"]) != "tool" || !orderHas(order, out[k]) {
					continue
				}
				consumed[k] = true
				have[str(out[k]["tool_call_id"])] = true
				seg = append(seg, out[k])
				fixed++ // 被隔断的应答挪回 assistant 正后方
			}
		}
		sorted := append([]map[string]any(nil), seg...)
		sort.SliceStable(sorted, func(a, b int) bool {
			return order[str(sorted[a]["tool_call_id"])] < order[str(sorted[b]["tool_call_id"])]
		})
		for k := range sorted {
			if str(seg[k]["tool_call_id"]) != str(sorted[k]["tool_call_id"]) {
				fixed++
			}
		}
		rebuilt = append(rebuilt, m)
		rebuilt = append(rebuilt, sorted...)
		i = j - 1
	}
	if fixed == 0 {
		return messages, 0
	}
	return rebuilt, fixed
}

// orderHas 判断 tool 消息是否属于当前 assistant 的调用集合。
func orderHas(order map[string]int, m map[string]any) bool {
	id := str(m["tool_call_id"])
	if id == "" {
		return false
	}
	_, ok := order[id]
	return ok
}

// toolCallsOf 取 assistant 消息的 tool_calls 数组（缺省/类型不符时为空）。
//
// 兼容两种形态：JSON 解码产物是 []any，转换层直接构造的是 []map[string]any。
func toolCallsOf(m map[string]any) []any {
	switch calls := m["tool_calls"].(type) {
	case []any:
		return calls
	case []map[string]any:
		out := make([]any, len(calls))
		for i, c := range calls {
			out[i] = c
		}
		return out
	}
	return nil
}

// contentEmpty 判断消息正文是否为空（nil / 空串 / 空数组都算空）。
func contentEmpty(m map[string]any) bool {
	switch c := m["content"].(type) {
	case nil:
		return true
	case string:
		return c == ""
	case []any:
		return len(c) == 0
	}
	return false
}
