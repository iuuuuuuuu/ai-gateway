package server

import (
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/upstream"
)

// ---------------------------------------------------------------------------
// 回归：按区域的**计费倍率**真值必须能取到（供宿主判断免费模型）
//
// 缺陷背景（所有者原话）：「兼容网关，消耗积分与实际不符。国际版账号的
// 4.1flash 模型，是免费的，但是我看到后面居然统计的也有积分」。
//
// 根因是链路断在**解析**这一步：上游 /v3/config 的每个模型条目本就带
// `credits` 计费倍率（实测国际版 22/22 个模型都有），但 FetchModels 用的是
// **手写匿名结构体**，没写进去的字段被 json 包静默丢弃 —— 读代码完全看不出
// 上游给过它，于是宿主只能靠硬编码/猜，最终把免费模型也算成了消耗。
//
// 本组测试锁住两件事：
//  1. 倍率从上游一路带到 regionCapability（按区域分桶，不合并）；
//  2. /v1/models/regions 把它透出（含 null 三态与上游原文），
//     宿主据此判定「免费」，不自己硬编码清单。
//
// 为什么倍率必须**按区域**：同名模型两区计费可以相反。实测
// deepseek-v4.1-flash 国服 "x0.03"（计费）/ 国际版 "x0.00"（免费）——
// 合并成一份全局清单必然在其中一侧判错。
// ---------------------------------------------------------------------------

// regionBillingUpstream 造一个按**区域**返回不同 /v3/config 的假上游。
//
// 复用 newFakeUpstream 的 roundTripFunc 形态，但按 host 分流：区域真值本来就
// 是「抽一个该区域账号去问」得到的，按 host 分流正对应这个语义。
// 两个账号的 access token 不同，故 host 是唯一可靠的区分依据。
func regionBillingUpstream(t *testing.T, cnBody, intlBody string) *upstream.Client {
	t.Helper()
	up := &upstream.Client{
		HTTP: &http.Client{Transport: roundTripFunc(func(r *http.Request) (*http.Response, error) {
			body := cnBody
			if strings.Contains(r.URL.Host, "intl") {
				body = intlBody
			}
			return &http.Response{
				StatusCode: http.StatusOK,
				Header:     http.Header{"Content-Type": []string{"application/json"}},
				Body:       io.NopCloser(strings.NewReader(body)),
			}, nil
		})},
		ChatBaseCN:    "https://cn.example",
		BillingBaseCN: "https://cn.example",
		BaseIntl:      "https://intl.example",
	}
	return up
}

// billingHandler 造一个「国服 + 国际版各一个账号」的 handler。
func billingHandler(t *testing.T, cnBody, intlBody string) *Handler {
	t.Helper()
	resetModelsCache()
	p := testPoolWith(
		&auth.Auth{UID: "cn-1", AccessToken: "tcn", Domain: "copilot.tencent.com", ExpiresAt: 9999999999, SoonestExpireAt: 1 << 40},
		&auth.Auth{UID: "intl-1", AccessToken: "tintl", Domain: "www.workbuddy.ai", ExpiresAt: 9999999999, SoonestExpireAt: 1 << 40},
	)
	return NewHandler(Config{Pool: p, Upstream: regionBillingUpstream(t, cnBody, intlBody)})
}

// fetchRegionViews 调一次 /v1/models/regions，返回 region 名 → 该区域的模型条目。
func fetchRegionViews(t *testing.T, h *Handler) map[string]map[string]map[string]any {
	t.Helper()
	req := httptest.NewRequest(http.MethodGet, "/v1/models/regions", nil)
	rec := httptest.NewRecorder()
	h.mux.ServeHTTP(rec, req)
	if rec.Code != http.StatusOK {
		t.Fatalf("/v1/models/regions 应返回 200，实际 %d（%s）", rec.Code, rec.Body.String())
	}
	var payload struct {
		Regions []struct {
			Region string           `json:"region"`
			Models []map[string]any `json:"models"`
		} `json:"regions"`
	}
	if err := json.Unmarshal(rec.Body.Bytes(), &payload); err != nil {
		t.Fatalf("解析响应失败: %v（原文 %s）", err, rec.Body.String())
	}
	out := map[string]map[string]map[string]any{}
	for _, r := range payload.Regions {
		byID := map[string]map[string]any{}
		for _, m := range r.Models {
			if id, _ := m["id"].(string); id != "" {
				byID[id] = m
			}
		}
		out[r.Region] = byID
	}
	return out
}

