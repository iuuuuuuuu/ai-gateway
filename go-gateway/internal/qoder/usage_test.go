package qoder

// usage 上报的回归测试。
//
// # 缺陷原貌（2026-09-19 从上游 PR 调研中核实）
//
// 我们的 Qoder 路径**对客户端从不报 token 用量**：
//
//	sse.go:145  注释写着「其它：可能是 usage 汇总等 —— 不算解析失败，
//	            返回空分片」→ **usage 被丢掉了**
//	stream.go   frame() 里根本没有 usage 字段
//	AggregateQoder  产出的响应也没有 usage
//
// 后果：本地统计与成本台账拿不到 Qoder 的数，用户在客户端里也看不到用量。
//
// # 两个必须同时满足的约束
//
//	① usage 要**收下**（上游常塞在带 choices 的 finish 分片里）
//	② 要**拆成独立帧**发出去（严格客户端只从 choices 为空的帧读 usage）
//
// 只做 ① 不做 ② 等于没修 —— 客户端读不到。这条测试盯的就是这个。

import (
	"encoding/json"
	"io"
	"strings"
	"testing"
)

// TestUsageFromStandaloneChunk 独立 usage 分片（choices 为空）要被收下。
func TestUsageFromStandaloneChunk(t *testing.T) {
	raw := `{"id":"x","choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120}}`
	ch, err := parseOpenAIShaped([]byte(raw), raw)
	if err != nil {
		t.Fatalf("解析失败: %v", err)
	}
	if len(ch.Usage) == 0 {
		t.Fatal("独立 usage 分片必须被收下（此前被静默丢弃）")
	}
	if got, _ := numOfAny(ch.Usage["prompt_tokens"]); got != 100 {
		t.Errorf("prompt_tokens 应为 100，实际 %v", ch.Usage["prompt_tokens"])
	}
}

// TestUsageFromFinishChunk 带 choices 的 finish 分片里的 usage 也要收下。
//
// 这是上游**实际**的用法位置（与标准 OpenAI 不同，故容易漏）。
func TestUsageFromFinishChunk(t *testing.T) {
	raw := `{"id":"x","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],` +
		`"usage":{"prompt_tokens":50,"completion_tokens":10,"total_tokens":60}}`
	ch, err := parseOpenAIShaped([]byte(raw), raw)
	if err != nil {
		t.Fatalf("解析失败: %v", err)
	}
	if ch.FinishReason != "stop" {
		t.Errorf("finish_reason 应保留，实际 %q", ch.FinishReason)
	}
	if len(ch.Usage) == 0 {
		t.Fatal("带 choices 的 finish 分片里的 usage 也必须收下（上游就是放这里的）")
	}
}

