package upstream

import (
	"net/http"
	"strings"
	"testing"

	"workbuddy2api/internal/auth"
)

// ---------------------------------------------------------------------------
// 回归：模型清单必须来自 /v3/config 的 agents[cli].models，且必须用 WorkBuddy UA
//
// 实测缺陷（2026-09-15）：
//
//	1. 端点用错：/console/enterprises/personal/models 在国际版恒返回 500
//	   （openresty 错误页，5/5 账号复现，与认证方式/请求头无关），
//	   导致国际版永远只能靠硬编码静态表，静态表一过时就与真实可用集脱节。
//
//	2. UA 用错：该接口按 UA 返回**不同产品**的清单。
//	   WorkBuddy/... → WorkBuddy 产品清单；CLI/...CodeBuddy/... → CodeBuddy 清单。
//	   两者差异很大（国际版 20 vs 17），且 HTTP 都是 200，
//	   属于「静默返回另一套数据」，最需要测试锁住。
//
//	3. 取错字段：data.models 是产品全部模型池（含图片/视频生成、lite 辅助模型），
//	   真正可用的子集是 data.agents[name=="cli"].models。
// ---------------------------------------------------------------------------

// v3ConfigResponse 贴近真实的 /v3/config 响应。
//
// 刻意让 data.models 混入「不可用于对话」的条目（图片生成、视频生成、lite），
// 用来验证它们不出现在结果里 —— 这正是取 agents[cli].models 而非 data.models 的意义。
const v3ConfigResponse = `{
  "code": 0,
  "msg": "OK",
  "data": {
    "agents": [
      {"name": "cli", "models": ["default-model", "gpt-6-astra", "glm-5.3", "kimi-k2.8-preview", "brand-new-model"]},
      {"name": "compact", "models": []},
      {"name": "summaryGenerator", "models": ["default-model-lite"]}
    ],
    "models": [
      {"id": "default-model", "name": "Auto", "maxInputTokens": 176000, "maxOutputTokens": 24000},
      {"id": "gpt-6-astra", "name": "GPT-6-Astra", "maxInputTokens": 1000000, "maxOutputTokens": 128000,
       "reasoning": {"supportedEfforts": ["low", "medium", "high", "xhigh", "max"]}},
      {"id": "glm-5.3", "name": "GLM-5.3", "maxInputTokens": 1000000, "maxOutputTokens": 48000,
       "reasoning": {"supportedEfforts": ["low", "high", "max"]}},
      {"id": "kimi-k2.8-preview", "name": "Kimi-K2.8-Preview", "maxInputTokens": 1000000, "maxOutputTokens": 32000},
      {"id": "default-model-lite", "name": "Auto Lite", "maxInputTokens": 176000, "maxOutputTokens": 24000},
      {"id": "gemini-3.0-pro-image", "name": "Image", "maxInputTokens": 1, "maxOutputTokens": 1},
      {"id": "hunyuan-video-art", "name": "Video", "maxInputTokens": 1, "maxOutputTokens": 1}
    ]
  }
}`

// fetchModelsCapturing 用假 Transport 跑一次 FetchModels，返回结果与捕获到的请求。
func fetchModelsCapturing(t *testing.T, body string, status int) ([]ModelInfo, *http.Request, *Client, *auth.Auth) {
	t.Helper()
	var captured *http.Request
	c := testClient(rtFunc(func(r *http.Request) (*http.Response, error) {
		captured = r
		return jsonResp(status, body), nil
	}))
	c.BaseIntl = "https://intl.example"
	a := &auth.Auth{UID: "u1", AccessToken: "tok", Domain: "www.workbuddy.ai"}
	got, err := c.FetchModels(a)
	if err != nil && status == 200 {
		t.Fatalf("FetchModels 失败: %v", err)
	}
	return got, captured, c, a
}

// idsOf 提取模型 id 列表。
func idsOf(ms []ModelInfo) []string {
	out := make([]string, 0, len(ms))
	for _, m := range ms {
		out = append(out, m.ID)
	}
	return out
}

