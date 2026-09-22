package server

// static_fallback_test.go 钉住「静态兜底表不得编造可用性」。
//
// # 为什么需要它（2026-09-21 所有者报的缺陷）
//
// 所有者原话：
//
//	「国际版既然不支持，为什么你还能跑出来这个模型？接口返回没有就不要
//	  搞出来，懂吗？」
//
// 实测确认（他自己的 5 个国际版账号，`truth_source: dynamic`）：
//
//	/v1/models          hy4-preview → channels: [workbuddy/cn, workbuddy/intl]
//	/v1/models/regions  国际版真值 22 个 **不含 hy4-preview**（含 hy4-preview-f）
//
// 根因：`mergedModelList` 把两张静态表的模型**无条件**加进 order 与 channels，
// 哪怕动态真值早就拿到了。于是静态表从"回退"变成了"宣称"——
// 清单里出现一个用户实际调不到的模型。
//
// # 这组测试钉住的三件事
//
//	① 有真值时，真值里没有的模型**不得**出现（不得编造）
//	② 有真值时，真值里有的模型**必须**出现（别修过头，把好的也滤掉）
//	③ 真值取不到时，静态表**照旧兜底**（回退语义不能被破坏）
//
// ③ 尤其重要：修 ① 最省事的做法是"干脆不用静态表"，那会让
// "上游暂时不可达"时清单变空 —— 比编造一个模型更糟。

import (
	"testing"
	"time"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/upstream"
)

// injectRegionModels 直接往区域模型缓存里塞一份真值（或塞 nil 表示"拉不到"）。
//
// # 为什么不伪造上游 HTTP 响应
//
// `fetchModelsForRegion` 的探测逻辑很绕（按区域挑账号、逐个重试、指数退避），
// 伪造响应要连带伪造账号池与协商过程 —— 那测的是"探测能不能跑通"，
// 而本文件要测的是**拿到真值后怎么组装清单**。直接注入缓存让两者解耦：
// 探测本身由 capability.go 自己的用例覆盖。
func injectRegionModels(region auth.Region, infos []upstream.ModelInfo) {
	regionModelCache.Lock()
	defer regionModelCache.Unlock()
	if infos == nil {
		// nil = 该区域拉不到真值。**注意不能塞空切片** —— 空切片与 nil
		// 在 fetchModelsForRegion 的判据里是同一个意思（都走兜底），
		// 但这里用 nil 更贴合"没取到"的语义。
		//
		// 关键：**不写 byRegion 条目**，让 fetchModelsForRegion 走
		// "没缓存 → 去拉" 的分支；而它拉的时候池里没有账号，
		// 于是返回 nil。这保证"拉不到"是**真实链路**的产物。
		delete(regionModelCache.byRegion, region)
		return
	}
	regionModelCache.byRegion[region] = &regionModels{
		infos:   infos,
		fetched: time.Now(),
	}
}

// modelsE2EHandler 造一个两区真值可控的 handler。
//
// realCN / realIntl 为 nil 表示**该区域拉不到真值**（走静态兜底）。
// 注意 `newTestHandlerForChannels` 的池里有账号，故 nil 时真的会去"拉"
// —— 它拉的是真实上游，测试环境拿不到，于是返回 nil，正好等于"拉不到"。
func modelsE2EHandler(t *testing.T, realCN, realIntl []upstream.ModelInfo) *Handler {
	t.Helper()
	resetModelsCache()

	h := newTestHandlerForChannels(nil)
	injectRegionModels(auth.RegionCN, realCN)
	injectRegionModels(auth.RegionIntl, realIntl)
	return h
}

// modelOrNil 取指定 id 的条目；不存在返回 nil（不 fatal）。
//
// 为什么不复用 findModel：它在缺失时 `t.Fatalf`，而本文件多处要断言
// **不该存在** —— 那是正常路径，不该让测试中止。
func modelOrNil(data []map[string]any, id string) map[string]any {
	for _, m := range data {
		if m["id"] == id {
			return m
		}
	}
	return nil
}

// regionOfChannels 收集某模型所有渠道的 region 集合。
func regionOfChannels(m map[string]any) map[string]bool {
	out := map[string]bool{}
	raw, _ := m["channels"].([]any)
	for _, c := range raw {
		cm, ok := c.(map[string]any)
		if !ok {
			continue
		}
		if r, _ := cm["region"].(string); r != "" {
			out[r] = true
		}
	}
	return out
}

// TestStaticDoesNotFabricateModelPresentInTruth 真值里没有的模型不得标成该区可用。
//
// 这是所有者报的那个缺陷的直接回归测试：
// `hy4-preview` 只在国服真值里，却因躺在国际版静态表里被标成"国际版可用"。
func TestStaticDoesNotFabricateModelPresentInTruth(t *testing.T) {
	// 国服真值有 hy4-preview；国际版真值**没有**它。
	cn := []upstream.ModelInfo{{ID: "hy4-preview"}, {ID: "glm-5.3"}}
	intl := []upstream.ModelInfo{{ID: "hy4-preview-f"}, {ID: "glm-5.3"}}
	h := modelsE2EHandler(t, cn, intl)

	data := listModelsRaw(t, h)
	m := modelOrNil(data, "hy4-preview")
	if m == nil {
		t.Fatal("hy4-preview 在国服真值里，应出现在 /v1/models")
	}
	regions := regionOfChannels(m)

	// ① 国服渠道必须在（真值里有）。
	if !regions["cn"] {
		t.Fatalf("hy4-preview 应有 cn 渠道（国服真值里有），实际 %+v", regions)
	}
	// ② 国际版渠道**必须不在** —— 这是本次修复的核心断言。
	if regions["intl"] {
		t.Errorf("hy4-preview 不得标为国际版可用（国际版真值里没有它）—— "+
			"用户按这个清单去用会失败。regions=%+v", regions)
	}
}