// TestUsageSplitIntoOwnFrame usage 必须**拆成独立帧**，且内容帧里不带它。
//
// 严格遵循 OpenAI 规范的客户端只从 `choices` 为空的帧读 usage；
// 塞在带 choices 的帧里它读不到 —— 表现为"用量面板恒为 0"。
//
// ⚠ 这里走**真实入口**（`NewOpenAIStream` + 读流），而不是直接调内部
// `consume` —— 内部函数签名随时可能变，而"从流里读出来的帧长什么样"
// 才是客户端真正看到的东西。
func TestUsageSplitIntoOwnFrame(t *testing.T) {
	// 上游形状：usage 塞在**带 choices 的 finish 分片**里（Qoder 的实际做法）
	upstream := "data: {\"id\":\"x\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":50,\"completion_tokens\":10,\"total_tokens\":60}}\n\n" +
		"data: [DONE]\n\n"

	rc := NewOpenAIStream(io.NopCloser(strings.NewReader(upstream)), "m")
	defer rc.Close()
	out, err := io.ReadAll(rc)
	if err != nil {
		t.Fatalf("读流失败: %v", err)
	}
	text := string(out)

	if !strings.Contains(text, "prompt_tokens") {
		t.Fatalf("输出里必须带上 usage（此前被完全丢弃）：%q", text)
	}

	var contentFrame, usageFrame map[string]any
	for _, line := range strings.Split(text, "\n") {
		line = strings.TrimSpace(line)
		if !strings.HasPrefix(line, "data: ") || strings.Contains(line, "[DONE]") {
			continue
		}
		var m map[string]any
		if json.Unmarshal([]byte(strings.TrimPrefix(line, "data: ")), &m) != nil {
			continue
		}
		if choices, ok := m["choices"].([]any); ok && len(choices) > 0 {
			contentFrame = m
		} else if _, ok := m["usage"]; ok {
			usageFrame = m
		}
	}
	if contentFrame == nil {
		t.Fatalf("没找到内容帧：%q", text)
	}
	if usageFrame == nil {
		t.Fatalf("没找到独立 usage 帧（严格客户端只从这里读用量）：%q", text)
	}
	if _, has := contentFrame["usage"]; has {
		t.Error("内容帧里不该带 usage（超出规范，且严格客户端读不到）")
	}
	if choices, ok := usageFrame["choices"].([]any); !ok || len(choices) != 0 {
		t.Errorf("usage 帧必须显式带 choices 空数组（部分客户端靠它判定），实际 %v", usageFrame["choices"])
	}
}

// TestCacheAliasesEnriched 缓存用量要**两种形状都给**。
//
//	标准 OpenAI：prompt_tokens_details.cached_tokens
//	CodeBuddy  ：prompt_cache_hit_tokens（顶层）
//
// 只给标准形状时，读顶层别名的客户端把命中显示成 0，
// 用户以为缓存没生效（实际生效了），去查一个不存在的问题。
func TestCacheAliasesEnriched(t *testing.T) {
	usage := map[string]any{
		"prompt_tokens":         float64(1000),
		"prompt_tokens_details": map[string]any{"cached_tokens": float64(800)},
	}
	enrichCacheAliases(usage)

	if got, _ := numOfAny(usage["prompt_cache_hit_tokens"]); got != 800 {
		t.Errorf("prompt_cache_hit_tokens 应为 800（顶层别名），实际 %v", usage["prompt_cache_hit_tokens"])
	}
	if got, _ := numOfAny(usage["prompt_cache_miss_tokens"]); got != 200 {
		t.Errorf("prompt_cache_miss_tokens 应为 1000-800=200，实际 %v", usage["prompt_cache_miss_tokens"])
	}
}

// TestCacheAliasesDoNotOverwriteNative 原生字段优先，绝不覆盖。
func TestCacheAliasesDoNotOverwriteNative(t *testing.T) {
	usage := map[string]any{
		"prompt_tokens":             float64(1000),
		"prompt_cache_hit_tokens":   float64(777), // 上游自己给的
		"prompt_tokens_details":     map[string]any{"cached_tokens": float64(800)},
	}
	enrichCacheAliases(usage)
	if got, _ := numOfAny(usage["prompt_cache_hit_tokens"]); got != 777 {
		t.Errorf("上游已给的顶层别名不能被覆盖（应保持 777），实际 %v", got)
	}
}

// TestNumOfAnyHandlesFloat64 JSON 数字是 float64，断言 int64 会失败。
//
// 这条防的是"补别名那段静默不生效"——`v.(int64)` 对 float64 返回 false，
// 而 false 被当成"取不到"就悄悄跳过了。
func TestNumOfAnyHandlesFloat64(t *testing.T) {
	if got, ok := numOfAny(float64(42)); !ok || got != 42 {
		t.Errorf("float64(42) 应解析成 42，实际 %d ok=%v", got, ok)
	}
	if _, ok := numOfAny("42"); ok {
		t.Error("字符串不该被当成数字")
	}
	if _, ok := numOfAny(nil); ok {
		t.Error("nil 不该被当成数字")
	}
}
