package usage

// billing_test.go 钉住「Token 用量按计费归属拆开并带倍率」。
//
// # 为什么需要它（2026-09-21 所有者要求）
//
// 所有者原话：
//
//	「兼容网关的token用量也要显示出这个模型的倍率（如果有多个 则需要拆开显示）」
//
// 要钉住三件事：
//
//	① 同一模型的不同归属（平台/区域）**各记一份**，不合并
//	② 每份带自己的倍率（两区相反时不会串）
//	③ 倍率三态可区分：数字 / null（未声明）/ 键缺席（无此概念）

import (
	"testing"
	"time"
)

func floatPtr(v float64) *float64 { return &v }

// TestRecordBilledSplitsByBilling 同一模型的两个归属各记一份。
//
// 这是"如果有多个 则需要拆开显示"的直接断言：若有人把 billed 的键
// 退回只用模型名，两份会合并成一份、倍率只剩一个，这条会红。
func TestRecordBilledSplitsByBilling(t *testing.T) {
	s := New("")
	at := time.Date(2026, 9, 21, 12, 0, 0, 0, time.Local)

	// 同一个模型名，两个区域，倍率**相反**（实测形态）。
	s.RecordBilled("u1", "deepseek-v4.1-flash", Billing{
		Product: "workbuddy", Region: "cn",
		Multiplier: floatPtr(0.03), HasMultiplier: true,
	}, Counters{Input: 100, Output: 10})
	s.RecordBilled("u2", "deepseek-v4.1-flash", Billing{
		Product: "workbuddy", Region: "intl",
		Multiplier: floatPtr(0), HasMultiplier: true,
	}, Counters{Input: 200, Output: 20})
	_ = at

	snap := s.Snapshot(0)
	mb, ok := snap["modelBilling"].(map[string]any)
	if !ok {
		t.Fatal("modelBilling 缺失 —— 前端拿不到倍率")
	}
	groups, ok := mb["deepseek-v4.1-flash"].([]map[string]any)
	if !ok {
		t.Fatalf("modelBilling[deepseek-v4.1-flash] 类型异常: %#v", mb["deepseek-v4.1-flash"])
	}
	if len(groups) != 2 {
		t.Fatalf("两个归属应各记一份（共 2 条），实际 %d：%+v", len(groups), groups)
	}

	// 逐条核对区域与倍率**配对正确**（不能两行都取到同一个倍率）。
	byRegion := map[string]map[string]any{}
	for _, g := range groups {
		r, _ := g["region"].(string)
		byRegion[r] = g
	}
	cn, okCN := byRegion["cn"]
	intl, okIntl := byRegion["intl"]
	if !okCN || !okIntl {
		t.Fatalf("应各有 cn / intl 一条，实际 %+v", byRegion)
	}
	if v, _ := cn["creditMultiplier"].(float64); v != 0.03 {
		t.Errorf("cn 倍率应为 0.03，实际 %v", cn["creditMultiplier"])
	}
	if v, _ := intl["creditMultiplier"].(float64); v != 0 {
		t.Errorf("intl 倍率应为 0（确定的免费），实际 %v", intl["creditMultiplier"])
	}
	// 用量也要各自独立（不是两份都等于合计）。
	// 注意 total 在 Snapshot 里是 int64（Counters.Value 的原样输出），
	// 只有经过 JSON 往返才会变成 float64。
	if n, ok := cn["total"].(int64); ok {
		if n != 110 {
			t.Errorf("cn 的 total 应是它自己那份（110），实际 %d —— 两份串了", n)
		}
	} else if f, ok := cn["total"].(float64); ok {
		if f != 110 {
			t.Errorf("cn 的 total 应是它自己那份（110），实际 %v —— 两份串了", f)
		}
	} else {
		t.Errorf("cn 的 total 类型异常: %#v", cn["total"])
	}
}

