package qoder

// translate.go OpenAI ↔ Qoder 的请求/响应翻译。
//
// 这一层存在的理由：网关对外是 **OpenAI 协议**（客户端按它发请求），
// 而 Qoder 上游是**另一套协议**：
//
//	方向         OpenAI 形状                        Qoder 形状
//	───────────  ─────────────────────────────────  ──────────────────────────────
//	请求          {model, messages, tools, stream}   {request_id, agent_id, chat_task,
//	                                                  chat_context, messages, model_config}
//	请求体编码    无                                 QoderEncode（base64+重排+映射）
//	响应流        data: {"choices":[...]}            data: {"body":"{...choices...}"}
//
// 翻译错了不会报错，只会让用户看到空回答或上游 400 —— 故两个方向都有单测。

import (
	"encoding/json"
	"fmt"
	"strings"
	"time"
)

// agentBody 构造 agent_chat_generation 的请求体。
//
// ## 为什么"纯透传"而不是构造模板
//
// 参考实现实测：模板 system + 模板 tools **均非必需**，纯透传客户端消息后
// baseline prompt_tokens 从约 10K 降到约 60。10K 的固定开销会吃掉用户
// 相当一部分额度，且对简单问答毫无价值。
//
// 因此：客户端消息**全量转发**（含 system/assistant/tool 多轮），
// tools 仅在客户端显式传入时注入。
//
// ## chat_context.text 为什么必填
//
// 上游协议要求它存在（哪怕 messages 里已有完整历史）。取最后一条 user
// 消息的文本作为它的值 —— 与参考实现口径一致。
func agentBody(openAIMessages []map[string]any, modelKey string) ([]byte, error) {
	prompt := lastUserText(openAIMessages)

	now := time.Now()
	newID := NewUUID()

	base := map[string]any{
		"request_id":       newID,
		"chat_record_id":   newID,
		"request_set_id":   NewUUID(),
		"session_id":       NewUUID(),
		"stream":           true,
		"aliyun_user_type": "personal_professional_trial",
		"agent_id":         "agent_common",
		"chat_task":        "FREE_INPUT",
		"is_reply":         true,
		"image_urls":       nil,
		"session_type":     "qodercli",
		"model_config":     map[string]any{"key": modelKey, "is_reasoning": false},
		"chat_context": map[string]any{
			"chatPrompt": "",
			"text":       map[string]any{"type": "text", "text": prompt},
			"extra": map[string]any{
				"context":         []any{},
				"modelConfig":     map[string]any{"key": modelKey, "is_reasoning": false},
				"originalContent": map[string]any{"type": "text", "text": prompt},
			},
			"features":  []any{},
			"imageUrls": nil,
		},
		"messages": openAIMessages,
		"business": map[string]any{
			"id":       NewUUID(),
			"begin_at": now.UnixMilli(),
			"name":     truncateRunes(prompt, 30),
		},
	}

	return json.Marshal(base)
}

// BuildAgentBody 从 OpenAI 请求体构造 Qoder 请求体。
//
// modelKey 是**上游模型 key**（不是客户端看到的模型名）—— 调用方负责映射。
func BuildAgentBody(openAIBody []byte, modelKey string) ([]byte, error) {
	var req struct {
		Messages []map[string]any `json:"messages"`
		Tools    []any            `json:"tools"`
	}
	if err := json.Unmarshal(openAIBody, &req); err != nil {
		return nil, fmt.Errorf("解析 OpenAI 请求体失败: %w", err)
	}
	if len(req.Messages) == 0 {
		return nil, fmt.Errorf("请求里没有 messages")
	}

	raw, err := agentBody(req.Messages, modelKey)
	if err != nil {
		return nil, err
	}

	// tools 仅在客户端显式传入时注入（参考实现的口径）。
	if len(req.Tools) > 0 {
		var m map[string]any
		if err := json.Unmarshal(raw, &m); err != nil {
			return nil, err
		}
		m["tools"] = req.Tools
		return json.Marshal(m)
	}
	return raw, nil
}

// lastUserText 取最后一条 user 消息的文本。
//
// content 可能是字符串，也可能是多模态数组（[{type:text,text:...},...]）。
// 两种都要处理：只认字符串会让带图片的请求拿到空 prompt，
// 而 chat_context.text 为空时上游可能直接拒绝。
func lastUserText(messages []map[string]any) string {
	for i := len(messages) - 1; i >= 0; i-- {
		if messages[i]["role"] != "user" {
			continue
		}
		switch c := messages[i]["content"].(type) {
		case string:
			if c != "" {
				return c
			}
		case []any:
			// 多模态：拼接所有 text 段（忽略 image_url）
			var sb strings.Builder
			for _, part := range c {
				pm, ok := part.(map[string]any)
				if !ok {
					continue
				}
				if t, ok := pm["text"].(string); ok && t != "" {
					sb.WriteString(t)
				}
			}
			if sb.Len() > 0 {
				return sb.String()
			}
		}
	}
	return ""
}

// truncateRunes 按 rune 截断（不能按字节：会把汉字劈成两半）。
func truncateRunes(s string, n int) string {
	r := []rune(s)
	if len(r) <= n {
		return s
	}
	return string(r[:n])
}
