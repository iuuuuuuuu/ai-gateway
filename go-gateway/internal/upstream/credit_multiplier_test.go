package upstream

import (
	"encoding/json"
	"net/http"
	"testing"
)

// ---------------------------------------------------------------------------
// 免费模型判定：上游 /v3/config 的 `credits` 计费倍率
//
// 缺陷背景（所有者原话）：「兼容网关，消耗积分与实际不符。国际版账号的
// 4.1flash 模型，是免费的，但是我看到后面居然统计的也有积分」。
//
// 实测（2026-09-18 直接拉两区 /v3/config 的真实响应）：
//
//	国际版 22 个模型**全部**带 credits 字段，倍率 0 的有 4 个：
//	    default-model ""（空串）      deepseek-v4.1-flash "x0.00"
//	    hy4-preview-f "x0.00"          hy3                "x0.00"
//	  计费样例：fast-model "x0.34 credits"、primary-model "x3.31 credits"
//
//	国服 52 个模型里**只有 33 个**带该字段，倍率 0 的只有 1 个（hy3 "x0.00"）；
//	  其余 19 个（auto / deepseek-r1-0528-* / codewise-* 等）**整条缺失**。
//
//	同名模型两区结论相反：deepseek-v4.1-flash 国服 "x0.03" / 国际版 "x0.00"。
//
// 这些测试锁住的就是上面这些事实，重点是两种「错法」都不能犯：
//   把「字段缺失」当免费  → 该计费的显示成免费
//   把「倍率 0」当未知    → 免费的被显示成计费（所有者报的这个 bug）
// ---------------------------------------------------------------------------

// TestParseCreditMultiplierRealForms 上游实测出现过的 4 种形态都必须吃下。
func TestParseCreditMultiplierRealForms(t *testing.T) {
	cases := []struct {
		name     string
		raw      string
		want     float64
		declared bool
	}{
		// 国际版 default-model：**有字段**且是空串 → 免费。
		// 注意与「字段缺失」区分：那是另一个用例（TestCreditMultiplierMissingFieldIsUnknown）。
		{"空串=免费", "", 0, true},
		{"空串带空白=免费", "   ", 0, true},
		// 国际版 ds4.1-flash / hy4-preview-f / hy3，国服 hy3。
		{"零倍率=免费", "x0.00", 0, true},
		{"零倍率无小数点", "x0", 0, true},
		// 纯倍率（国际版 kimi-k2.8-preview / deepseek-v4.1-flash-sg）。
		{"纯倍率", "x0.34", 0.34, true},
		{"国服倍率", "x0.03", 0.03, true},
		// 带单位后缀（国际版 fast-model / primary-model / deep-model 等）。
		// 这是最容易写错的一种：整串 ParseFloat 会失败，失败又会被当成「未声明」。
		{"带credits后缀", "x0.34 credits", 0.34, true},
		{"带后缀大倍率", "x3.31 credits", 3.31, true},
		{"无x前缀带后缀", "0.34 credits", 0.34, true},
		{"前后空白", "  x0.79  ", 0.79, true},
		// 解析不出数字 → 未声明，**不猜**。
		{"纯文字", "free", 0, false},
		{"占位符", "N/A", 0, false},
		{"只有x", "x", 0, false},
		{"负倍率语义已变", "x-1.5", 0, false},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			got, declared := parseCreditMultiplier(c.raw)
			if declared != c.declared {
				t.Fatalf("parseCreditMultiplier(%q) declared=%v，期望 %v", c.raw, declared, c.declared)
			}
			if declared && got != c.want {
				t.Fatalf("parseCreditMultiplier(%q) = %v，期望 %v", c.raw, got, c.want)
			}
		})
	}
}

// TestCreditMultiplierMissingFieldIsUnknown 字段缺失 ⇒ nil（未声明），**不是免费**。
//
// 这是本修复里最容易写反的一条：国服 19 个模型整条没有 credits 字段，
// 若解析成「免费」，就会把本该计费的模型统计成 0 积分 —— 与所有者报的 bug
// 反方向、但同样属于「统计与实际不符」。
func TestCreditMultiplierMissingFieldIsUnknown(t *testing.T) {
	// 显式传 nil（= json 里压根没有这个键）
	if got := creditMultiplierOf(nil); got != nil {
		t.Fatalf("字段缺失应得到 nil（未声明），实际 %v —— 「未声明 ≠ 免费」", *got)
	}
	// 空串是**有字段**、值明确为免费：两者必须是不同结论。
	blank := ""
	if got := creditMultiplierOf(&blank); got == nil {
		t.Fatal("空串应是明确的免费（0），不能与字段缺失混为一谈")
	} else if *got != 0 {
		t.Fatalf("空串倍率应为 0，实际 %v", *got)
	}
}

// TestParseCreditMultiplierJSONMissingVsBlank 从**真实 JSON 形状**上锁住两种形态的区分。
//
// 上一轮用例是直接调函数；这里走一遍 json.Unmarshal，验证 `*string` 这个选择
// 确实把「键不存在」与「键存在但为空串」分开了 —— 用非指针 string 时这两者
// 都会变成 ""，那是本用例真正要防的回归。
func TestParseCreditMultiplierJSONMissingVsBlank(t *testing.T) {
	var env struct {
		Models []struct {
			ID      string  `json:"id"`
			Credits *string `json:"credits"`
		} `json:"models"`
	}
	raw := `{"models":[
		{"id":"intl-default-model","credits":""},
		{"id":"cn-auto"},
		{"id":"intl-ds41","credits":"x0.00"},
		{"id":"intl-fast","credits":"x0.34 credits"}
	]}`
	if err := json.Unmarshal([]byte(raw), &env); err != nil {
		t.Fatalf("解析失败: %v", err)
	}
	got := map[string]*float64{}
	for _, m := range env.Models {
		got[m.ID] = creditMultiplierOf(m.Credits)
	}
	assertMultiplier(t, got, "intl-default-model", 0, true, "空串 = 免费")
	assertMultiplier(t, got, "cn-auto", 0, false, "字段缺失 = 未声明")
	assertMultiplier(t, got, "intl-ds41", 0, true, "x0.00 = 免费")
	assertMultiplier(t, got, "intl-fast", 0.34, true, "带后缀倍率")
}

