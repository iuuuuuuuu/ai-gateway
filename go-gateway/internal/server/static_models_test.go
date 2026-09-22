package server

import (
	"encoding/json"
	"testing"
)

// ---------------------------------------------------------------------------
// 静态表校正回归
//
// 静态表是动态拉取失败时的回退，必须与 /v3/config 的真实清单一致。
//
// 历史缺陷：国际版静态表抄自本地缓存 acc-product-config-v3.json，其中
//   - gpt-5.3-codex 属于 CodeBuddy 产品清单，不在 WorkBuddy 的 cli 清单里；
//   - 缺 kimi-k2.8-preview、hy4-preview-f（实测在国际版均可正常调用）。
//
// 同时 context_length 原先一律填 131072（凭感觉的兜底值），
// 与真实 maxInputTokens 差距很大（如 gpt-5.6-sol 实为 1000000），
// 会让客户端低估可用窗口、过早触发「上下文超长」。
// ---------------------------------------------------------------------------

// staticIntlIDs 提取国际版静态表的 id 列表。
func staticIntlIDs() []string {
	out := make([]string, 0, len(staticModelsIntl))
	for _, m := range staticModelsIntl {
		if id, _ := m["id"].(string); id != "" {
			out = append(out, id)
		}
	}
	return out
}

// TestStaticModelsIntlMatchesV3Config 国际版静态表应与实测的 cli 清单一致。
//
// # ⚠ hy4-preview 已按 2026-09-21 的真值移除
//
// 所有者反馈：「国际版既然不支持，为什么你还能跑出来这个模型？接口返回
// 没有就不要搞出来」。
//
// 本测试原先的 `want` 里有 `hy4-preview`，依据是「实测 2026-09-15」。
// 而 2026-09-21 用他的 5 个国际版账号重新拉真值（`/v1/models/regions`，
// `truth_source: dynamic`）：
//
//	国际版 22 个模型 → **不含 hy4-preview**，含 hy4-preview-f（另一个模型）
//
// 即旧记录已过期，上游把 `hy4-preview` 从国际版拿掉了。本测试随之更新 ——
// **测试要跟随真值，不能让真值迁就过期的测试**。
func TestStaticModelsIntlMatchesV3Config(t *testing.T) {
	// 实测（2026-09-21，/v3/config 的 data.agents[name=="cli"].models，
	// truth_source=dynamic）。比 09-15 那次少 hy4-preview、多
	// deepseek-v4.1-flash-sg（同一批真值里出现）。
	want := []string{
		"default-model", "fast-model", "balanced-model", "primary-model", "deep-model",
		"hy4-preview-f", "hy3", "deepseek-v4.1-flash", "gpt-6-astra",
		"gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna", "gpt-5.5", "gpt-5.4",
		"gemini-3.5-flash", "glm-5.3", "glm-5.2", "kimi-k3", "kimi-k2.8-preview", "kimi-k2.6",
	}

	got := staticIntlIDs()
	// ⚠ 只断言「want 里的都在」+「不该有的不在」，**不断言总数相等**。
	//
	// 为什么不相等也要通过：静态表是**手抄兜底**，真值变动时它必然滞后
	//（这正是本次要修的缺陷的根源）。用总数相等做断言会让"上游新增一个模型"
	// 变成测试失败 —— 那是把"表该更新了"误报成"代码坏了"。
	// 真正的事实来源是动态真值，静态表只在拉不到时兜底。
	for _, w := range want {
		if !containsStr(got, w) {
			t.Errorf("国际版静态表缺少 %q（实测 /v3/config 的 cli 清单里有）", w)
		}
	}
	// gpt-5.3-codex 属于 CodeBuddy 清单，不该出现在 WorkBuddy 的国际版静态表里
	if containsStr(got, "gpt-5.3-codex") {
		t.Error("gpt-5.3-codex 属于 CodeBuddy 产品清单，不在 WorkBuddy 的 cli 清单里，不应出现在国际版静态表")
	}
	// ⚠ 回归保护：hy4-preview **不该**在国际版表里（它只在国服真值里）。
	//
	// 它此前同时躺在两张表里，于是 `/v1/models` 把国际版标成支持它，
	// 而国际版真值里根本没有 —— 用户按清单用就失败。
	if containsStr(got, "hy4-preview") {
		t.Error("hy4-preview 不在国际版真值里（实测 2026-09-21），不得出现在国际版静态表 —— " +
			"它会让 /v1/models 编造一个国际版用不了的模型")
	}
}

// TestStaticModelsIntlHasNewlyAddedModels 锁住本次新增的两个模型。
func TestStaticModelsIntlHasNewlyAddedModels(t *testing.T) {
	got := staticIntlIDs()
	for _, want := range []string{"kimi-k2.8-preview", "hy4-preview-f"} {
		if !containsStr(got, want) {
			t.Errorf("国际版静态表应含 %q（实测可用，此前遗漏）", want)
		}
	}
}

