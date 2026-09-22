package qoder

// developer_role_test.go —— 钉住「`developer` 角色必须归一化成 `system`」这个修复。
//
// # 所有者现场（2026-09-22）
//
//	「关闭 developer 角色,关闭后就能用了」
//
// 修复前的现象是**对话根本发不出去**，每次都报：
//
//	503 {"code":"no_healthy_account",
//	     "message":"上游服务异常（HTTP 503），已切换到其他账号"}
//
// 而同一网关、同一时刻，`zcode:` 与直接 `curl` 都是好的 ——
// 所有者因此反复说「zcode 没问题、qoder 不行」。
//
// # 根因
//
// 客户端对 `openai-completions` 会用 **`developer` 替代 `system`**
//（pi-ai 的 `supportsDeveloperRole`）。而 **qoder 上游只认 `system`**。
//
// `agentBody` 此前 `"messages": openAIMessages` **原样透传** ⇒ 上游 503。
//
// 另外两条路径都早已规范化（`upstream/payload.go:225` 的 WorkBuddy、
// `zcode/translate.go` 的 ZCode），**qoder 是唯一漏掉的那条**。
//
// # ⚠ 为什么这些测试必须用「完整客户端形态」
//
// 复现的关键是**组合**：
//
//	developer 单独发（无 tools、非流式） → 上游容忍，200
//	developer + tools + 流式             → **必然 503**
//
// 只测裸 `developer` 会误判成"没问题" —— 我第一遍就这么测错了，
// 白绕了很久。回归测试必须带上 tools 与流式这些真实字段。

import (
	"encoding/json"
	"strings"
	"testing"
)

// buildDeveloperBody 构造一个「带 `developer` 角色 + tools」的完整客户端请求体，
// 并返回 qoder 侧最终会发给上游的 messages 数组。
func buildDeveloperBody(t *testing.T) []map[string]any {
	t.Helper()
	// 与客户端真实形态一致：developer 系统消息 + 上下文注入 + tools + 流式
	raw := `{
		"model": "qoder:Qwen3.8-Flash",
		"messages": [
			{"role": "developer", "content": "You are an agent working in the workspace."},
			{"role": "user", "content": "上下文注入 · dsh-tauri-ui"},
			{"role": "user", "content": "1+1"}
		],
		"tools": [{"type":"function","function":{"name":"read_file","description":"Read a file",
			"parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}}],
		"stream": true,
		"stream_options": {"include_usage": true},
		"max_completion_tokens": 8000,
		"store": false
	}`

	body, err := BuildAgentBody([]byte(raw), "Qwen3.8-Flash")
	if err != nil {
		t.Fatalf("BuildAgentBody 失败: %v", err)
	}

	var out struct {
		Messages []map[string]any `json:"messages"`
	}
	if err := json.Unmarshal(body, &out); err != nil {
		t.Fatalf("解析发给上游的请求体失败: %v\n%s", err, body)
	}
	return out.Messages
}

// TestDeveloperRoleNormalizedToSystem ★ `developer` 不得原样发给 qoder 上游。
//
// 这是本修复的核心断言，也是所有者「对话根本发不出去」的直接原因。
func TestDeveloperRoleNormalizedToSystem(t *testing.T) {
	msgs := buildDeveloperBody(t)

	if len(msgs) == 0 {
		t.Fatal("发给上游的 messages 不能为空")
	}

	// 不得有任何 developer 角色残留
	for i, m := range msgs {
		if role, _ := m["role"].(string); strings.EqualFold(strings.TrimSpace(role), "developer") {
			t.Fatalf(
				"messages[%d] 的 role 仍是 `developer` —— qoder 上游只认 `system`，\n"+
					"原样透传会让**对话根本发不出去**，报：\n"+
					"  503 no_healthy_account「上游服务异常（HTTP 503）」\n"+
					"（另外两条路径 upstream/payload.go 与 zcode/translate.go 都已归一化，\n"+
					"  qoder 是唯一漏掉的）\n完整 messages: %v", i, msgs)
		}
	}

	// 且第一条（原本的 developer）必须变成 system —— 不能是"删掉了"
	first, _ := msgs[0]["role"].(string)
	if first != "system" {
		t.Errorf("原本的 developer 消息应改写为 system，实际 %q\n完整 messages: %v", first, msgs)
	}
}

