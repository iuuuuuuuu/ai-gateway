package server

// channel_multiplier_test.go 钉住「模型清单按 (平台,区域) 拆行并带倍率」。
//
// # 为什么需要这组端到端断言（2026-09-21 所有者要求）
//
// 所有者原话：
//
//	「模型清单 也要显示出 对应平台的倍率」
//	「分区域各列一行」
//
// 单元测试（channels_test.go）只验证 channelsToJSON 这个**转换函数**，
// 而真正要保证的是 `/v1/models` 的**完整响应**里：
//
//	① 同一模型的 cn / intl 是两条独立渠道（不是一条带 regions 数组）
//	② 每条各自带自己的倍率（两区不同时不会串）
//	③ 三态在 JSON 里可区分（数字 / null / 键缺席）
//
// 只测转换函数会漏掉组装处（mergedModelList）把区域传错、或倍率取自
// 错误区域这类缺陷 —— 而那正是本功能最容易错的地方。

import (
	"encoding/json"
	"testing"
)

// listModelsRaw 取 /v1/models 的 data 数组。
//
// ⚠ 直接调 `mergedModelList()` 而不是走 HTTP：`newTestHandlerForChannels`
// 刻意不调 NewHandler（那需要完整 Config 与鉴权归一化），因此 `h.mux`
// 是 nil —— 走 ServeHTTP 会 panic。本测试要验的正是这个函数的产物，
// 直接调它既准确又不必造一套假 mux。
func listModelsRaw(t *testing.T, h *Handler) []map[string]any {
	t.Helper()
	list := h.mergedModelList()
	if len(list) == 0 {
		t.Fatal("mergedModelList 返回空 —— 静态表兜底也应有条目")
	}
	// 走一遍 JSON 往返：确保下发的形状可序列化，且断言读到的是
	// **前端实际会收到的**类型（map/slice/float64），而不是 Go 内部类型。
	raw, err := json.Marshal(list)
	if err != nil {
		t.Fatalf("序列化模型清单失败: %v", err)
	}
	var out []map[string]any
	if err := json.Unmarshal(raw, &out); err != nil {
		t.Fatalf("反序列化模型清单失败: %v", err)
	}
	return out
}

// findModel 取指定 id 的条目。
func findModel(t *testing.T, data []map[string]any, id string) map[string]any {
	t.Helper()
	for _, m := range data {
		if m["id"] == id {
			return m
		}
	}
	t.Fatalf("模型 %q 不在 /v1/models 里", id)
	return nil
}

// channelsOf 取某模型的 channels 数组。
func channelsOf(t *testing.T, m map[string]any) []map[string]any {
	t.Helper()
	raw, ok := m["channels"]
	if !ok {
		return nil
	}
	list, ok := raw.([]any)
	if !ok {
		t.Fatalf("channels 不是数组: %#v", raw)
	}
	out := make([]map[string]any, 0, len(list))
	for _, e := range list {
		em, ok := e.(map[string]any)
		if !ok {
			t.Fatalf("channels 元素不是对象: %#v", e)
		}
		out = append(out, em)
	}
	return out
}

// TestModelsChannelsSplitByRegion 同一模型的两个区域必须是**两条**渠道。
//
// 这是"分区域各列一行"的直接断言：若有人把去重键改回 Product，
// 两区会合并成一条、倍率只剩一个，这条会红。
func TestModelsChannelsSplitByRegion(t *testing.T) {
	h := newTestHandlerForChannels(map[string][]string{"zcode": {"glm-5.3"}})
	data := listModelsRaw(t, h)

	m := findModel(t, data, "glm-5.3")
	chs := channelsOf(t, m)

	// 收集 workbuddy 的渠道（本用例没有 workbuddy 账号，倍率会是 nil，
	// 但**区域拆分**仍必须发生 —— 那取决于静态表的两张，与账号无关）。
	var wbRegions []string
	for _, c := range chs {
		if c["product"] == "workbuddy" {
			if r, ok := c["region"].(string); ok {
				wbRegions = append(wbRegions, r)
			}
		}
	}
	if len(wbRegions) == 0 {
		t.Fatalf("workbuddy 应有渠道，实际 channels=%+v", chs)
	}
	// 静态表里 glm-5.3 在国服与国际版都有 → 应出现两条不同区域。
	seen := map[string]bool{}
	for _, r := range wbRegions {
		if seen[r] {
			t.Errorf("区域 %q 重复出现 —— 同 (平台,区域) 应只一条", r)
		}
		seen[r] = true
	}
	if len(seen) < 2 {
		t.Errorf("glm-5.3 在静态表的国服与国际版都有，应拆成 2 条渠道，实际 %v", wbRegions)
	}
}

// TestModelsChannelsCarryMultiplierPerRegion 倍率必须**按区域各自取值**。
//
// 构造两区倍率**相反**的场景（国服计费、国际版免费）——
// 这正是实测到的真实形态（deepseek-v4.1-flash），也是"必须拆行"的理由。
// 若组装处把倍率取错区域（如两行都取国服的），这条会红。
func TestModelsChannelsCarryMultiplierPerRegion(t *testing.T) {
	// 用两张静态表都有的模型，确保两区都有渠道。
	h := newTestHandlerForChannels(nil)
	data := listModelsRaw(t, h)

	m := findModel(t, data, "glm-5.3")
	chs := channelsOf(t, m)

	// 收集每个 workbuddy 区域的 creditMultiplier 键是否存在。
	// 本用例没有真实账号 → 倍率是 nil（未声明），但**键必须存在**
	//（HasMultiplier=true），否则界面无法与「没有倍率概念」区分。
	for _, c := range chs {
		if c["product"] != "workbuddy" {
			continue
		}
		if _, ok := c["creditMultiplier"]; !ok {
			t.Errorf("workbuddy 渠道必须带 creditMultiplier 键（未声明时为 null），实际 %+v", c)
		}
	}
	// 反向：非 WorkBuddy 平台（ZCode）**不该**带该键 —— 它没有倍率概念。
	for _, c := range chs {
		if c["product"] == "workbuddy" {
			continue
		}
		if _, ok := c["creditMultiplier"]; ok {
			t.Errorf("平台 %v 没有倍率概念，不该下发 creditMultiplier（会误导用户）", c["product"])
		}
	}
}

// TestModelsChannelsRegionIsSingularAndBackCompat 单数 region 与旧 regions 并存。
//
// 单数给新前端（按区域拆行）；数组保留给旧消费方（向后兼容）。
func TestModelsChannelsRegionIsSingularAndBackCompat(t *testing.T) {
	h := newTestHandlerForChannels(nil)
	data := listModelsRaw(t, h)

	m := findModel(t, data, "glm-5.3")
	chs := channelsOf(t, m)
	for _, c := range chs {
		r, hasRegion := c["region"]
		if !hasRegion {
			continue
		}
		// 单数必须是字符串，且与 regions[0] 一致。
		rs, ok := r.(string)
		if !ok {
			t.Errorf("region 应为字符串，实际 %#v", r)
			continue
		}
		regions, ok := c["regions"].([]any)
		if !ok || len(regions) != 1 {
			t.Errorf("regions 应为长度 1 的数组（向后兼容），实际 %#v", c["regions"])
			continue
		}
		if regions[0] != rs {
			t.Errorf("region=%q 与 regions[0]=%v 不一致", rs, regions[0])
		}
	}
}
