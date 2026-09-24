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
// （只是模型不带 Qoder/ZCode 的渠道）。缺一份可选配置不该让整个接口失败。
// TestMergedModelListExposesAliasesAsIDs 组合路由名既要在 aliases 中保留，
// 也要作为独立 data[].id 返回，兼容只读取 id 的 OpenAI 客户端。
func TestMergedModelListExposesAliasesAsIDs(t *testing.T) {
	h := newTestHandlerForChannels(map[string][]string{
		"zcode": {"glm-5.3"},
	})
	items := h.mergedModelList()
	want := map[string]bool{"glm-5.3": false, "zcode:glm-5.3": false}
	for _, item := range items {
		id, _ := item["id"].(string)
		if _, ok := want[id]; ok {
			want[id] = true
		}
	}
	for id, ok := range want {
		if !ok {
			t.Errorf("/v1/models 应返回独立模型 id %q，实际未找到", id)
		}
	}
}

func TestQwen38FlashContextMatchesOfficialQoder(t *testing.T) {
	h := newTestHandlerForChannels(map[string][]string{"qoder": {"Qwen3.8-Flash"}})
	for _, item := range h.mergedModelList() {
		if item["id"] == "Qwen3.8-Flash" {
			if got, _ := item["context_length"].(int64); got != 1000000 {
				t.Fatalf("Qwen3.8-Flash context_length 应为官方 1M，实际 %#v", item["context_length"])
			}
			return
		}
	}
	t.Fatal("未找到 Qwen3.8-Flash 模型项")
}

func TestMergedAliasChannelsRespectExplicitRoute(t *testing.T) {
	h := newTestHandlerForChannels(map[string][]string{"zcode": {"glm-5.3"}})
	for _, item := range h.mergedModelList() {
		if item["id"] != "zcode:glm-5.3" {
			continue
		}
		channels, _ := item["channels"].([]map[string]any)
		if len(channels) != 1 || channels[0]["product"] != "zcode" {
			t.Fatalf("zcode 组合项只能显示 zcode 渠道，实际 %#v", item["channels"])
		}
		return
	}
	t.Fatal("未找到 zcode:glm-5.3 独立模型项")
}

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

// TestAppendChannelSplitsByRegion 同平台的不同区域**各成一条**（2026-09-21 反转）。
//
// # 为什么反转（所有者要求）
//
// 所有者原话：
//
//	「模型清单 也要显示出 对应平台的倍率」+「分区域各列一行」
//
// 旧行为是同平台合并区域（一条 "WorkBuddy" 带 regions:["cn","intl"]）。
// 但**倍率按区域不同** —— 实测 deepseek-v4.1-flash 国服 x0.03、国际版 x0.00。
// 合并后一条渠道只能承载一个倍率，两区不同时必然有一个显示错，
// 而"显示错的倍率"比"不显示"更糟：用户会据它判断该烧哪个账号的额度。
//
// 故现在 (产品, 区域) 一条渠道，界面上一行一个 —— 正是"分区域各列一行"。
func TestAppendChannelSplitsByRegion(t *testing.T) {
	var list []productChannel
	list = appendChannel(list, productChannel{Product: "workbuddy", Label: "WorkBuddy", Region: "cn"})
	list = appendChannel(list, productChannel{Product: "workbuddy", Label: "WorkBuddy", Region: "intl"})

	if len(list) != 2 {
		t.Fatalf("同一平台的两个区域应各成一条（倍率不同，必须分开），实际 %d：%+v", len(list), list)
	}
	if list[0].Region != "cn" || list[1].Region != "intl" {
		t.Errorf("两条渠道应分别带 cn / intl，实际 %q / %q", list[0].Region, list[1].Region)
	}

	// 再加一个**不同**平台 → 应变成三条
	list = appendChannel(list, productChannel{Product: "zcode", Label: "ZCode"})
	if len(list) != 3 {
		t.Fatalf("不同平台应各成一条，实际 %d：%+v", len(list), list)
	}
}

// TestAppendChannelDedupesSameProductAndRegion 同 (平台, 区域) 重复加不出重复项。
func TestAppendChannelDedupesSameProductAndRegion(t *testing.T) {
	var list []productChannel
	list = appendChannel(list, productChannel{Product: "workbuddy", Region: "cn"})
	list = appendChannel(list, productChannel{Product: "workbuddy", Region: "cn"})
	if len(list) != 1 {
		t.Fatalf("同 (平台, 区域) 不该重复，实际 %d：%+v", len(list), list)
	}
}

