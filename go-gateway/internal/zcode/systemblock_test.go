package zcode

// systemblock 的单测：钉住官方请求体的**形状**。
//
// # 为什么值得为一块"默认不启用"的代码写测试
//
// 它在 3012 上已被 A/B 证伪（见 systemblock.go 文件头），保留的价值是
// "将来风控策略变化时可随时启用"。而**能被启用**的前提是形状仍然正确 ——
// 形状错了，启用只会让人误判"这条路也不行"。
//
// 故测试盯的是**形状的硬约束**（块数、缓存断点、前缀、调用方内容不被吞），
// 而不是"它能不能过 3012"（那要靠真机 A/B）。

import (
	"strings"
	"testing"
)

// TestOfficialSystemShapeIsExactlyThreeBlocks 恰好 3 个块。
//
// Anthropic 只允许 4 个缓存断点：官方把 3 个用在 system、第 4 个留给
// 最后一条消息。**多发一块就会超限**，而超限的表现是上游报错或静默降级
// 缓存 —— 都不好查。
func TestOfficialSystemShapeIsExactlyThreeBlocks(t *testing.T) {
	blocks, err := BuildOfficialSystem(nil, "GLM-5.3", envInfo{
		Cwd: "/tmp/x", Platform: "win32-x64", Shell: "powershell", OSVersion: "10.0.26100",
		Provider: "zhipu",
	})
	if err != nil {
		t.Fatalf("构造失败: %v", err)
	}
	if len(blocks) != 3 {
		t.Fatalf("官方形状恰好 3 块（Anthropic 缓存断点上限 4，第 4 个留给末条消息），实际 %d", len(blocks))
	}
	for i, b := range blocks {
		if _, ok := b["cache_control"]; !ok {
			t.Errorf("第 %d 块缺 cache_control（官方每块都带 ephemeral）", i+1)
		}
		if b["type"] != "text" {
			t.Errorf("第 %d 块 type 应为 text，实际 %v", i+1, b["type"])
		}
	}
}

// TestOfficialSystemThirdBlockHasNewlinePrefix 第 3 块带 "\n\n" 前缀。
//
// 这是官方形状里最容易被人当"笔误"删掉的一处 —— 参考实现逐字保留了它。
func TestOfficialSystemThirdBlockHasNewlinePrefix(t *testing.T) {
	blocks, err := BuildOfficialSystem(nil, "GLM-5.3", envInfo{Provider: "zai"})
	if err != nil {
		t.Fatalf("构造失败: %v", err)
	}
	text, _ := blocks[2]["text"].(string)
	if !strings.HasPrefix(text, "\n\n") {
		t.Errorf("第 3 块必须以 \\n\\n 开头（官方形状的一部分，不是笔误），实际开头 %q",
			text[:min(12, len(text))])
	}
}

// TestOfficialSystemKeepsCallerSystem 调用方的 system **不被吞掉**。
//
// 整体替换会丢掉客户端的项目规范与工具约定 —— 那是 prompt 模块
// `custom` 模式的已知副作用，这里必须避免：官方块排在调用方**前面**，
// 不是取而代之。
func TestOfficialSystemKeepsCallerSystem(t *testing.T) {
	caller := "PROJECT RULES: 必须用 tabs 缩进"
	blocks, err := BuildOfficialSystem(caller, "GLM-5.3", envInfo{Provider: "zai"})
	if err != nil {
		t.Fatalf("构造失败: %v", err)
	}
	if len(blocks) != 4 {
		t.Fatalf("3 个官方块 + 1 个调用方块 = 4，实际 %d", len(blocks))
	}
	last, _ := blocks[3]["text"].(string)
	if last != caller {
		t.Errorf("调用方的 system 必须原样保留在最后，实际 %q", last)
	}
}

// TestOfficialSystemStripsCallerCacheControl 剥掉调用方自带的缓存断点。
//
// 官方 3 块 + 末条消息 = 4 个断点，正好用满。客户端自带的**外来断点**
// 会超出上限 —— 而真实客户端自己拥有整个请求体，从不发外来断点。
func TestOfficialSystemStripsCallerCacheControl(t *testing.T) {
	caller := []any{
		map[string]any{"type": "text", "text": "规则 A", "cache_control": map[string]any{"type": "ephemeral"}},
		map[string]any{"type": "text", "text": "规则 B"},
	}
	blocks, err := BuildOfficialSystem(caller, "GLM-5.3", envInfo{Provider: "zai"})
	if err != nil {
		t.Fatalf("构造失败: %v", err)
	}
	user := blocks[3:]
	if len(user) != 2 {
		t.Fatalf("调用方 2 块应全部保留，实际 %d", len(user))
	}
	for i, b := range user {
		if _, ok := b["cache_control"]; ok {
			t.Errorf("调用方第 %d 块的 cache_control 必须被剥掉（否则超出 4 个断点上限）", i+1)
		}
	}
	if user[0]["text"] != "规则 A" {
		t.Error("剥 cache_control 不该影响 text 内容")
	}
}