// TestStaticModelsIntlContextLengthsMatchUpstream 关键模型的 context_length 应对齐真实值。
//
// 全部填 131072 会让客户端低估窗口（如 gpt-5.6-sol 实为 1000000）。
func TestStaticModelsIntlContextLengthsMatchUpstream(t *testing.T) {
	want := map[string]int{
		"gpt-5.6-sol":        1000000,
		"gpt-5.6-terra":      1000000,
		"gpt-5.6-luna":       1000000,
		"gpt-5.5":            1000000,
		"gemini-3.5-flash":   1000000,
		"glm-5.3":            1000000,
		"glm-5.2":            1000000,
		"kimi-k3":            1000000,
		"gpt-6-astra":        400000,
		"deepseek-v4.1-flash": 300000,
		"kimi-k2.8-preview":  300000,
		"hy4-preview-f":      300000,
		"gpt-5.4":            272000,
		"balanced-model":     256000,
		"kimi-k2.6":          256000,
		"default-model":      200000,
		"fast-model":         200000,
		"deep-model":         200000,
		"hy3":                192000,
	}
	// ⚠ `hy4-preview` 已从本表移除（它不在国际版真值里，见
	// TestStaticModelsIntlMatchesV3Config 的注释）—— 故这里也不再断言它。
	byID := map[string]int{}
	for _, m := range staticModelsIntl {
		id, _ := m["id"].(string)
		cl, _ := m["context_length"].(int)
		byID[id] = cl
	}
	for id, cl := range want {
		got, ok := byID[id]
		if !ok {
			t.Errorf("静态表缺少 %q", id)
			continue
		}
		if got != cl {
			t.Errorf("%s 的 context_length 应为 %d（实测 maxInputTokens），实际 %d", id, cl, got)
		}
	}
}

// TestStaticModelsHaveRequiredFields 静态表每项都要有必需字段且取值合法。
func TestStaticModelsHaveRequiredFields(t *testing.T) {
	check := func(name string, table []map[string]any) {
		for i, m := range table {
			for _, k := range []string{"id", "object", "created", "owned_by", "context_length"} {
				if _, ok := m[k]; !ok {
					t.Errorf("%s[%d] 缺少字段 %q: %v", name, i, k, m)
				}
			}
			if id, _ := m["id"].(string); id == "" {
				t.Errorf("%s[%d] id 为空", name, i)
			}
			if obj, _ := m["object"].(string); obj != "model" {
				t.Errorf("%s[%d] object 应为 \"model\"，实际 %q", name, i, obj)
			}
			// context_length 必须为正，客户端据此判断能否装下请求
			cl, ok := m["context_length"].(int)
			if !ok || cl <= 0 {
				t.Errorf("%s[%d] context_length 非法: %v", name, i, m["context_length"])
			}
		}
	}
	check("staticModels", staticModels)
	check("staticModelsIntl", staticModelsIntl)
}

// TestStaticModelsNoDuplicateIDs 表内不应有重复 id。
func TestStaticModelsNoDuplicateIDs(t *testing.T) {
	check := func(name string, table []map[string]any) {
		seen := map[string]bool{}
		for _, m := range table {
			id, _ := m["id"].(string)
			if seen[id] {
				t.Errorf("%s 里 %q 重复", name, id)
			}
			seen[id] = true
		}
	}
	check("staticModels", staticModels)
	check("staticModelsIntl", staticModelsIntl)
}

// TestStaticModelsAllDedupes 并集要按 id 去重，且两个区域都在。
func TestStaticModelsAllDedupes(t *testing.T) {
	seen := map[string]bool{}
	for _, m := range staticModelsAll {
		id, _ := m["id"].(string)
		if id == "" {
			t.Error("并集里出现空 id")
			continue
		}
		if seen[id] {
			t.Errorf("并集里 %q 重复", id)
		}
		seen[id] = true
	}
	if !seen["glm-5.2"] {
		t.Error("并集应含国服模型 glm-5.2")
	}
	if !seen["gpt-6-astra"] {
		t.Error("并集应含国际版模型 gpt-6-astra")
	}
}

// TestStaticModelsJSONShape 静态表要能序列化成 OpenAI /v1/models 形状。
func TestStaticModelsJSONShape(t *testing.T) {
	raw, err := json.Marshal(staticModelsAll)
	if err != nil {
		t.Fatalf("序列化失败: %v", err)
	}
	var out []map[string]any
	if err := json.Unmarshal(raw, &out); err != nil {
		t.Fatalf("反序列化失败: %v", err)
	}
	if len(out) == 0 {
		t.Fatal("并集不应为空")
	}
	if out[0]["object"] != "model" {
		t.Errorf("object 字段应为 \"model\"，实际 %v", out[0]["object"])
	}
}

func containsStr(xs []string, want string) bool {
	for _, x := range xs {
		if x == want {
			return true
		}
	}
	return false
}
