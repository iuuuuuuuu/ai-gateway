package prompt

// `append` 模式的单测。
//
// # 为什么需要第三种模式（`custom` 的已知副作用）
//
// `Rewrite`（custom）把客户端的 system **整体替换**掉，代价是客户端的
// **项目规范**（代码风格、工具约定、"不要改 X 文件"这类约束）一并消失。
// 用户看到的是"模型变笨了/不听话了"，而原因在网关把它的规则删了。
//
// `append` 让两者共存。本文件钉住它的三条不变式：
//
//	① 客户端原有 system **逐字保留**（不能被改写或丢弃）
//	② 网关提示词插在**开头连续的** system 之后（不是末尾、不是中间）
//	③ 除 messages 外的字段**字节透传**（与 Rewrite 同一纪律）

import (
	"encoding/json"
	"strings"
	"testing"
)

// TestAppendKeepsClientSystem 客户端的 system 必须逐字保留。
//
// 这是 append 相对 custom 的**全部意义** —— 丢了它，append 就退化成了
// 一个更绕的 custom。
func TestAppendKeepsClientSystem(t *testing.T) {
	body := []byte(`{"model":"m","messages":[` +
		`{"role":"system","content":"PROJECT RULES: 必须用 tabs 缩进，不要改 config.go"},` +
		`{"role":"user","content":"改一下登录逻辑"}]}`)

	out := Append(body, "你是网关注入的提示词")
	msgs := messagesOf(t, out)

	if len(msgs) != 3 {
		t.Fatalf("应为 3 条消息（客户端 system + 网关 system + user），实际 %d：%s", len(msgs), out)
	}
	// ① 客户端 system 原样在**第一位**
	if got := roleOf(t, msgs[0]); got != "system" {
		t.Errorf("第 1 条应是客户端的 system，实际 role=%s", got)
	}
	if got := contentOf(t, msgs[0]); !strings.Contains(got, "必须用 tabs 缩进") {
		t.Errorf("客户端的项目规范必须逐字保留（append 的全部意义），实际 %q", got)
	}
	// ② 网关提示词紧随其后
	if got := roleOf(t, msgs[1]); got != "system" {
		t.Errorf("第 2 条应是网关注入的 system，实际 role=%s", got)
	}
	if got := contentOf(t, msgs[1]); got != "你是网关注入的提示词" {
		t.Errorf("第 2 条应是网关提示词，实际 %q", got)
	}
	// ③ user 消息未被改动
	if got := contentOf(t, msgs[2]); got != "改一下登录逻辑" {
		t.Errorf("user 消息不该被改动，实际 %q", got)
	}
}

// TestAppendInsertsAfterLeadingSystemsOnly 只跳过**开头连续的** system。
//
// # 为什么不能"跳到最后一个 system 之后"
//
// 有些 agentic 客户端会在**对话中途**注入 system（例如工具调用后补一条
// 约束）。若把网关提示词插到最后一条 system 之后，等于在"用户说到一半时
// 改了系统规则" —— 语义上很怪，且会破坏客户端原有的前缀缓存布局。
func TestAppendInsertsAfterLeadingSystemsOnly(t *testing.T) {
	body := []byte(`{"model":"m","messages":[` +
		`{"role":"system","content":"规则 A"},` +
		`{"role":"system","content":"规则 B"},` +
		`{"role":"user","content":"问题"},` +
		`{"role":"system","content":"中途注入的规则"},` +
		`{"role":"assistant","content":"回答"}]}`)

	out := Append(body, "网关提示词")
	msgs := messagesOf(t, out)

	if len(msgs) != 6 {
		t.Fatalf("应为 6 条，实际 %d：%s", len(msgs), out)
	}
	// 第 3 位（索引 2）应是网关提示词 —— 即「开头两条 system 之后」
	if got := contentOf(t, msgs[2]); got != "网关提示词" {
		t.Errorf("网关提示词应插在开头连续的 system 之后（索引 2），实际索引 2 是 %q", got)
	}
	// 中途那条 system 必须原样留在原位（索引 4）
	if got := contentOf(t, msgs[4]); got != "中途注入的规则" {
		t.Errorf("对话中途的 system 必须保持原位，实际索引 4 是 %q", got)
	}
}

// TestAppendWithNoSystemPutsGatewayFirst 没有 system 时，网关提示词在最前。
func TestAppendWithNoSystemPutsGatewayFirst(t *testing.T) {
	body := []byte(`{"model":"m","messages":[{"role":"user","content":"你好"}]}`)
	out := Append(body, "网关提示词")
	msgs := messagesOf(t, out)

	if len(msgs) != 2 {
		t.Fatalf("应为 2 条，实际 %d", len(msgs))
	}
	if got := contentOf(t, msgs[0]); got != "网关提示词" {
		t.Errorf("没有客户端 system 时网关提示词应在最前，实际 %q", got)
	}
}

