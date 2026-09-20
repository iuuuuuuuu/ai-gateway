package zcode

// translate.go OpenAI ↔ Anthropic Messages 的**双向翻译**。
//
// # 为什么必须有这一层（2026-09-20 抓包定案）
//
// 网关对**外**是 OpenAI 协议（客户端按它发请求），而 ZCode 上游是
// **Anthropic Messages** 协议。此前本包"零翻译"的假设是错的：
//
//	我们打的:  open.bigmodel.cn/api/coding/paas/v4/chat/completions  (OpenAI)
//	           → 恒回 429 {"code":1113,"msg":"余额不足或无可用资源包,…"}
//	官方打的:  zcode.z.ai/api/v1/zcode-plan/anthropic/v1/messages     (Anthropic)
//	           → HTTP 200，流式正常
//
// 那个 `1113` **不是"账号没钱"**（账号有 300 万 + 500 万 token/日），
// 而是"**这条通道没有该账号的资源包**" —— 因为额度挂在 start-plan 上，
// 而我们把请求发到了按量计费通道。
//
// 抓包实测的官方 provider 清单里，**所有** provider 的 `schema` 都是
// `anthropic`（含 coding-plan 那两个）；`openai:chat` 只出现在
// `templateRules`（给"自定义 provider"用的模板）。故 anthropic 才是
// ZCode 的正路。
//
// # 协议差异（照抓包逐字对齐）
//
//	维度        OpenAI                       Anthropic
//	──────────  ───────────────────────────  ────────────────────────────────
//	system      messages 里 role:"system"    **顶层** system 字段（可以是数组）
//	消息内容    content 是字符串或数组        content **必须是块数组**
//	工具         tools[].function.{name,…}    tools[].{name,description,input_schema}
//	工具调用     assistant.tool_calls          assistant.content[].type="tool_use"
//	工具结果     role:"tool"                  user.content[].type="tool_result"
//	上限         max_tokens 可选              max_tokens **必填**
//	停止         stop                        stop_sequences
//	思考         reasoning_content（非标准）  thinking + output_config.effort
//	响应         choices[].delta              event: content_block_delta 等
//	结束标记     data: [DONE]                 event: message_stop
//
// # 翻译错了不会报错
//
// 只会让用户看到空回答、或上游 400。故**两个方向都有单测** ——
// 这是继承 qoder/translate.go 的做法（那边也是 164 行 + 395 行测试）。

import (
	"bufio"
	"bytes"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"strings"
	"time"
)

// DefaultMaxTokens 上游 `max_tokens` 的默认值。
//
// Anthropic 协议**必填** max_tokens，而 OpenAI 客户端经常不传。
// 取 builtinModels 里两个模型的 `maxCompletionTokens`（实测都是 128000）。
//
// ⚠ 这是**上限声明**而非"要生成这么多" —— 上游按实际生成计费。
const DefaultMaxTokens = 128000

// ---------------------------------------------------------------------------
// 方向一：OpenAI 请求 → Anthropic 请求
// ---------------------------------------------------------------------------

// BuildAnthropicBody 把 OpenAI 的 chat 请求体翻成 Anthropic Messages 请求体。
//
// model 是**已规范化的上游模型名**（如 `GLM-5.3-Flash`，注意官方用
// 展示名的大小写，不是目录里的 `glm-5.3-flash`）。
func BuildAnthropicBody(openAIBody []byte, model string) ([]byte, error) {
	return BuildAnthropicBodyWithMeta(openAIBody, model, AnthropicMeta{})
}

// AnthropicMeta 官方请求体里的**会话元数据**（抓包实测必发）。
//
// # 为什么必须发（这不是可有可无的装饰）
//
// 官方客户端每次对话都在请求体里带：
//
//	"metadata": {"user_id": "{\"device_id\":\"…\",\"account_uuid\":\"\",\"session_id\":\"…\"}"}
//
// 注意 `user_id` 的值本身是一个**序列化后的 JSON 字符串**（双重编码）——
// 这是官方 SDK 的写法，照抄。
//
// 它给上游提供了"这是哪台设备、哪个会话"的维度。缺失时上游少了一个
// 用于关联请求与判定行为模式的信号 —— 而风控恰恰靠这类信号区分
// "真实客户端"与"脚本"。故即便它不影响认证，也应当发。
type AnthropicMeta struct {
	// DeviceID 设备标识（官方用 x-device-mid 那个值）。
	DeviceID string
	// AccountUUID 账号 uuid（官方抓包里是空串）。
	AccountUUID string
	// SessionID 会话标识。
	SessionID string
}

