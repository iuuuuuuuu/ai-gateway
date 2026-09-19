package server

// `channels` 字段（模型来自哪个平台）的行为。
//
// # 需求
//
// 「智能体管理,哪里显示出来的模型,现在可以加一个渠道,是来自于哪个平台,
//   如果重叠,就显示多个平台」
//
// # 要钉住的三件事
//
//   1. **平台标识与显示名都下发** —— 只给标识界面要自己映射，只给显示名
//      界面没法按平台过滤
//   2. **同一平台的多个区域算一条渠道**（区域合并）—— 否则 WorkBuddy 的
//      国服+国际版会让每个模型出现两条 "WorkBuddy"，看起来像重复
//   3. **重叠时真的出现多条** —— 这是需求的核心（"如果重叠就显示多个平台"）
//
// 另外：缺配置时不能报错（降级为只有 WorkBuddy 的渠道）。

import (
	"testing"

	"workbuddy2api/internal/pool"
)

// newTestHandlerForChannels 造一个只填了 channels 所需字段的 handler。
//
// 不调 NewHandler：那会做鉴权归一化、缓存等一堆与渠道无关的事，
// 且需要完整的 Config。这里直接构造，只测我们关心的那段。
func newTestHandlerForChannels(productModels map[string][]string) *Handler {
	h := &Handler{}
	h.cfg = Config{
		Pool:          pool.New(""),
		ProductModels: productModels,
	}
	return h
}

// TestProductModelsReadsConfigFromConfig 从配置读模型清单。
func TestProductModelsReadsConfigFromConfig(t *testing.T) {
	h := newTestHandlerForChannels(map[string][]string{
		"zcode": {"glm-5.3", "glm-5.3-flash"},
		"qoder": {"Qwen3.8-Max"},
	})
	got := h.productModels()

	if len(got) != 3 {
		t.Fatalf("应读出 3 条，实际 %d：%+v", len(got), got)
	}
	// 顺序按产品名排序（可复现）
	if got[0].Product != "qoder" {
		t.Errorf("产品顺序应按名排序，首条实际 %q", got[0].Product)
	}
	if got[1].Product != "zcode" || got[2].Product != "zcode" {
		t.Errorf("zcode 的两条应相邻，实际 %+v", got)
	}
}

// TestProductModelsEmptyConfigIsSafe 缺配置时不报错、返回空。
//
// 这是**降级路径**：没有 product_models 时 /v1/models 仍要能正常返回
//（只是模型不带 Qoder/ZCode 的渠道）。缺一份可选配置不该让整个接口失败。
func TestProductModelsEmptyConfigIsSafe(t *testing.T) {
	for name, cfg := range map[string]map[string][]string{
		"nil":   nil,
		"empty": {},
		"含空串":   {"zcode": {"", "  ", "glm-5.3"}},
	} {
		t.Run(name, func(t *testing.T) {
			h := newTestHandlerForChannels(cfg)
			got := h.productModels()
			for _, m := range got {
				if m.ID == "" {
					t.Errorf("不该产出空 id：%+v", got)
				}
			}
			if name == "含空串" && len(got) != 1 {
				t.Errorf("空白项应被丢弃，只剩 glm-5.3，实际 %+v", got)
			}
		})
	}
}

// TestAppendChannelDedupesSameProduct 同平台的多个区域合并成一条。
//
// 若不去重，WorkBuddy 的 cn + intl 会让每个模型都出现两条 "WorkBuddy" ——
// 界面上看起来像重复，而需求要的是"不同平台"才算多条。
func TestAppendChannelDedupesSameProduct(t *testing.T) {
	var list []productChannel
	list = appendChannel(list, productChannel{Product: "workbuddy", Label: "WorkBuddy", Regions: []string{"cn"}})
	list = appendChannel(list, productChannel{Product: "workbuddy", Label: "WorkBuddy", Regions: []string{"intl"}})

	if len(list) != 1 {
		t.Fatalf("同一平台应合并为 1 条，实际 %d：%+v", len(list), list)
	}
	if len(list[0].Regions) != 2 {
		t.Errorf("两个区域都应保留，实际 %+v", list[0].Regions)
	}

	// 再加一个**不同**平台 → 应变成两条（这就是"重叠显示多个平台"）
	list = appendChannel(list, productChannel{Product: "zcode", Label: "ZCode"})
	if len(list) != 2 {
		t.Fatalf("不同平台应各成一条，实际 %d：%+v", len(list), list)
	}
}

// TestAppendChannelIgnoresDuplicateRegion 同区域重复加不出重复项。
func TestAppendChannelIgnoresDuplicateRegion(t *testing.T) {
	var list []productChannel
	list = appendChannel(list, productChannel{Product: "workbuddy", Regions: []string{"cn"}})
	list = appendChannel(list, productChannel{Product: "workbuddy", Regions: []string{"cn"}})
	if len(list[0].Regions) != 1 {
		t.Errorf("同区域不该重复，实际 %+v", list[0].Regions)
	}
}

// TestChannelsToJSONHasProductAndLabel 同时下发标识与显示名。
//
// 只给标识 → 界面要自己维护映射表（改文案要动两处）；
// 只给显示名 → 界面没法按平台过滤/分组。
func TestChannelsToJSONHasProductAndLabel(t *testing.T) {
	out := channelsToJSON([]productChannel{
		{Product: "zcode", Label: "ZCode", Regions: []string{"cn"}},
	})
	if len(out) != 1 {
		t.Fatalf("应有一条，实际 %d", len(out))
	}
	if out[0]["product"] != "zcode" {
		t.Errorf("product 应为 zcode，实际 %v", out[0]["product"])
	}
	if out[0]["label"] != "ZCode" {
		t.Errorf("label 应为 ZCode，实际 %v", out[0]["label"])
	}
	if _, ok := out[0]["regions"]; !ok {
		t.Error("有区域时应下发 regions")
	}

	// 没有区域时不下发该键（而不是下发空数组 —— 空数组会让界面
	// 以为"查过了但没有区域"，而实际是"区域信息不适用"）
	bare := channelsToJSON([]productChannel{{Product: "qoder", Label: "Qoder"}})
	if _, ok := bare[0]["regions"]; ok {
		t.Error("没有区域时不该下发 regions 键")
	}
}

// TestChannelLabelsCoverAllProducts 每个已知产品都有显示名。
//
// 漏一个会让界面显示英文标识（如 `zcode`）而不是「ZCode」——
// 而同项目的账号页用的是「ZCode」，两处不一致。
func TestChannelLabelsCoverAllProducts(t *testing.T) {
	for _, p := range []string{"workbuddy", "qoder", "zcode"} {
		if channelLabels[p] == "" {
			t.Errorf("产品 %q 缺显示名（channelLabels）", p)
		}
	}
}