// TestAppendChannelPrefersDeclaredMultiplier 倍率取**已声明的那个**，不被 nil 覆盖。
//
// 为什么这条重要：同一条渠道会被多个来源追加（动态真值 + 静态兜底），
// 而静态表**不编造倍率**（手抄表里没有 credits）。若让后来的 nil 覆盖了
// 先到的真值，用户会看到「—」（未声明）而不是真实倍率 —— 功能等于没做。
func TestAppendChannelPrefersDeclaredMultiplier(t *testing.T) {
	three := 0.03
	// 先加带真值的一条
	list := appendChannel(nil, productChannel{Product: "workbuddy", Region: "cn", CreditMultiplier: &three, HasMultiplier: true})
	// 再加同 (平台,区域) 但倍率为 nil 的一条（模拟静态表补录）
	list = appendChannel(list, productChannel{Product: "workbuddy", Region: "cn", HasMultiplier: true})

	if len(list) != 1 {
		t.Fatalf("同 (平台,区域) 应合并，实际 %d", len(list))
	}
	if list[0].CreditMultiplier == nil {
		t.Fatal("已声明的倍率被 nil 覆盖了 —— 用户会看到「—」而不是真实倍率")
	}
	if *list[0].CreditMultiplier != 0.03 {
		t.Errorf("倍率=%v want 0.03", *list[0].CreditMultiplier)
	}

	// 反向：先 nil 后真值 → 应取真值
	list2 := appendChannel(nil, productChannel{Product: "workbuddy", Region: "cn", HasMultiplier: true})
	list2 = appendChannel(list2, productChannel{Product: "workbuddy", Region: "cn", CreditMultiplier: &three, HasMultiplier: true})
	if list2[0].CreditMultiplier == nil || *list2[0].CreditMultiplier != 0.03 {
		t.Errorf("后到的真值应补上，实际 %+v", list2[0].CreditMultiplier)
	}
}

// TestChannelsToJSONHasProductAndLabel 同时下发标识与显示名。
//
// 只给标识 → 界面要自己维护映射表（改文案要动两处）；
// 只给显示名 → 界面没法按平台过滤/分组。
func TestChannelsToJSONHasProductAndLabel(t *testing.T) {
	out := channelsToJSON([]productChannel{
		{Product: "zcode", Label: "ZCode", Region: "cn"},
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
	if out[0]["region"] != "cn" {
		t.Errorf("有区域时应下发单数 region，实际 %v", out[0]["region"])
	}
	// regions（复数）保留给旧消费方，向后兼容。
	if _, ok := out[0]["regions"]; !ok {
		t.Error("有区域时也应下发 regions（旧前端仍读它）")
	}

	// 没有区域时不下发该键（而不是下发空数组 —— 空数组会让界面
	// 以为"查过了但没有区域"，而实际是"区域信息不适用"）
	bare := channelsToJSON([]productChannel{{Product: "qoder", Label: "Qoder"}})
	if _, ok := bare[0]["region"]; ok {
		t.Error("没有区域时不该下发 region 键")
	}
	if _, ok := bare[0]["regions"]; ok {
		t.Error("没有区域时不该下发 regions 键")
	}
}

// TestChannelsToJSONCarriesMultiplier 渠道带倍率，且三态可区分（2026-09-21 新增）。
//
// 所有者要求：「模型清单 也要显示出 对应平台的倍率」。
//
// 三种状态必须能在 JSON 里区分：
//
//	数字  → 上游声明的倍率
//	null  → 上游**未声明**（不知道，**不是**免费）
//	缺席  → 该平台没有倍率概念（ZCode 目前如此）
//
// 把 null 当成 0（免费）会让用户以为不扣积分，而它可能正在烧额度 ——
// 这是本功能最容易犯的错，故用测试钉死。
func TestChannelsToJSONCarriesMultiplier(t *testing.T) {
	three := 0.03
	zero := 0.0
	out := channelsToJSON([]productChannel{
		// ① 已声明（计费）
		{Product: "workbuddy", Label: "WorkBuddy", Region: "cn", CreditMultiplier: &three, HasMultiplier: true},
		// ② 已声明（免费 —— 与"未声明"必须区分）
		{Product: "workbuddy", Label: "WorkBuddy", Region: "intl", CreditMultiplier: &zero, HasMultiplier: true},
		// ③ 未声明（null，不是 0）
		{Product: "workbuddy", Label: "WorkBuddy", Region: "cn", CreditMultiplier: nil, HasMultiplier: true},
		// ④ 该平台没有倍率概念（键缺席）
		{Product: "zcode", Label: "ZCode", Region: "cn", HasMultiplier: false},
	})
	if len(out) != 4 {
		t.Fatalf("应有 4 条，实际 %d", len(out))
	}

	if v, ok := out[0]["creditMultiplier"].(*float64); !ok || v == nil || *v != 0.03 {
		t.Errorf("① 应为 0.03，实际 %v", out[0]["creditMultiplier"])
	}
	if v, ok := out[1]["creditMultiplier"].(*float64); !ok || v == nil || *v != 0 {
		t.Errorf("② 应为 0（免费），实际 %v", out[1]["creditMultiplier"])
	}
	// ③ 未声明：键存在但值是 nil 指针 → 序列化成 null
	v3, has3 := out[2]["creditMultiplier"]
	if !has3 {
		t.Error("③ 未声明时键应存在（值为 null），否则界面无法与「没有倍率概念」区分")
	}
	if p, ok := v3.(*float64); !ok || p != nil {
		t.Errorf("③ 未声明应为 nil 指针（→ null），实际 %#v", v3)
	}
	// ④ 没有倍率概念：键缺席
	if _, ok := out[3]["creditMultiplier"]; ok {
		t.Error("④ 该平台没有倍率概念时不该下发 creditMultiplier 键")
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