// BuildAnthropicBodyWithMeta 同 BuildAnthropicBody，但可注入会话元数据。
func BuildAnthropicBodyWithMeta(openAIBody []byte, model string, meta AnthropicMeta) ([]byte, error) {
	var in map[string]any
	if err := json.Unmarshal(openAIBody, &in); err != nil {
		return nil, fmt.Errorf("解析 OpenAI 请求体失败: %w", err)
	}

	out := map[string]any{"model": model}

	// ---- system：从 messages 里抽出来，放到顶层 ----
	//
	// Anthropic 的 system 是**顶层字段**，且可以是字符串或块数组。
	// 我们用块数组（与抓包一致，且支持 cache_control）。
	var systemBlocks []any
	rawMsgs, _ := in["messages"].([]any)
	msgs := make([]any, 0, len(rawMsgs))

	for _, rm := range rawMsgs {
		m, ok := rm.(map[string]any)
		if !ok {
			continue
		}
		role, _ := m["role"].(string)
		switch role {
		case "system", "developer":
			// OpenAI 的 system（含新的 developer 角色）→ Anthropic 顶层 system。
			//
			// ⚠ 合并成**一个**字符串块 vs 保留多块：官方抓包是**多块**
			//（每块可带 cache_control，用于提示缓存）。保留多块。
			if txt := contentToText(m["content"]); txt != "" {
				systemBlocks = append(systemBlocks, map[string]any{
					"type": "text",
					"text": txt,
				})
			}
		case "user":
			msgs = append(msgs, map[string]any{
				"role":    "user",
				"content": openAIContentToAnthropic(m["content"]),
			})
		case "assistant":
			blocks := openAIContentToAnthropic(m["content"])
			// 工具调用：OpenAI 的 `tool_calls[]` → Anthropic 的
			// content 里的 `tool_use` 块。
			if tcs, ok := m["tool_calls"].([]any); ok {
				for _, t := range tcs {
					tm, ok := t.(map[string]any)
					if !ok {
						continue
					}
					fn, _ := tm["function"].(map[string]any)
					if fn == nil {
						continue
					}
					name, _ := fn["name"].(string)
					argsStr, _ := fn["arguments"].(string)
					var args any = map[string]any{}
					if strings.TrimSpace(argsStr) != "" {
						// 解析失败时**原样塞成字符串**而不是丢弃 ——
						// 丢弃会让模型看到"调用了但没参数"，症状更怪。
						if err := json.Unmarshal([]byte(argsStr), &args); err != nil {
							args = map[string]any{"__raw": argsStr}
						}
					}
					blocks = append(blocks, map[string]any{
						"type":  "tool_use",
						"id":    strOr(tm["id"], "toolu_"+randID()),
						"name":  name,
						"input": args,
					})
				}
			}
			// ⚠ Anthropic 不接受 content 为空数组的消息 —— 但 assistant
			// 只带 tool_calls、没有文本时就会空。补一个空文本块兜住。
			if len(blocks) == 0 {
				blocks = append(blocks, map[string]any{"type": "text", "text": ""})
			}
			msgs = append(msgs, map[string]any{"role": "assistant", "content": blocks})
		case "tool":
			// OpenAI 的 tool 结果 → Anthropic 的 user + tool_result 块。
			//
			// ⚠ Anthropic **没有** `tool` 角色：工具结果要作为 **user**
			// 消息里的 `tool_result` 块回传。
			msgs = append(msgs, map[string]any{
				"role": "user",
				"content": []any{map[string]any{
					"type":        "tool_result",
					"tool_use_id": strOr(m["tool_call_id"], ""),
					"content":     contentToText(m["content"]),
				}},
			})
		default:
			// 未知角色：当成 user 处理，不丢消息。
			msgs = append(msgs, map[string]any{
				"role":    "user",
				"content": openAIContentToAnthropic(m["content"]),
			})
		}
	}
	if len(msgs) == 0 {
		return nil, fmt.Errorf("请求里没有可用的消息")
	}
	out["messages"] = msgs

	if len(systemBlocks) > 0 {
		out["system"] = systemBlocks
	}

	// ---- metadata：官方的会话元数据（抓包实测必发）----
	//
	// ⚠ `user_id` 的值本身是一个**序列化后的 JSON 字符串**（双重编码）——
	// 它是官方 SDK 的写法，照抄。上游据此把请求关联到"设备 / 会话"，
	// 而风控正是靠这类维度区分真实客户端与脚本。
	if meta.DeviceID != "" || meta.SessionID != "" {
		inner, _ := json.Marshal(map[string]any{
			"device_id":    meta.DeviceID,
			"account_uuid": meta.AccountUUID,
			"session_id":   meta.SessionID,
		})
		out["metadata"] = map[string]any{"user_id": string(inner)}
	}

	// ---- max_tokens：**必填** ----
	if v, ok := numOrZero(in["max_tokens"]); ok && v > 0 {
		out["max_tokens"] = v
	} else {
		out["max_tokens"] = DefaultMaxTokens
	}

	// ---- 采样参数（同名直通）----
	for _, k := range []string{"temperature", "top_p"} {
		if v, ok := in[k]; ok && v != nil {
			out[k] = v
		}
	}
	// ---- stop → stop_sequences（OpenAI 允许 string 或 []string）----
	switch s := in["stop"].(type) {
	case string:
		if s != "" {
			out["stop_sequences"] = []any{s}
		}
	case []any:
		if len(s) > 0 {
			out["stop_sequences"] = s
		}
	}
	// ---- stream 直通 ----
	if v, ok := in["stream"].(bool); ok {
		out["stream"] = v
	}

	// ---- tools ----
	//
	//	OpenAI     {type:"function", function:{name, description, parameters}}
	//	Anthropic  {name, description, input_schema}
	if tools, ok := in["tools"].([]any); ok && len(tools) > 0 {
		anthTools := make([]any, 0, len(tools))
		for _, t := range tools {
			tm, ok := t.(map[string]any)
			if !ok {
				continue
			}
			fn, _ := tm["function"].(map[string]any)
			if fn == nil {
				// 已经是 Anthropic 形状（客户端直接按 Anthropic 发的）→ 原样保留
				if _, has := tm["input_schema"]; has {
					anthTools = append(anthTools, tm)
				}
				continue
			}
			name, _ := fn["name"].(string)
			if name == "" {
				continue
			}
			at := map[string]any{"name": name}
			if d, ok := fn["description"].(string); ok && d != "" {
				at["description"] = d
			}
			// input_schema 是**必填**：缺了上游会 400。
			if p, ok := fn["parameters"]; ok && p != nil {
				at["input_schema"] = p
			} else {
				at["input_schema"] = map[string]any{"type": "object", "properties": map[string]any{}}
			}
			anthTools = append(anthTools, at)
		}
		if len(anthTools) > 0 {
			out["tools"] = anthTools
			out["tool_choice"] = anthropicToolChoice(in["tool_choice"])
		}
	}

	// ---- 思考（reasoning）----
	//
	// 官方抓包：
	//	"thinking": {"type":"enabled"},
	//	"output_config": {"effort":"low"}
	//
	// 且 `builtinModels` 里写明档位 → output_config.effort 的映射：
	//	low→"low"  high→"high"  max→"max"，defaultLevel="max"
	//
	// OpenAI 客户端用 `reasoning_effort` 表达档位，故这里做映射。
	if eff, ok := in["reasoning_effort"].(string); ok {
		if lvl := anthropicEffort(eff); lvl != "" {
			out["thinking"] = map[string]any{"type": "enabled"}
			out["output_config"] = map[string]any{"effort": lvl}
		}
	} else if rc, ok := in["reasoning"].(map[string]any); ok {
		// 有些客户端用 `reasoning: {effort:"…"}` 形态
		if eff, ok := rc["effort"].(string); ok {
			if lvl := anthropicEffort(eff); lvl != "" {
				out["thinking"] = map[string]any{"type": "enabled"}
				out["output_config"] = map[string]any{"effort": lvl}
			}
		}
	}

	return json.Marshal(out)
}