// 两区真实形态：同名模型两区倍率相反，且各有一种「未声明」。
const (
	billingCNBody = `{"code":0,"data":{
	  "agents":[{"name":"cli","models":["deepseek-v4.1-flash","hy3","cn-auto"]}],
	  "models":[
	    {"id":"deepseek-v4.1-flash","maxInputTokens":1000000,"maxOutputTokens":10,"credits":"x0.03"},
	    {"id":"hy3","maxInputTokens":192000,"maxOutputTokens":10,"credits":"x0.00"},
	    {"id":"cn-auto","maxInputTokens":256000,"maxOutputTokens":10}
	  ]}}`
	billingIntlBody = `{"code":0,"data":{
	  "agents":[{"name":"cli","models":["deepseek-v4.1-flash","hy3","intl-default"]}],
	  "models":[
	    {"id":"deepseek-v4.1-flash","maxInputTokens":300000,"maxOutputTokens":10,"credits":"x0.00"},
	    {"id":"hy3","maxInputTokens":192000,"maxOutputTokens":10,"credits":"x0.00"},
	    {"id":"intl-default","maxInputTokens":200000,"maxOutputTokens":10,"credits":""}
	  ]}}`
)

// TestRegionsExposeCreditMultiplierPerRegion 倍率必须**按区域**分别给出，
// 且同名模型在两区得到相反结论。
//
// 这是「判定必须同时看模型与区域」的端到端依据：若这里退化成一张全局表，
// deepseek-v4.1-flash 必然在其中一侧判错 —— 国服被显示成免费（少算消耗），
// 或国际版被显示成计费（多算消耗，正是所有者报的现象）。
func TestRegionsExposeCreditMultiplierPerRegion(t *testing.T) {
	h := billingHandler(t, billingCNBody, billingIntlBody)
	views := fetchRegionViews(t, h)

	cnFlash := multiplierOf(t, views, "cn", "deepseek-v4.1-flash")
	intlFlash := multiplierOf(t, views, "intl", "deepseek-v4.1-flash")

	if cnFlash == nil || *cnFlash != 0.03 {
		t.Fatalf("国服 deepseek-v4.1-flash 应为计费 0.03，实际 %v", ptrText(cnFlash))
	}
	if intlFlash == nil || *intlFlash != 0 {
		t.Fatalf("国际版 deepseek-v4.1-flash 应为免费 0，实际 %v —— 这正是所有者报的那个模型", ptrText(intlFlash))
	}
	if *cnFlash == *intlFlash {
		t.Fatal("同名模型在两区必须得到不同倍率 —— 判定不能只看模型 ID")
	}

	// hy3 两区都免费：这条证明「不是所有模型都靠区域区分」，避免实现走偏成
	// 「凡国际版就免费」。国际版也确有计费模型（见下面 fast-model 的对照）。
	for _, region := range []string{"cn", "intl"} {
		if v := multiplierOf(t, views, region, "hy3"); v == nil || *v != 0 {
			t.Errorf("%s hy3 应为免费 0，实际 %v", region, ptrText(v))
		}
	}
}