// has 判断切片是否含某元素。
func has(xs []string, want string) bool {
	for _, x := range xs {
		if x == want {
			return true
		}
	}
	return false
}

// TestFetchModelsUsesV3ConfigPath 端点必须是 /v3/config。
//
// 旧端点 /console/enterprises/personal/models 在国际版恒 500，
// 是「国际版只能靠静态表」的根因。
func TestFetchModelsUsesV3ConfigPath(t *testing.T) {
	_, req, _, _ := fetchModelsCapturing(t, v3ConfigResponse, 200)
	if req == nil {
		t.Fatal("未捕获到请求")
	}
	if req.URL.Path != "/v3/config" {
		t.Fatalf("应请求 /v3/config（旧端点在国际版恒 500），实际 %q", req.URL.Path)
	}
}

// TestFetchModelsSendsWorkBuddyUA UA 必须是 WorkBuddy 前缀。
//
// 该接口按 UA 返回不同产品的清单；用 CodeBuddy 的 UA 会拿到另一套模型，
// 且 HTTP 仍是 200 —— 静默错误，必须有测试锁住。
func TestFetchModelsSendsWorkBuddyUA(t *testing.T) {
	_, req, _, _ := fetchModelsCapturing(t, v3ConfigResponse, 200)
	ua := req.Header.Get("User-Agent")
	if !strings.HasPrefix(ua, "WorkBuddy/") {
		t.Fatalf("UA 必须以 WorkBuddy/ 开头（否则拿到 CodeBuddy 产品清单），实际 %q", ua)
	}
	if ua == clientUA {
		t.Fatalf("模型接口不能复用全局 clientUA（%q）—— 那会返回 CodeBuddy 的模型清单", clientUA)
	}
}

// TestFetchModelsUsesCliAgentList 只返回 agents[cli].models 里的模型。
func TestFetchModelsUsesCliAgentList(t *testing.T) {
	got, _, _, _ := fetchModelsCapturing(t, v3ConfigResponse, 200)
	ids := idsOf(got)

	for _, want := range []string{"default-model", "gpt-6-astra", "glm-5.3", "kimi-k2.8-preview", "brand-new-model"} {
		if !has(ids, want) {
			t.Errorf("缺少 cli 清单里的模型 %q，实际 %v", want, ids)
		}
	}
	for _, bad := range []string{"gemini-3.0-pro-image", "hunyuan-video-art", "default-model-lite"} {
		if has(ids, bad) {
			t.Errorf("%q 不在 agents[cli].models 里（图片/视频/lite 模型），不应返回；实际 %v", bad, ids)
		}
	}
	if len(got) != 5 {
		t.Errorf("应恰好返回 cli 清单的 5 个模型，实际 %d 个: %v", len(got), ids)
	}
}

// TestFetchModelsEnrichesMetadata 元数据从 data.models 按 id 关联补齐。
func TestFetchModelsEnrichesMetadata(t *testing.T) {
	got, _, _, _ := fetchModelsCapturing(t, v3ConfigResponse, 200)
	byID := map[string]ModelInfo{}
	for _, m := range got {
		byID[m.ID] = m
	}

	astra, ok := byID["gpt-6-astra"]
	if !ok {
		t.Fatal("应包含 gpt-6-astra")
	}
	if astra.ContextWindow != 1000000 {
		t.Errorf("gpt-6-astra ContextWindow 应为 1000000（maxInputTokens），实际 %d", astra.ContextWindow)
	}
	if astra.MaxTokens != 128000 {
		t.Errorf("gpt-6-astra MaxTokens 应为 128000（maxOutputTokens），实际 %d", astra.MaxTokens)
	}
	if len(astra.Efforts) != 5 {
		t.Errorf("gpt-6-astra 应带 5 个 supportedEfforts，实际 %v", astra.Efforts)
	}

	// cli 清单里有、models 池里没有的：仍返回，元数据留空
	brandNew, ok := byID["brand-new-model"]
	if !ok {
		t.Fatal("cli 清单里的新模型即使 models 池没有也要返回（它确实可用）")
	}
	if brandNew.ContextWindow != 0 {
		t.Errorf("元数据未知时应为 0，实际 %d", brandNew.ContextWindow)
	}
}