// anthropicEffort 把客户端档位名映射成 `output_config.effort` 的取值。
//
// 取值来自 `builtinModels[].reasoning.levels`（实测抓包）：
//
//	low → low / high → high / max → max；
//	另有 "medium" 与 "minimal" 两个常见别名（OpenAI 系的档位名），
//	分别就近映射到 low / high —— **就近**而不是拒绝，因为拒绝会让
//	"用户选了档位却完全没生效"，比"档位略偏"更糟。
func anthropicEffort(eff string) string {
	switch strings.ToLower(strings.TrimSpace(eff)) {
	case "low", "minimal", "none":
		return "low"
	case "medium", "mid", "default":
		// 上游只认 low/high/max；medium 取中间值 high
		return "high"
	case "high":
		return "high"
	case "max", "xhigh", "highest":
		return "max"
	default:
		return ""
	}
}

// anthropicToolChoice 把 OpenAI 的 tool_choice 翻成 Anthropic 形状。
//
//	OpenAI                        Anthropic
//	────────────────────────────  ──────────────────────────────────────
//	"auto" / 缺省                 {"type":"auto"}
//	"none"                        （**不发** —— 见下）
//	"required"                    {"type":"any"}
//	{"function":{"name":"f"}}     {"type":"tool","name":"f"}
//
// ⚠ OpenAI 的 `"none"` 在 Anthropic 里**没有等价物**。这里选择**不发
// tool_choice** 而不是发一个"禁用"的假值 —— Anthropic 只认 auto/any/tool，
// 发个不认识的值会 400；而"不发"时上游默认 auto，配合下面"不发 tools"
// 才是正确的禁用姿势（真正的禁用由调用方在 tools 层面处理）。
func anthropicToolChoice(v any) any {
	switch t := v.(type) {
	case string:
		switch strings.ToLower(strings.TrimSpace(t)) {
		case "auto", "":
			return map[string]any{"type": "auto"}
		case "required", "any":
			return map[string]any{"type": "any"}
		case "none":
			return nil // 交由调用方决定（见上）
		}
	case map[string]any:
		if fn, ok := t["function"].(map[string]any); ok {
			if name, ok := fn["name"].(string); ok && name != "" {
				return map[string]any{"type": "tool", "name": name}
			}
		}
		if ty, ok := t["type"].(string); ok {
			// 已经是 Anthropic 形状 → 原样
			if ty == "auto" || ty == "any" || ty == "tool" {
				return t
			}
		}
	}
	return map[string]any{"type": "auto"}
}

