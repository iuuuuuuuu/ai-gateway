package zcode

// 官方 system 块**必须真的注入**到 Anthropic 请求体里（2026-09-20）。
//
// # 背景：这是一条"写了但没接上"的链路
//
// `systemblock.go` 早就实现了官方形状的 system 块（3 块 + cache_control），
// 形状也对（有单测）。但 `BuildOfficialSystem` 在**生产代码里零调用点** ——
// 只有测试调它。
//
// 于是 `BuildAnthropicBodyWithMeta` 只从客户端 messages 里抽 system：
//
//	for _, rm := range rawMsgs { case "system", "developer": … }
//
// 而普通 API 客户端（Cline / DSH / curl）**根本不发 system 消息** ⇒
// `systemBlocks` 为空 ⇒ 官方块从未注入。
//
// # 为什么这是 3012 的关键
//
// 参考实现 `gakiyukr/zcode2api-plus`（Go，持续更新，能跑通）在
// `internal/upstream/request.go` 写得很直接：
//
//	// JWT 账号请求必须注入到顶层 system，否则上游返回 405。
//
// 而 **405 正是 3012 的载体**（见 captcha.go：`HTTP 405 {"code":3012,…}`）。
//
// # 为什么之前的"已证伪"结论站不住
//
// `systemblock.go` 文件头记着一次 A/B：「开/关两组都 3012」。
// 但那次是在**风控期**做的 —— 当时连官方客户端都一律吃 3012。
// 在"所有请求都失败"的窗口里比较两个方案，结论必然无差别。
// 那是**实验设计的错误**，不是方案无效。
//
// 故这些测试锁的是：**注入必须发生**（而不是"环境变量打开才发生"）。

import (
	"encoding/json"
	"testing"
)

// minimalOpenAIBody 一个**不含 system** 的请求体 —— 复现普通客户端的形状。
func minimalOpenAIBody(t *testing.T) []byte {
	t.Helper()
	return []byte(`{"model":"GLM-5.3","messages":[{"role":"user","content":"你好"}]}`)
}

// systemOf 取出翻译结果的 system 字段。
func systemOf(t *testing.T, raw []byte) []any {
	t.Helper()
	var m map[string]any
	if err := json.Unmarshal(raw, &m); err != nil {
		t.Fatalf("翻译产物不是合法 JSON: %v", err)
	}
	blocks, _ := m["system"].([]any)
	return blocks
}

// TestSystemBlockInjectedWithoutClientSystem 客户端没发 system 时，官方块仍要注入。
//
// 这是本次修复的核心断言：普通客户端（Cline/DSH/curl）不发 system，
// 而 JWT 账号缺官方 system 会被上游回 405（= 3012 的载体）。
func TestSystemBlockInjectedWithoutClientSystem(t *testing.T) {
	out, err := BuildAnthropicBodyWithMeta(minimalOpenAIBody(t), "GLM-5.3", AnthropicMeta{})
	if err != nil {
		t.Fatalf("翻译失败: %v", err)
	}
	blocks := systemOf(t, out)
	if len(blocks) == 0 {
		t.Fatal("客户端没发 system 时，官方 system 块**必须**被注入 —— " +
			"否则上游回 405（= 3012），这正是 ZCode 对话接不上的原因")
	}

	// 官方 3 块必须都在（形状：cliPrefix + stable + dynamic）
	if len(blocks) < 3 {
		t.Errorf("官方块应有 3 个（cliPrefix/stable/dynamic），实际 %d", len(blocks))
	}
	// 第 1 块必须是 "You are ZCode, …"（官方身份声明）
	first, _ := blocks[0].(map[string]any)
	text, _ := first["text"].(string)
	if text == "" {
		t.Error("第 1 块应有文本")
	}
	// 每块都要带 cache_control（官方形状的一部分，Anthropic 靠它做提示缓存）
	for i, b := range blocks {
		bm, _ := b.(map[string]any)
		if _, ok := bm["cache_control"]; !ok {
			t.Errorf("第 %d 块缺 cache_control —— 官方 3 块都带它", i+1)
		}
	}
}

// TestSystemBlockKeepsClientSystem 客户端的 system **不能丢**。
//
// 替换而不是追加会丢掉客户端的项目规范与工具约定
//（那是 `prompt` 模块 `custom` 模式的已知副作用）。
func TestSystemBlockKeepsClientSystem(t *testing.T) {
	body := []byte(`{"model":"GLM-5.3","messages":[
		{"role":"system","content":"我是客户端的项目规范：只用 tabs 缩进"},
		{"role":"user","content":"你好"}]}`)
	out, err := BuildAnthropicBodyWithMeta(body, "GLM-5.3", AnthropicMeta{})
	if err != nil {
		t.Fatalf("翻译失败: %v", err)
	}
	blocks := systemOf(t, out)

	foundClient := false
	for _, b := range blocks {
		bm, _ := b.(map[string]any)
		if txt, _ := bm["text"].(string); txt != "" {
			if len(txt) >= 6 && containsStr(txt, "项目规范") {
				foundClient = true
			}
		}
	}
	if !foundClient {
		t.Errorf("客户端自己的 system 必须保留（官方块应**排在它前面**，不是替换它）；"+
			"实际 %d 块：%v", len(blocks), blocks)
	}
	// 官方块仍要在
	if len(blocks) < 4 {
		t.Errorf("官方 3 块 + 客户端 1 块 = 至少 4 块，实际 %d", len(blocks))
	}
}

// TestSystemBlockOrderOfficialFirst 官方块必须**排在客户端内容前面**。
func TestSystemBlockOrderOfficialFirst(t *testing.T) {
	body := []byte(`{"model":"GLM-5.3","messages":[
		{"role":"system","content":"CLIENT_MARKER_XYZ"},
		{"role":"user","content":"hi"}]}`)
	out, err := BuildAnthropicBodyWithMeta(body, "GLM-5.3", AnthropicMeta{})
	if err != nil {
		t.Fatalf("翻译失败: %v", err)
	}
	blocks := systemOf(t, out)

	clientIdx := -1
	for i, b := range blocks {
		bm, _ := b.(map[string]any)
		if txt, _ := bm["text"].(string); containsStr(txt, "CLIENT_MARKER_XYZ") {
			clientIdx = i
			break
		}
	}
	if clientIdx < 3 {
		t.Errorf("客户端内容应排在官方 3 块**之后**（index >= 3），实际 index=%d", clientIdx)
	}
}