// TestRegionsCreditMultiplierKeepsThreeStates 「未声明」必须是 null，**不是 0**。
//
// 上游只给部分模型写 credits（实测国服 52 个里只有 33 个）。
// 把 null 读成 0 会把本该计费的模型当成免费 —— 与所有者报的 bug 反方向、
// 但同样属于「统计与实际不符」，因此三态必须原样透出。
func TestRegionsCreditMultiplierKeepsThreeStates(t *testing.T) {
	h := billingHandler(t, billingCNBody, billingIntlBody)
	views := fetchRegionViews(t, h)

	cnAuto, ok := views["cn"]["cn-auto"]
	if !ok {
		t.Fatal("国服应列出 cn-auto")
	}
	// 字段缺失 → JSON null（解码成 nil）。不能用「键不存在」表达：
	// 消费方需要能区分「这个响应没这个字段」与「上游未声明」。
	raw, present := cnAuto["credit_multiplier"]
	if !present {
		t.Fatal("credit_multiplier 键必须总是存在（未声明时值为 null），否则消费方无法与旧版响应区分")
	}
	if raw != nil {
		t.Fatalf("cn-auto 上游未声明倍率，应为 null，实际 %v（把未声明当 0 会把计费模型算成免费）", raw)
	}

	// 空串是**有字段且明确免费**，与缺失必须不同结论。
	intlDefault := multiplierOf(t, views, "intl", "intl-default")
	if intlDefault == nil || *intlDefault != 0 {
		t.Fatalf("intl-default（credits=\"\"）应为免费 0，实际 %v", ptrText(intlDefault))
	}

	// 上游原文一并透出，便于人工核对解析是否正确。
	if got, _ := views["intl"]["intl-default"]["credits_raw"].(string); got != "" {
		t.Errorf("intl-default 原文应为空串，实际 %q", got)
	}
	if got, _ := views["cn"]["deepseek-v4.1-flash"]["credits_raw"].(string); got != "x0.03" {
		t.Errorf("国服 ds4.1 原文应为 x0.03，实际 %q", got)
	}
}

// TestRegionsExposeMultiplierWithUnitSuffix 带单位后缀的倍率必须解析出数字。
//
// 实测国际版的默认档模型写作 "x0.34 credits"。整串 ParseFloat 会失败，
// 而失败又会被上层当成「未声明」→ 该计费的显示成未知。
func TestRegionsExposeMultiplierWithUnitSuffix(t *testing.T) {
	body := `{"code":0,"data":{
	  "agents":[{"name":"cli","models":["fast-model"]}],
	  "models":[{"id":"fast-model","maxInputTokens":100,"maxOutputTokens":10,"credits":"x0.34 credits"}]}}`
	// 两区用同一份响应即可：本用例只关心「后缀能不能解析」，与区域无关。
	h := billingHandler(t, body, body)
	v := multiplierOf(t, fetchRegionViews(t, h), "intl", "fast-model")
	if v == nil || *v != 0.34 {
		t.Fatalf("fast-model 应解析出 0.34，实际 %v（带 credits 后缀必须能解析）", ptrText(v))
	}
}

// multiplierOf 从 regions 视图里取某区域某模型的倍率。
func multiplierOf(t *testing.T, views map[string]map[string]map[string]any, region, id string) *float64 {
	t.Helper()
	byID, ok := views[region]
	if !ok {
		t.Fatalf("响应里没有区域 %s", region)
	}
	entry, ok := byID[id]
	if !ok {
		t.Fatalf("区域 %s 的清单里没有 %s", region, id)
	}
	raw, present := entry["credit_multiplier"]
	if !present {
		t.Fatalf("%s/%s 缺 credit_multiplier 键", region, id)
	}
	if raw == nil {
		return nil
	}
	v, ok := raw.(float64)
	if !ok {
		t.Fatalf("%s/%s 的 credit_multiplier 不是数字: %T", region, id, raw)
	}
	return &v
}

// ptrText 把可选的倍率渲染成人能读的字符串（nil 显示成 null，与 JSON 一致）。
func ptrText(v *float64) string {
	if v == nil {
		return "null"
	}
	b, _ := json.Marshal(*v)
	return string(b)
}