// openAIContentToAnthropic 把 OpenAI 的 content 转成 Anthropic 的块数组。
//
//	OpenAI 形态                         Anthropic 形态
//	──────────────────────────────────  ──────────────────────────────────
//	"纯字符串"                          [{type:"text",text:"…"}]
//	[{type:"text",text:"…"}]            [{type:"text",text:"…"}]
//	[{type:"image_url",…}]              [{type:"image",source:{…}}]
//
// ⚠ OpenAI 的 `image_url` 有两种来源（http(s) URL 或 data: URI），
// Anthropic 分别对应 `source.type="url"` 与 `source.type="base64"`。
// 弄错会让**带图请求整体 400**，故单独处理。
func openAIContentToAnthropic(content any) []any {
	switch c := content.(type) {
	case string:
		if c == "" {
			return []any{map[string]any{"type": "text", "text": ""}}
		}
		return []any{map[string]any{"type": "text", "text": c}}
	case []any:
		out := make([]any, 0, len(c))
		for _, part := range c {
			pm, ok := part.(map[string]any)
			if !ok {
				if s, ok := part.(string); ok {
					out = append(out, map[string]any{"type": "text", "text": s})
				}
				continue
			}
			ty, _ := pm["type"].(string)
			switch ty {
			case "text", "input_text":
				out = append(out, map[string]any{
					"type": "text",
					"text": strOr(pm["text"], ""),
				})
			case "image_url", "input_image":
				if blk := openAIImageToAnthropic(pm); blk != nil {
					out = append(out, blk)
				}
			case "image":
				// 已经是 Anthropic 形状
				out = append(out, pm)
			default:
				// 未知块：若有 text 字段就当文本，否则**保留原样**
				//（上游可能认识我们不认识的新块类型；丢掉比透传更糟）
				if s, ok := pm["text"].(string); ok {
					out = append(out, map[string]any{"type": "text", "text": s})
				} else {
					out = append(out, pm)
				}
			}
		}
		if len(out) == 0 {
			return []any{map[string]any{"type": "text", "text": ""}}
		}
		return out
	case map[string]any:
		// 单块对象（非数组）—— 少见但要兜住
		if s, ok := c["text"].(string); ok {
			return []any{map[string]any{"type": "text", "text": s}}
		}
		return []any{map[string]any{"type": "text", "text": ""}}
	case nil:
		return []any{map[string]any{"type": "text", "text": ""}}
	default:
		return []any{map[string]any{"type": "text", "text": fmt.Sprint(c)}}
	}
}