func assertMultiplier(t *testing.T, got map[string]*float64, id string, want float64, declared bool, why string) {
	t.Helper()
	v, ok := got[id]
	if !ok {
		t.Fatalf("%s 不在结果里", id)
	}
	if declared && v == nil {
		t.Fatalf("%s（%s）应为已声明 %v，实际 nil（未声明）", id, why, want)
	}
	if !declared {
		if v != nil {
			t.Fatalf("%s（%s）应为未声明，实际 %v", id, why, *v)
		}
		return
	}
	if *v != want {
		t.Fatalf("%s（%s）应为 %v，实际 %v", id, why, want, *v)
	}
}

// TestFetchModelsCarriesCreditMultiplier FetchModels 必须把倍率带到 ModelInfo 上。
//
// 端到端锁住「手写匿名结构体漏字段」这个根因：若 Credits 没写进结构体，
// json 包会静默丢弃它，本用例立刻失败（而只读代码是看不出来的）。
func TestFetchModelsCarriesCreditMultiplier(t *testing.T) {
	body := `{"code":0,"data":{
	  "agents":[{"name":"cli","models":["free-model","paid-model","unknown-model","blank-model"]}],
	  "models":[
	    {"id":"free-model","maxInputTokens":100,"maxOutputTokens":10,"credits":"x0.00"},
	    {"id":"paid-model","maxInputTokens":100,"maxOutputTokens":10,"credits":"x0.34 credits"},
	    {"id":"unknown-model","maxInputTokens":100,"maxOutputTokens":10},
	    {"id":"blank-model","maxInputTokens":100,"maxOutputTokens":10,"credits":""}
	  ]}}`
	infos, _, _, _ := fetchModelsCapturing(t, body, http.StatusOK)
	byID := map[string]ModelInfo{}
	for _, mi := range infos {
		byID[mi.ID] = mi
	}
	if len(byID) != 4 {
		t.Fatalf("应返回 4 个模型，实际 %d：%v", len(byID), byID)
	}
	free := byID["free-model"].CreditMultiplier
	if free == nil || *free != 0 {
		t.Fatalf("free-model 倍率应为 0（免费），实际 %v", free)
	}
	paid := byID["paid-model"].CreditMultiplier
	if paid == nil || *paid != 0.34 {
		t.Fatalf("paid-model 倍率应为 0.34，实际 %v —— 带 credits 后缀必须能解析", paid)
	}
	if got := byID["unknown-model"].CreditMultiplier; got != nil {
		t.Fatalf("unknown-model 未声明倍率，应为 nil，实际 %v", *got)
	}
	blank := byID["blank-model"].CreditMultiplier
	if blank == nil || *blank != 0 {
		t.Fatalf("blank-model（credits=\"\"）应为免费 0，实际 %v —— 空串与缺失必须区分", blank)
	}
}

// TestSameModelNameDiffersByRegion 同名模型在两区的倍率**可以相反**。
//
// 这正是「不能硬编码一张全局免费清单」的实测依据：deepseek-v4.1-flash
// 国际版免费（所有者观察到的那个模型），国服却在计费。两区各自拉取，
// 因此各自得到自己的真值 —— 本用例锁住「同名不同结论」这一事实本身。
func TestSameModelNameDiffersByRegion(t *testing.T) {
	const id = "deepseek-v4.1-flash"
	intlBody := `{"code":0,"data":{
	  "agents":[{"name":"cli","models":["` + id + `"]}],
	  "models":[{"id":"` + id + `","maxInputTokens":300000,"maxOutputTokens":10,"credits":"x0.00"}]}}`
	cnBody := `{"code":0,"data":{
	  "agents":[{"name":"cli","models":["` + id + `"]}],
	  "models":[{"id":"` + id + `","maxInputTokens":1000000,"maxOutputTokens":10,"credits":"x0.03"}]}}`

	intl := fetchOne(t, intlBody, id)
	cn := fetchOne(t, cnBody, id)

	if intl == nil || *intl != 0 {
		t.Fatalf("国际版 %s 应为免费（倍率 0），实际 %v", id, intl)
	}
	if cn == nil || *cn != 0.03 {
		t.Fatalf("国服 %s 应为计费 0.03，实际 %v", id, cn)
	}
	if *intl == *cn {
		t.Fatal("同名模型在两区必须得到不同结论 —— 判定不能只看模型 ID，必须同时看区域")
	}
}

// fetchOne 跑一次 FetchModels，返回指定模型的倍率。
func fetchOne(t *testing.T, body, id string) *float64 {
	t.Helper()
	infos, _, _, _ := fetchModelsCapturing(t, body, http.StatusOK)
	for _, mi := range infos {
		if mi.ID == id {
			return mi.CreditMultiplier
		}
	}
	t.Fatalf("结果里没有 %s", id)
	return nil
}