// TestRecordBilledKeepsThreeStates 倍率三态在 JSON 层可区分。
//
// ⚠ 这是本功能最容易犯的错：把 null（未声明）当成 0（免费），
// 会让用户以为不扣积分，而它可能正在烧额度。
func TestRecordBilledKeepsThreeStates(t *testing.T) {
	s := New("")

	// ① 已声明（计费）
	s.RecordBilled("u1", "m-declared", Billing{
		Product: "workbuddy", Region: "cn", Multiplier: floatPtr(0.5), HasMultiplier: true,
	}, Counters{Input: 1})
	// ② 未声明（nil）
	s.RecordBilled("u1", "m-unknown", Billing{
		Product: "workbuddy", Region: "cn", Multiplier: nil, HasMultiplier: true,
	}, Counters{Input: 1})
	// ③ 该平台没有倍率概念
	s.RecordBilled("u1", "m-noconcept", Billing{
		Product: "zcode", Region: "cn", Multiplier: nil, HasMultiplier: false,
	}, Counters{Input: 1})

	snap := s.Snapshot(0)
	mb := snap["modelBilling"].(map[string]any)

	// ① 有值
	g1 := mb["m-declared"].([]map[string]any)[0]
	if v, ok := g1["creditMultiplier"].(float64); !ok || v != 0.5 {
		t.Errorf("① 应为 0.5，实际 %#v", g1["creditMultiplier"])
	}

	// ② 键存在但值为 nil → JSON null
	g2 := mb["m-unknown"].([]map[string]any)[0]
	v2, has2 := g2["creditMultiplier"]
	if !has2 {
		t.Error("② 未声明时键应存在（值为 null），否则界面无法与「无此概念」区分")
	}
	if v2 != nil {
		t.Errorf("② 未声明应为 nil（→ null），实际 %#v", v2)
	}
	if hm, _ := g2["hasMultiplier"].(bool); !hm {
		t.Error("② hasMultiplier 应为 true（该平台有这个概念，只是没取到值）")
	}

	// ③ 键缺席
	g3 := mb["m-noconcept"].([]map[string]any)[0]
	if _, has3 := g3["creditMultiplier"]; has3 {
		t.Error("③ 该平台没有倍率概念时不该有 creditMultiplier 键")
	}
	if hm, _ := g3["hasMultiplier"].(bool); hm {
		t.Error("③ hasMultiplier 应为 false")
	}
}

// TestRecordWithoutBillingStaysOutOfModelBilling 不带归属的记录不进 modelBilling。
//
// 老调用点（Record）没有归属信息。若把它们混进 modelBilling，
// 界面会出现一行**没有来源**的用量（product 为空），比不显示更让人困惑。
// 它们仍应正常进 models/accounts（那两条路径与归属无关）。
func TestRecordWithoutBillingStaysOutOfModelBilling(t *testing.T) {
	s := New("")
	s.Record("u1", "plain-model", Counters{Input: 5, Output: 5})

	snap := s.Snapshot(0)
	if mb, ok := snap["modelBilling"].(map[string]any); ok {
		if _, present := mb["plain-model"]; present {
			t.Error("不带归属的记录不该出现在 modelBilling 里（会产生没有来源的一行）")
		}
	}
	// 但 models 维度必须照常有它。
	models, _ := snap["models"].([]map[string]any)
	found := false
	for _, m := range models {
		if m["key"] == "plain-model" {
			found = true
		}
	}
	if !found {
		t.Error("不带归属的记录仍应出现在 models 维度里")
	}
}

// TestRecordBilledSplitsByMultiplierChange 同平台同区域、倍率变了要分开记。
//
// 上游会调倍率（实测国服倍率变过）。若键里不含倍率，两个倍率的用量会合并，
// "倍率"这一列就只能显示其中一个 —— 与"拆开显示"的要求相悖。
func TestRecordBilledSplitsByMultiplierChange(t *testing.T) {
	s := New("")
	s.RecordBilled("u1", "m", Billing{Product: "workbuddy", Region: "cn", Multiplier: floatPtr(0.03), HasMultiplier: true}, Counters{Input: 1})
	s.RecordBilled("u1", "m", Billing{Product: "workbuddy", Region: "cn", Multiplier: floatPtr(0.05), HasMultiplier: true}, Counters{Input: 1})

	snap := s.Snapshot(0)
	groups := snap["modelBilling"].(map[string]any)["m"].([]map[string]any)
	if len(groups) != 2 {
		t.Fatalf("倍率不同应分开记（共 2 条），实际 %d：%+v", len(groups), groups)
	}
	seen := map[float64]bool{}
	for _, g := range groups {
		if v, ok := g["creditMultiplier"].(float64); ok {
			seen[v] = true
		}
	}
	if !seen[0.03] || !seen[0.05] {
		t.Errorf("两个倍率都应保留，实际 %v", seen)
	}
}