// openAIImageToAnthropic 把 OpenAI 的 image_url 块翻成 Anthropic 的 image 块。
func openAIImageToAnthropic(pm map[string]any) map[string]any {
	// OpenAI: {"type":"image_url","image_url":{"url":"…"}}
	url := ""
	if iu, ok := pm["image_url"].(map[string]any); ok {
		url, _ = iu["url"].(string)
	} else if s, ok := pm["image_url"].(string); ok {
		url = s
	}
	// 有些客户端直接给 base64 或 url 字段
	if url == "" {
		url = strOr(pm["url"], "")
	}
	if b64, ok := pm["data"].(string); ok && b64 != "" {
		return map[string]any{
			"type": "image",
			"source": map[string]any{
				"type":       "base64",
				"media_type": strOr(pm["media_type"], "image/png"),
				"data":       b64,
			},
		}
	}
	if url == "" {
		return nil
	}
	// data: URI → base64 source
	if strings.HasPrefix(url, "data:") {
		// data:image/png;base64,XXXX
		rest := strings.TrimPrefix(url, "data:")
		media, data, ok := strings.Cut(rest, ",")
		if !ok {
			return nil
		}
		media = strings.TrimSuffix(media, ";base64")
		if media == "" {
			media = "image/png"
		}
		return map[string]any{
			"type": "image",
			"source": map[string]any{
				"type":       "base64",
				"media_type": media,
				"data":       data,
			},
		}
	}
	// http(s) URL → url source
	return map[string]any{
		"type":   "image",
		"source": map[string]any{"type": "url", "url": url},
	}
}

// contentToText 把任意 content 压成纯文本（用于 system / tool 结果）。
func contentToText(content any) string {
	switch c := content.(type) {
	case string:
		return c
	case []any:
		var b strings.Builder
		for _, part := range c {
			switch p := part.(type) {
			case string:
				b.WriteString(p)
			case map[string]any:
				if s, ok := p["text"].(string); ok {
					b.WriteString(s)
				}
			}
		}
		return b.String()
	case map[string]any:
		if s, ok := c["text"].(string); ok {
			return s
		}
	}
	return ""
}

// ---------------------------------------------------------------------------
// 方向二：Anthropic 响应流 → OpenAI 响应流
// ---------------------------------------------------------------------------

// anthropicEvent Anthropic SSE 的单个事件。
type anthropicEvent struct {
	Type string          `json:"type"`
	Raw  json.RawMessage `json:"-"`
}

// TranslateStream 把 Anthropic 的 SSE 流翻成 OpenAI 的 SSE 流。
//
// 返回的 ReadCloser 会先读完上游、边读边翻，调用方负责 Close。
//
// # 为什么返回 io.ReadCloser 而不是直接写 http.ResponseWriter
//
// 与 qoder 的实现保持一致：翻译层只做"字节进、字节出"，
// 由调用方决定写到哪（HTTP 响应 / 测试缓冲）。这让**单测不必起 HTTP 服务**。
func TranslateStream(r io.Reader, model string) io.ReadCloser {
	pr, pw := io.Pipe()
	go func() {
		defer pw.Close()
		translateAnthropicStream(r, pw, model)
	}()
	return pr
}