// TestDeveloperNormalizationKeepsOtherRoles 只动 developer，别的不许碰。
//
// 回归保护：归一化很容易写成"把 role 全改成 system"，
// 那会把 tool / assistant 多轮消息一起毁掉（qoder 是全量转发历史消息的）。
func TestDeveloperNormalizationKeepsOtherRoles(t *testing.T) {
	raw := `{"messages":[
		{"role":"developer","content":"sys"},
		{"role":"user","content":"问题"},
		{"role":"assistant","content":"回答"},
		{"role":"tool","content":"结果"}
	]}`

	body, err := BuildAgentBody([]byte(raw), "Qwen3.8-Flash")
	if err != nil {
		t.Fatalf("BuildAgentBody 失败: %v", err)
	}
	var out struct {
		Messages []map[string]any `json:"messages"`
	}
	if err := json.Unmarshal(body, &out); err != nil {
		t.Fatalf("解析失败: %v", err)
	}

	want := []string{"system", "user", "assistant", "tool"}
	if len(out.Messages) != len(want) {
		t.Fatalf("消息条数应保持 %d，实际 %d", len(want), len(out.Messages))
	}
	for i, w := range want {
		got, _ := out.Messages[i]["role"].(string)
		if got != w {
			t.Errorf("messages[%d].role = %q，应为 %q（归一化只能改 developer）", i, got, w)
		}
	}
}

// TestSystemRoleUnaffected 本来就正确的 `system` 不受影响。
//
// 这是"修复不能改变既有正确行为"的护栏：绝大多数请求发的就是 system。
func TestSystemRoleUnaffected(t *testing.T) {
	raw := `{"messages":[
		{"role":"system","content":"You are helpful."},
		{"role":"user","content":"hi"}
	]}`
	body, err := BuildAgentBody([]byte(raw), "Qwen3.8-Flash")
	if err != nil {
		t.Fatalf("BuildAgentBody 失败: %v", err)
	}
	var out struct {
		Messages []map[string]any `json:"messages"`
	}
	if err := json.Unmarshal(body, &out); err != nil {
		t.Fatalf("解析失败: %v", err)
	}
	if got, _ := out.Messages[0]["role"].(string); got != "system" {
		t.Errorf("system 角色不应被改动，实际 %q", got)
	}
}

// TestDeveloperNormalizationIsCaseInsensitive 大小写与空白要容错。
//
// 上游客户端不保证写成小写；`Developer` / ` developer ` 都该被处理。
func TestDeveloperNormalizationIsCaseInsensitive(t *testing.T) {
	for _, role := range []string{"Developer", "DEVELOPER", " developer "} {
		raw := `{"messages":[{"role":"` + role + `","content":"sys"},{"role":"user","content":"hi"}]}`
		body, err := BuildAgentBody([]byte(raw), "Qwen3.8-Flash")
		if err != nil {
			t.Fatalf("BuildAgentBody 失败: %v", err)
		}
		var out struct {
			Messages []map[string]any `json:"messages"`
		}
		if err := json.Unmarshal(body, &out); err != nil {
			t.Fatalf("解析失败: %v", err)
		}
		if got, _ := out.Messages[0]["role"].(string); got != "system" {
			t.Errorf("role=%q 应归一化成 system，实际 %q", role, got)
		}
	}
}

// TestGeneratedBodyIsValidJSON 归一化后整个请求体仍是合法 JSON。
//
// 这条护栏针对"就地改 map 之后重新序列化出错"这类低级但致命的失误 ——
// body 不合法的话上游会报 `11101`，那会被误判成完全无关的问题。
func TestGeneratedBodyIsValidJSON(t *testing.T) {
	raw := `{"model":"qoder:Qwen3.8-Flash","messages":[
		{"role":"developer","content":"sys"},{"role":"user","content":"hi"}],
		"tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object"}}}],
		"stream":true}`
	body, err := BuildAgentBody([]byte(raw), "Qwen3.8-Flash")
	if err != nil {
		t.Fatalf("BuildAgentBody 失败: %v", err)
	}
	var v map[string]any
	if err := json.Unmarshal(body, &v); err != nil {
		t.Fatalf("发给上游的请求体不是合法 JSON: %v\n%s", err, body)
	}
	// tools 必须仍在（BuildAgentBody 的另一条职责）
	if _, ok := v["tools"]; !ok {
		t.Errorf("tools 应被保留：\n%s", body)
	}
}