// TestFetchModelsCachesEfforts supportedEfforts 要写入 effort 缓存（供请求体降级）。
func TestFetchModelsCachesEfforts(t *testing.T) {
	_, _, c, _ := fetchModelsCapturing(t, v3ConfigResponse, 200)
	snap := c.effortsSnapshot()

	got, ok := snap["glm-5.3"]
	if !ok {
		t.Fatalf("glm-5.3 的 efforts 应入缓存，实际缓存键: %v", snap)
	}
	if len(got) != 3 {
		t.Errorf("glm-5.3 应有 3 个 efforts，实际 %v", got)
	}
	if _, bad := snap["default-model"]; bad {
		t.Error("没有 supportedEfforts 的模型不应写入 effort 缓存")
	}
}

// TestFetchModelsFallsBackToFullPool 上游没给 cli agent 时退回全量池。
//
// 宁可多不可少：退回旧行为总比返回空列表导致客户端看不到任何模型好。
func TestFetchModelsFallsBackToFullPool(t *testing.T) {
	const noAgents = `{"code":0,"data":{"models":[
		{"id":"m1","maxInputTokens":100,"maxOutputTokens":10},
		{"id":"m2","maxInputTokens":200,"maxOutputTokens":20}
	]}}`
	got, _, _, _ := fetchModelsCapturing(t, noAgents, 200)
	if len(got) != 2 {
		t.Fatalf("无 cli agent 时应退回全量池（2 个），实际 %d 个: %v", len(got), idsOf(got))
	}
}

// TestFetchModelsRejectsNonZeroCode 业务码非 0 要报错。
func TestFetchModelsRejectsNonZeroCode(t *testing.T) {
	c := testClient(rtFunc(func(r *http.Request) (*http.Response, error) {
		return jsonResp(200, `{"code":401,"msg":"unauthorized","data":{}}`), nil
	}))
	a := &auth.Auth{UID: "u1", AccessToken: "tok", Domain: "www.workbuddy.ai"}
	if _, err := c.FetchModels(a); err == nil {
		t.Fatal("code!=0 时应返回错误")
	}
}

// TestFetchModelsRejectsEmptyList 空列表要报错（让上层回退静态表）。
func TestFetchModelsRejectsEmptyList(t *testing.T) {
	c := testClient(rtFunc(func(r *http.Request) (*http.Response, error) {
		return jsonResp(200, `{"code":0,"data":{"models":[],"agents":[]}}`), nil
	}))
	a := &auth.Auth{UID: "u1", AccessToken: "tok", Domain: "www.workbuddy.ai"}
	if _, err := c.FetchModels(a); err == nil {
		t.Fatal("空列表时应返回错误，以便上层回退静态表")
	}
}

// TestFetchModelsReportsHTTPError 非 200 要带状态码报错。
func TestFetchModelsReportsHTTPError(t *testing.T) {
	c := testClient(rtFunc(func(r *http.Request) (*http.Response, error) {
		return jsonResp(500, `<html>500 Internal Server Error</html>`), nil
	}))
	a := &auth.Auth{UID: "u1", AccessToken: "tok", Domain: "www.workbuddy.ai"}
	_, err := c.FetchModels(a)
	if err == nil {
		t.Fatal("HTTP 500 时应返回错误")
	}
	if !strings.Contains(err.Error(), "500") {
		t.Errorf("错误信息应含状态码，实际: %v", err)
	}
}

// TestFetchModelsUsesIntlBaseForIntlAccount 国际版账号要走国际版基址。
func TestFetchModelsUsesIntlBaseForIntlAccount(t *testing.T) {
	_, req, _, _ := fetchModelsCapturing(t, v3ConfigResponse, 200)
	if req.URL.Host != "intl.example" {
		t.Fatalf("国际版账号应请求 BaseIntl，实际 host=%q", req.URL.Host)
	}
}