// translateAnthropicStream 是 TranslateStream 的实际工作循环。
func translateAnthropicStream(r io.Reader, w io.Writer, model string) {
	sc := bufio.NewScanner(r)
	// SSE 单行可能很长（工具参数、长文本）—— 默认 64KB 会截断。
	// 放到 8MB：一个 input_json_delta 理论上是增量，但实测偶有整块下发。
	sc.Buffer(make([]byte, 0, 64*1024), 8*1024*1024)

	id := "chatcmpl-" + randID()
	created := nowUnix()
	roleSent := false
	// tool_use 块的 index → OpenAI 的 tool_calls 序号。
	// Anthropic 用 `index`（content block 序号），OpenAI 用 tool_calls 数组下标，
	// 两者**不是一回事**（中间可能有 text 块），故要单独编号。
	toolIdx := -1
	blockToTool := map[int]int{}
	// 当前文本块是否属于 thinking（reasoning）
	blockType := map[int]string{}

	writeChunk := func(delta map[string]any, finish any) {
		choice := map[string]any{"index": 0, "delta": delta}
		if finish != nil {
			choice["finish_reason"] = finish
		} else {
			choice["finish_reason"] = nil
		}
		obj := map[string]any{
			"id":      id,
			"object":  "chat.completion.chunk",
			"created": created,
			"model":   model,
			"choices": []any{choice},
		}
		b, err := json.Marshal(obj)
		if err != nil {
			return
		}
		fmt.Fprintf(w, "data: %s\n\n", b)
	}

	for sc.Scan() {
		line := strings.TrimRight(sc.Text(), "\r")
		// 只处理 data: 行 —— Anthropic 的 event: 行只是名字，类型在 JSON 里。
		if !strings.HasPrefix(line, "data:") {
			continue
		}
		payload := strings.TrimSpace(strings.TrimPrefix(line, "data:"))
		if payload == "" || payload == "[DONE]" {
			continue
		}
		var ev map[string]any
		if err := json.Unmarshal([]byte(payload), &ev); err != nil {
			// 单个事件解析失败**不中断整条流** —— 中断会让用户看到的回答
			// 比上游实际给的短，且没有任何提示。跳过并继续。
			continue
		}
		ty, _ := ev["type"].(string)

		switch ty {
		case "message_start":
			// 首个 chunk 带 role（OpenAI 约定）
			if !roleSent {
				writeChunk(map[string]any{"role": "assistant", "content": ""}, nil)
				roleSent = true
			}
		case "content_block_start":
			idx := intOf(ev["index"])
			cb, _ := ev["content_block"].(map[string]any)
			bt, _ := cb["type"].(string)
			blockType[idx] = bt
			switch bt {
			case "tool_use":
				toolIdx++
				blockToTool[idx] = toolIdx
				name, _ := cb["name"].(string)
				writeChunk(map[string]any{
					"tool_calls": []any{map[string]any{
						"index": toolIdx,
						"id":    strOr(cb["id"], "call_"+randID()),
						"type":  "function",
						"function": map[string]any{
							"name": name,
							// ⚠ 不能是空串起手：OpenAI 客户端（含我们的网关
							// Aggregate）会把空 arguments 当"无参数"。
							// 用 "{" 起手表示"JSON 还没结束"。
							"arguments": "",
						},
					}},
				}, nil)
			}
			// text / thinking 块在 start 时**不发** delta
			//（Anthropic 的 text 起手是空串，发了等于多发一个空 chunk）
		case "content_block_delta":
			idx := intOf(ev["index"])
			d, _ := ev["delta"].(map[string]any)
			dt, _ := d["type"].(string)
			switch dt {
			case "text_delta":
				if s, ok := d["text"].(string); ok && s != "" {
					writeChunk(map[string]any{"content": s}, nil)
				}
			case "thinking_delta":
				// Anthropic 的思考增量 → 网关内部的 reasoning_content
				//（网关的 upstream/sse.go 就认这个字段名）
				if s, ok := d["thinking"].(string); ok && s != "" {
					writeChunk(map[string]any{"reasoning_content": s}, nil)
				} else if s, ok := d["text"].(string); ok && s != "" {
					writeChunk(map[string]any{"reasoning_content": s}, nil)
				}
			case "input_json_delta":
				// 工具参数增量：OpenAI 用**字符串拼接**（不完整 JSON 是常态）
				ti, ok := blockToTool[idx]
				if !ok {
					continue
				}
				partial, _ := d["partial_json"].(string)
				if partial == "" {
					continue
				}
				writeChunk(map[string]any{
					"tool_calls": []any{map[string]any{
						"index":    ti,
						"function": map[string]any{"arguments": partial},
					}},
				}, nil)
			case "signature_delta":
				// 思考块签名，OpenAI 无对应物 —— 丢弃
			}
		case "content_block_stop":
			// 无需发 chunk
		case "message_delta":
			// 这里带 stop_reason 与 usage
			d, _ := ev["delta"].(map[string]any)
			sr, _ := d["stop_reason"].(string)
			finish := openAIFinishReason(sr)
			// usage 一并下发（网关会聚合它做统计）
			delta := map[string]any{}
			if u, ok := ev["usage"].(map[string]any); ok {
				u2 := map[string]any{}
				// Anthropic 的 input_tokens **不含**缓存命中，OpenAI 的
				// prompt_tokens 含 —— 加起来才是用户感知的"输入量"。
				in := intOf(u["input_tokens"])
				cr := intOf(u["cache_read_input_tokens"])
				cw := intOf(u["cache_creation_input_tokens"])
				u2["prompt_tokens"] = in + cr + cw
				u2["completion_tokens"] = intOf(u["output_tokens"])
				u2["total_tokens"] = in + cr + cw + intOf(u["output_tokens"])
				if cr > 0 {
					u2["prompt_tokens_details"] = map[string]any{"cached_tokens": cr}
				}
				delta["usage"] = u2
			}
			// finish_reason 与 usage 分两个 chunk 更稳（有些客户端只读第一个）
			if finish != "" {
				writeChunk(map[string]any{}, finish)
			}
			if u, ok := delta["usage"]; ok {
				obj := map[string]any{
					"id":      id,
					"object":  "chat.completion.chunk",
					"created": created,
					"model":   model,
					"choices": []any{},
					"usage":   u,
				}
				b, _ := json.Marshal(obj)
				fmt.Fprintf(w, "data: %s\n\n", b)
			}
		case "message_stop":
			fmt.Fprint(w, "data: [DONE]\n\n")
			return
		case "error":
			// 上游在中途报错：翻成 OpenAI 的 error 形状再结束，
			// 不能让客户端一直等（那样表现为"卡住"）。
			errObj, _ := ev["error"].(map[string]any)
			msg, _ := errObj["message"].(string)
			if msg == "" {
				msg = "上游返回错误"
			}
			b, _ := json.Marshal(map[string]any{
				"error": map[string]any{
					"message": msg,
					"type":    strOr(errObj["type"], "upstream_error"),
					"code":    errObj["code"],
				},
			})
			fmt.Fprintf(w, "data: %s\n\n", b)
			fmt.Fprint(w, "data: [DONE]\n\n")
			return
		case "ping":
			// 心跳：忽略（OpenAI 无对应物，透传会让客户端 parse 失败）
		}
	}
	// 上游流意外结束（没发 message_stop）：补一个 [DONE]，
	// 否则客户端会一直等 —— 表现为"回答到一半卡住"。
	fmt.Fprint(w, "data: [DONE]\n\n")
}