// TestEnvironmentSectionOmitsEmptyValues 取不到的值**整行省略**，不写 "unknown"。
//
// 与身份头同一取舍：填假值比不发更容易被识别成"非真实客户端" ——
// 一个真实客户端不会把自己的 OS 版本写成 "unknown"。
func TestEnvironmentSectionOmitsEmptyValues(t *testing.T) {
	blocks, err := BuildOfficialSystem(nil, "GLM-5.3", envInfo{
		Cwd: "/w", Platform: "win32-x64",
		// Shell / OSVersion 故意留空
		Provider: "zai",
	})
	if err != nil {
		t.Fatalf("构造失败: %v", err)
	}
	text, _ := blocks[2]["text"].(string)
	if strings.Contains(text, "unknown") {
		t.Error("取不到的值应整行省略，不能写 unknown")
	}
	if !strings.Contains(text, "win32-x64") {
		t.Error("拿到的值应当出现（Platform）")
	}
}

// TestContextPrefixMessageShape meta_user 的形状。
func TestContextPrefixMessageShape(t *testing.T) {
	msg, err := BuildContextPrefixMessage("2026-09-19")
	if err != nil {
		t.Fatalf("构造失败: %v", err)
	}
	if msg["role"] != "user" {
		t.Errorf("meta_user 的 role 应为 user，实际 %v", msg["role"])
	}
	content, ok := msg["content"].([]map[string]any)
	if !ok || len(content) != 1 {
		t.Fatalf("content 应为单块数组，实际 %#v", msg["content"])
	}
	text, _ := content[0]["text"].(string)
	if !strings.HasPrefix(text, "<system-reminder>") || !strings.HasSuffix(text, "</system-reminder>") {
		t.Errorf("必须以 <system-reminder> 包裹，实际 %q", text[:min(40, len(text))])
	}
	if !strings.Contains(text, "2026-09-19") {
		t.Error("日期应被替换进文案")
	}
}

// TestSystemBlockEnabledByDefault **默认开启**（2026-09-20 反转的契约）。
//
// # 为什么这条测试从"默认关闭"反转成"默认开启"
//
// 它原先守的是「不要把已证伪的假设默认打开」，依据是一次 A/B：
// 开/关两组都 3012。
//
// **但那次 A/B 是在风控期做的** —— 当时该账号连官方客户端都一律吃 3012。
// 在"所有请求都失败"的窗口里比较两个方案，结论必然无差别。
// 那是**实验设计的错误**，不是方案无效。
//
// 对照参考实现（`gakiyukr/zcode2api-plus`，Go，持续更新，能跑通）确认：
//
//	// JWT 账号请求必须注入到顶层 system，否则上游返回 405。
//
// 而 405 正是 3012 的载体。故默认改为注入。
//
// ⚠ 保留 `=0` 作为逃生开关：万一注入在某个环境反而出问题，
// 用户能不改代码就关掉。
func TestSystemBlockEnabledByDefault(t *testing.T) {
	// 未设置 = 默认开启
	t.Setenv("AI_GATEWAY_ZCODE_SYSTEM_BLOCK", "")
	if !SystemBlockEnabled() {
		t.Error("默认必须**开启** —— 参考实现证明 JWT 账号缺官方 system 会被回 405(=3012)")
	}
	// 显式 1/true 仍表示开启
	for _, v := range []string{"1", "true", "TRUE"} {
		t.Setenv("AI_GATEWAY_ZCODE_SYSTEM_BLOCK", v)
		if !SystemBlockEnabled() {
			t.Errorf("%q 应视为开启", v)
		}
	}
	// 显式 0/false 才关（逃生开关）
	for _, v := range []string{"0", "false", "FALSE"} {
		t.Setenv("AI_GATEWAY_ZCODE_SYSTEM_BLOCK", v)
		if SystemBlockEnabled() {
			t.Errorf("%q 应视为关闭（逃生开关）", v)
		}
	}
}

func min(a, b int) int {
	if a < b {
		return a
	}
	return b
}