// TestAppendAllSystemMessagesAppendsAtEnd 全是 system 时追加到末尾。
//
// 边界情况：没有"非 system"可以插在它前面，故只能追加到末尾。
// 不处理会让网关提示词**静默不插入**（配置看起来生效了、实际没有）。
func TestAppendAllSystemMessagesAppendsAtEnd(t *testing.T) {
	body := []byte(`{"model":"m","messages":[{"role":"system","content":"只有规则"}]}`)
	out := Append(body, "网关提示词")
	msgs := messagesOf(t, out)

	if len(msgs) != 2 {
		t.Fatalf("应为 2 条，实际 %d：%s", len(msgs), out)
	}
	if got := contentOf(t, msgs[1]); got != "网关提示词" {
		t.Errorf("全是 system 时应追加到末尾，实际末条是 %q", got)
	}
}

// TestAppendDeveloperRoleCountsAsSystem developer 也算 system 类。
//
// 与 `isSystemLike` 同一口径：客户端拼写变体不该让插入位置漂移。
func TestAppendDeveloperRoleCountsAsSystem(t *testing.T) {
	body := []byte(`{"model":"m","messages":[` +
		`{"role":"developer","content":"开发规范"},` +
		`{"role":"user","content":"hi"}]}`)
	out := Append(body, "网关提示词")
	msgs := messagesOf(t, out)

	if got := contentOf(t, msgs[1]); got != "网关提示词" {
		t.Errorf("developer 应被当作 system 类（提示词插在其后），实际索引 1 是 %q", got)
	}
}

// TestAppendPreservesOtherFields 除 messages 外字段**字节透传**。
//
// 与 Rewrite 同一纪律：用 `map[string]json.RawMessage` 而非
// `map[string]any`，否则数字会被转成 float64 丢精度、字段顺序被打乱。
func TestAppendPreservesOtherFields(t *testing.T) {
	// 用一个大整数，float64 会丢精度（9007199254740993 → ...992）
	body := []byte(`{"model":"m","max_tokens":9007199254740993,"temperature":0.7,` +
		`"messages":[{"role":"user","content":"hi"}]}`)
	out := Append(body, "网关提示词")

	if !strings.Contains(string(out), "9007199254740993") {
		t.Errorf("大整数必须逐字保留（float64 会丢精度），实际：%s", out)
	}
	if !strings.Contains(string(out), `"temperature":0.7`) {
		t.Errorf("其它字段应原样保留，实际：%s", out)
	}
}

// TestAppendEmptyPromptIsNoop 空提示词 = 不改写。
func TestAppendEmptyPromptIsNoop(t *testing.T) {
	body := []byte(`{"model":"m","messages":[{"role":"user","content":"hi"}]}`)
	for _, p := range []string{"", "   ", "\n\t"} {
		out := Append(body, p)
		if string(out) != string(body) {
			t.Errorf("空提示词 %q 应原样返回，实际改成了：%s", p, out)
		}
	}
}

// TestAppendInvalidBodyIsNoop 非法请求体原样返回（绝不失败）。
//
// Append 在转发关键路径上：宁可少做一次改写，也不能把请求改坏
// 或让请求失败。
func TestAppendInvalidBodyIsNoop(t *testing.T) {
	for _, body := range []string{`not json`, `null`, `[]`, `{"messages":"不是数组"}`} {
		out := Append([]byte(body), "网关提示词")
		if body == `{"messages":"不是数组"}` {
			// 这一种是合法 JSON：会被改写，但**不能 panic**
			_ = out
			continue
		}
		if string(out) != body {
			t.Errorf("%q 应原样返回（不 panic、不改坏），实际：%s", body, out)
		}
	}
}

// ---- 测试辅助 ----

func messagesOf(t *testing.T, body []byte) []json.RawMessage {
	t.Helper()
	var doc struct {
		Messages []json.RawMessage `json:"messages"`
	}
	if err := json.Unmarshal(body, &doc); err != nil {
		t.Fatalf("解析输出失败: %v（原文 %s）", err, body)
	}
	return doc.Messages
}

func roleOf(t *testing.T, raw json.RawMessage) string {
	t.Helper()
	var m struct {
		Role string `json:"role"`
	}
	if err := json.Unmarshal(raw, &m); err != nil {
		t.Fatalf("解析消息失败: %v", err)
	}
	return m.Role
}

func contentOf(t *testing.T, raw json.RawMessage) string {
	t.Helper()
	var m struct {
		Content string `json:"content"`
	}
	if err := json.Unmarshal(raw, &m); err != nil {
		t.Fatalf("解析消息失败: %v", err)
	}
	return m.Content
}