// TestStaticKeepsModelsThatTruthHas 真值里有的模型不能被误删。
//
// 与上一条成对：只断言"不该有的没有"会让"把整张表滤掉"也算通过。
func TestStaticKeepsModelsThatTruthHas(t *testing.T) {
	cn := []upstream.ModelInfo{{ID: "hy4-preview"}, {ID: "glm-5.3"}, {ID: "kimi-k3-1"}}
	intl := []upstream.ModelInfo{{ID: "hy4-preview-f"}, {ID: "glm-5.3"}}
	h := modelsE2EHandler(t, cn, intl)

	data := listModelsRaw(t, h)
	// 两区真值的**并集**都必须出现。
	for _, id := range []string{"hy4-preview", "hy4-preview-f", "glm-5.3", "kimi-k3-1"} {
		if modelOrNil(data, id) == nil {
			t.Errorf("%s 在真值里存在，必须出现在 /v1/models", id)
		}
	}
	// glm-5.3 两区都有 → 必须两条渠道（分区域各一条）。
	g := modelOrNil(data, "glm-5.3")
	if g == nil {
		t.Fatal("glm-5.3 应存在")
	}
	got := regionOfChannels(g)
	if !got["cn"] || !got["intl"] {
		t.Errorf("glm-5.3 两区真值都有，应同时有 cn 与 intl 渠道，实际 %+v", got)
	}
}

// TestStaticStillFallsBackWhenTruthUnavailable 真值取不到时静态表仍兜底。
//
// # 为什么这条必须存在
//
// 修"编造"最省事的做法是干脆不用静态表。但静态表的**存在意义**就是
// "上游暂时不可达时不至于清单全空" —— 去掉它会让用户在弱网下看到
// 一个空模型列表，那比多显示一个模型更糟。
func TestStaticStillFallsBackWhenTruthUnavailable(t *testing.T) {
	// 国际版真值拉不到（nil）→ 应退回 staticModelsIntl。
	h := modelsE2EHandler(t, []upstream.ModelInfo{{ID: "glm-5.3"}}, nil)

	data := listModelsRaw(t, h)
	// hy4-preview-f 只在国际版静态表里（真值拉不到），应靠静态表出现。
	m := modelOrNil(data, "hy4-preview-f")
	if m == nil {
		t.Fatal("国际版真值拉不到时，静态表必须兜底 —— " +
			"hy4-preview-f 应出现（否则弱网下清单会残缺）")
	}
	if !regionOfChannels(m)["intl"] {
		t.Errorf("静态兜底的条目应标 intl 渠道，实际 %+v", regionOfChannels(m))
	}
}

// TestStaticFallbackDoesNotLeakAcrossRegions 一区拉不到、另一区拉到时的隔离。
//
// 修 ① 时若把"是否有真值"写成全局布尔（而不是按区域），会出现：
// 国服拉到了就以为国际版也拉到了 → 国际版连兜底都没了。
func TestStaticFallbackDoesNotLeakAcrossRegions(t *testing.T) {
	// 国服有真值（且其中没有 kimi-k2.7）、国际版没有。
	cn := []upstream.ModelInfo{{ID: "glm-5.3"}}
	h := modelsE2EHandler(t, cn, nil)

	data := listModelsRaw(t, h)
	// 国服有真值时不参与宣称 → kimi-k2.7（只在国服静态表里）不该有 cn 渠道。
	if m := modelOrNil(data, "kimi-k2.7"); m != nil {
		if regionOfChannels(m)["cn"] {
			t.Errorf("国服有真值且其中没有 kimi-k2.7，不该再由静态表宣称它有 cn 渠道。regions=%+v",
				regionOfChannels(m))
		}
	}
	// 国际版真值没拉到 → 静态表兜底仍生效（证明隔离是按区域的）。
	if modelOrNil(data, "hy4-preview-f") == nil {
		t.Error("国际版真值未拉到，静态表应兜底提供 hy4-preview-f")
	}
}

// TestIntlStaticTableHasNoHy4Preview 国际版静态表本身不得含 hy4-preview。
//
// 这条守的是**数据**而不是逻辑：即使组装逻辑将来被改回"无条件合并静态表"，
// 只要表里没有它，就不会编造。两道防线都留着。
func TestIntlStaticTableHasNoHy4Preview(t *testing.T) {
	for _, id := range staticIntlIDs() {
		if id == "hy4-preview" {
			t.Fatal("hy4-preview 不在国际版真值里（实测 2026-09-21），" +
				"不得出现在 staticModelsIntl —— 它会让 /v1/models 编造国际版可用")
		}
	}
	// 国服表**应保留**它（国服真值确实有）。
	found := false
	for _, m := range staticModels {
		if id, _ := m["id"].(string); id == "hy4-preview" {
			found = true
		}
	}
	if !found {
		t.Error("hy4-preview 在国服真值里，国服静态表应保留它")
	}
}
