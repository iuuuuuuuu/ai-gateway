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
	"crypto/sha256"
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

	// ⚠ `source` 是**上游是否回传 reasoning_content 的开关**（关键字段）。
	//
	// 来自上游 qoderwork2api 的 PR #3（`internal/upstream/body.go`）逆向穷举：
	//
	//	{key, source:"system"}                → 返回 reasoning  ✅
	//	{key, is_reasoning:true}              → 不返回          ❌
	//	全字段但缺 source                      → 不返回          ❌
	//	{key, is_reasoning, source:"custom"}  → 不返回          ❌
	//
	// **值必须严格等于 `"system"`**，`is_reasoning` 完全不影响。
	//
	// 我们此前发的正是「缺 source」那一格 —— 而 `stream.go:125` 又在
	// 解析并回传 `reasoning_content`，等于下游**永远白等**：
	// 思考块恒为空，用户以为模型不思考。
	// 这是"两头都写对了、中间少一个字段"的典型。
	modelConfig := map[string]any{
		"key":          modelKey,
		"is_reasoning": false,
		// 常量而非变量：上游只认这一个值，做成可配置反而容易被误改
		"source": modelSourceSystem,
	}

	base := map[string]any{
		"request_id":       newID,
		"chat_record_id":   newID,
		"request_set_id":   NewUUID(),
		// session_id 由 deriveSessionID 派生（不是每次新 UUID），见其说明
		"session_id":       deriveSessionID(modelKey, prompt),
		"stream":           true,
		"aliyun_user_type": "personal_professional_trial",
		"agent_id":         "agent_common",
		"chat_task":        "FREE_INPUT",
		"is_reply":         true,
		"image_urls":       nil,
		"session_type":     "qodercli",
		"model_config":     modelConfig,
		"chat_context": map[string]any{
			"chatPrompt": "",
			"text":       map[string]any{"type": "text", "text": prompt},
			"extra": map[string]any{
				"context":         []any{},
				"modelConfig":     modelConfig,
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

// modelSourceSystem 是 `model_config.source` 唯一被上游接受的值。
//
// 单独提成常量并写清理由，是因为它看起来"只是个元数据字段"、
// 很容易被当成无用字段删掉 —— 而删掉它，思考内容就静默没了。
const modelSourceSystem = "system"

// deriveSessionID 由「模型 + 首条用户文本」**确定性派生** session_id。
//
// # 为什么不能每次 new 一个 UUID（我们此前的做法）
//
// 上游按 `session_id` 做**会话级 prompt cache**。若每轮请求都是新 session，
// 缓存**在结构上就不可能命中** —— 每一轮都把完整上下文当新前缀重新计费。
// 对长对话（agentic IDE 动辄几万 token 上下文）这是实打实的额度浪费。
//
// # ⚠ 绝不能把 system 纳入哈希（上游踩过的坑，原话）
//
//	「CodeBuddy 等 agentic IDE 每轮向 system prompt 注入动态内容
//	  （时间 / git 快照 / linter / 光标），system 逐轮变
//	  → session 逐轮变 → 上游缓存永远读不到上一轮前缀（**命中恒 0**）」
//
// 故只用「模型 + 首条 user 文本」：前者标识模型命名空间，后者在同一对话内
// 天然稳定（用户不会改第一句话），且跨对话不同。
//
// ⚠ 这不引入安全风险：session_id 是我们主动告知上游的会话分组标识，
// 不含秘密、也不需要不可预测（它不被用作凭据）。
func deriveSessionID(modelKey, firstUserText string) string {
	h := sha256.New()
	h.Write([]byte(modelKey))
	h.Write([]byte{0}) // 分隔符：避免 ("ab","c") 与 ("a","bc") 撞成同一个
	h.Write([]byte(firstUserText))
	sum := h.Sum(nil)
	// 取前 16 字节拼成 UUID 形态 —— 上游对非 UUID 形态会拒
	return fmt.Sprintf("%x-%x-%x-%x-%x", sum[0:4], sum[4:6], sum[6:8], sum[8:10], sum[10:16])
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
