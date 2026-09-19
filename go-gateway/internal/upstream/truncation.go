package upstream

import (
	"encoding/json"
	"strings"
)

// truncation.go 丢弃**被截断的** tool_call 参数。
//
// # 问题
//
// 流式工具调用里，`arguments` 是**分片拼接**出来的。若流在中途断掉
//（连接被上游 reset、客户端取消、上游内部超时），拼出来的就是一个
// **半截 JSON**，例如：
//
//	{"path":"D:\\a.txt","content":"这是一段很长
//
// 把它喂给客户端，客户端 `JSON.parse` 抛异常 —— 表现是**整个会话卡死**
//（工具调用既没成功也没失败，agent 循环停在原地等一个永远不会来的结果）。
// 用户看到的是"卡住了"，而根因在上游那条流断了一半。
//
// # 为什么不补成 `{}` 让它"看起来合法"
//
// 补 `{}` 会把"参数丢了"伪装成"工具被无参调用"：
//
//	· 写文件变成写空内容、删除变成删默认路径 —— **可能造成数据损坏**
//	· 客户端不会报错，用户以为工具正常执行了
//
// 丢弃则是**显式失败**：客户端看到这个 tool_call 不见了，
// 会知道"这一步没成"并自行重试。宁可少一个调用，也不能伪造一个。

// isTruncatedArguments 判断 tool_call 的 arguments 是否是**残缺**的。
//
// 三种情况：
//
//	空串 / 纯空白        → **不是**残缺（合法的"无参数调用"）
//	非空且能解析成 JSON  → 不是残缺
//	非空但解析失败       → **是残缺**（拼了一半）
//
// ⚠ 为什么不校验"必须解析成对象"：JSON 标准允许 `null`，而有些上游对
// 无参调用发 `null`。把它判成残缺会**误删合法的无参调用**。
// 只要能被 JSON 解析器接受，就认为它是完整的。
func isTruncatedArguments(raw string) bool {
	trimmed := strings.TrimSpace(raw)
	if trimmed == "" {
		// 空 = 还没收到任何参数分片。这可能是：
		//   (a) 合法的无参调用（工具确实不需要参数）
		//   (b) 流在参数到达前就断了
		//
		// 两者**无法区分**，而 (a) 是常态（例如 `list_files` 无参）。
		// 故按 (a) 处理 —— 宁可留一个可能无参的调用，也不要删掉合法调用。
		return false
	}
	var probe any
	return json.Unmarshal([]byte(trimmed), &probe) != nil
}

// dropTruncatedToolCalls 从 tool_calls 里剔除参数残缺的那些。
//
// 返回**新的**切片（不改动入参），并同时返回被丢弃的条数 ——
// 调用方用它在日志里说明"这次丢了几个调用"，否则用户只会看到
// "工具调用少了"却不知为什么。
//
// 全部被丢弃时返回 nil（而不是空切片）：JSON 序列化时 nil 会变成
// `null` 或整个字段消失，而空切片是 `[]`。前者才是"没有工具调用"的
// 正确形状（客户端对 `tool_calls: []` 的处理有时会走进另一条分支）。
func dropTruncatedToolCalls(calls []map[string]any) (kept []map[string]any, dropped int) {
	if len(calls) == 0 {
		return calls, 0
	}
	for _, c := range calls {
		if c == nil {
			dropped++
			continue
		}
		if isTruncatedArguments(argumentsOf(c)) {
			dropped++
			continue
		}
		kept = append(kept, c)
	}
	if len(kept) == 0 {
		return nil, dropped
	}
	return kept, dropped
}

// argumentsOf 取 tool_call 里的 arguments 字符串。
//
// 上游三种形态都出现过（与 `indexOfToolCall` 同一情况）：
//
//	{"function":{"arguments":"{...}"}}   标准
//	{"function":{"arguments":{...}}}     直接给对象（少数实现）
//	{"arguments":"{...}"}                没有 function 包裹
func argumentsOf(call map[string]any) string {
	if fn, ok := call["function"].(map[string]any); ok {
		switch a := fn["arguments"].(type) {
		case string:
			return a
		case map[string]any:
			// 已经是对象 = 上游自己解析过了，不可能"残缺"
			return "{}"
		case nil:
			// 显式 null：按空串处理（见 isTruncatedArguments 的说明）
			return ""
		}
	}
	if s, ok := call["arguments"].(string); ok {
		return s
	}
	return ""
}