// openAIFinishReason Anthropic 的 stop_reason → OpenAI 的 finish_reason。
func openAIFinishReason(sr string) string {
	switch sr {
	case "end_turn", "stop_sequence":
		return "stop"
	case "max_tokens":
		return "length"
	case "tool_use":
		return "tool_calls"
	case "refusal":
		return "content_filter"
	case "pause_turn":
		// 服务端暂停（长任务）—— OpenAI 无等价物，当正常结束处理
		return "stop"
	default:
		return ""
	}
}

// ---------------------------------------------------------------------------
// 方向三：Anthropic 非流式响应 → OpenAI 非流式响应
// ---------------------------------------------------------------------------

// TranslateNonStream 把 Anthropic 的 message 对象翻成 OpenAI 的 chat.completion。
func TranslateNonStream(body []byte, model string) ([]byte, error) {
	var in map[string]any
	if err := json.Unmarshal(body, &in); err != nil {
		return nil, fmt.Errorf("解析 Anthropic 响应失败: %w", err)
	}
	// 已经是 OpenAI 形状就别动（幂等，便于重放/测试）
	if _, ok := in["choices"]; ok {
		return body, nil
	}

	msg := map[string]any{"role": "assistant"}
	var text strings.Builder
	var reasoning strings.Builder
	var toolCalls []any

	if blocks, ok := in["content"].([]any); ok {
		for _, b := range blocks {
			bm, ok := b.(map[string]any)
			if !ok {
				continue
			}
			switch strOr(bm["type"], "") {
			case "text":
				text.WriteString(strOr(bm["text"], ""))
			case "thinking":
				reasoning.WriteString(strOr(bm["thinking"], ""))
			case "tool_use":
				args, _ := json.Marshal(bm["input"])
				toolCalls = append(toolCalls, map[string]any{
					"id":   strOr(bm["id"], "call_"+randID()),
					"type": "function",
					"function": map[string]any{
						"name":      strOr(bm["name"], ""),
						"arguments": string(args),
					},
				})
			}
		}
	}
	msg["content"] = text.String()
	if reasoning.Len() > 0 {
		msg["reasoning_content"] = reasoning.String()
	}
	if len(toolCalls) > 0 {
		msg["tool_calls"] = toolCalls
	}

	finish := openAIFinishReason(strOr(in["stop_reason"], ""))
	if finish == "" {
		if len(toolCalls) > 0 {
			finish = "tool_calls"
		} else {
			finish = "stop"
		}
	}

	out := map[string]any{
		"id":      strOr(in["id"], "chatcmpl-"+randID()),
		"object":  "chat.completion",
		"created": nowUnix(),
		"model":   firstNonEmptyStr(model, strOr(in["model"], "")),
		"choices": []any{map[string]any{
			"index":         0,
			"message":       msg,
			"finish_reason": finish,
		}},
	}
	if u, ok := in["usage"].(map[string]any); ok {
		inTok := intOf(u["input_tokens"])
		cr := intOf(u["cache_read_input_tokens"])
		cw := intOf(u["cache_creation_input_tokens"])
		outTok := intOf(u["output_tokens"])
		out["usage"] = map[string]any{
			"prompt_tokens":     inTok + cr + cw,
			"completion_tokens": outTok,
			"total_tokens":      inTok + cr + cw + outTok,
		}
	}
	return json.Marshal(out)
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

func strOr(v any, def string) string {
	if s, ok := v.(string); ok && s != "" {
		return s
	}
	return def
}

// randID 生成一个短随机 id（用于 chatcmpl-/call_/toolu_ 前缀）。
//
// 复用 `NewDeviceMid` 的随机源思路但不带连字符 —— OpenAI 的 id 形态是
// `chatcmpl-<无连字符>`，Anthropic 的 tool id 是 `toolu_<…>`；
// 两者都**不校验格式**（与 X-Device-Mid 不同），故短 id 足够。
func randID() string {
	b := make([]byte, 12)
	if _, err := rand.Read(b); err != nil {
		// 随机源失败时回落到时间戳 —— 不能返回空串（空 id 会让
		// 客户端把多个响应当成同一个）。
		return fmt.Sprintf("%d", time.Now().UnixNano())
	}
	return hex.EncodeToString(b)
}

// nowUnix 当前 Unix 秒（OpenAI 的 `created` 字段）。
func nowUnix() int64 { return time.Now().Unix() }

func firstNonEmptyStr(a, b string) string {
	if strings.TrimSpace(a) != "" {
		return a
	}
	return b
}

// intOf 把 JSON 数字转 int（json 解出来是 float64）。
//
// ⚠ 必须是 `float64` 而不是 `int`：`encoding/json` 解到 `any` 时
// 数字**一律**是 float64。用 int 断言会**静默拿到 0** —— 那会让
// usage 全变 0、index 全变 0，且不报错。
func intOf(v any) int {
	switch n := v.(type) {
	case float64:
		return int(n)
	case int:
		return n
	case int64:
		return int(n)
	case json.Number:
		i, _ := n.Int64()
		return int(i)
	}
	return 0
}

// numOrZero 判断 JSON 数字是否有效。
func numOrZero(v any) (float64, bool) {
	switch n := v.(type) {
	case float64:
		return n, true
	case int:
		return float64(n), true
	case json.Number:
		f, err := n.Float64()
		return f, err == nil
	}
	return 0, false
}

// IsAnthropicBody 判断响应体是否是 Anthropic 形状（用于错误分支的识别）。
func IsAnthropicBody(body []byte) bool {
	s := string(bytes.TrimSpace(body))
	return strings.Contains(s, `"type":"error"`) ||
		strings.Contains(s, `"type": "error"`)
}
