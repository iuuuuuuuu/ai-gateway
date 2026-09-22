package server

// billing_e2e_test.go 端到端验证「用量按归属拆开并带倍率」的**完整链路**：
//
//	HTTP 请求 → 选号 → 转发（假上游）→ 采集 usage → RecordBilled → /usage 输出
//
// # 为什么不能只测 usage 包
//
// `usage` 包的单元测试验证的是"给定归属，聚合是否正确"。而本功能还依赖
// **server 层把归属填对**（fillBilling 从选中的账号推出平台/区域，
// 再从 capabilityIndex 取倍率）。那一段只有走完整 handler 才测得到 ——
// 而它正是最容易错的地方（区域取错 → 倍率取错 → 用户看到错的数字）。
//
// # 为什么用假上游
//
// 真上游的 usage 时有时无（实测 ZCode 通道经常不返回 usage），
// 依赖它做断言会得到 flaky 测试。假上游稳定返回 usage，
// 让本测试只关注**网关自己的聚合与归属**这一件事。

import (
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/pool"
	"workbuddy2api/internal/upstream"
	"workbuddy2api/internal/usage"
)

// sseWithUsage 一段带 usage 末帧的 OpenAI SSE。
//
// 末帧带 usage 是**流式的唯一计量来源**（前面各帧没有），
// 网关的 chatStatsReader 从这里采集。
const sseWithUsage = `data: {"choices":[{"delta":{"content":"hi"},"index":0}]}

data: {"choices":[{"delta":{},"finish_reason":"stop","index":0}],"usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,"prompt_tokens_details":{"cached_tokens":10}}}

data: [DONE]

`

// configBodyWithMultiplier 造一份 `/v3/config` 响应体，只含一个模型及其倍率。
//
// 形状与 `credit_multiplier_test.go` 的 `billingCNBody` 逐字一致 ——
// 那个体已被既有测试验证过能被解析（credits 是 `x0.03` 这种带 x 前缀的字符串）。
func configBodyWithMultiplier(model, credits string) string {
	return `{"code":0,"data":{
	  "agents":[{"name":"cli","models":["` + model + `"]}],
	  "models":[{"id":"` + model + `","maxInputTokens":1000000,"maxOutputTokens":10,"credits":"` + credits + `"}]
	}}`
}

// nopCloser 把字符串包成 io.ReadCloser（测试里反复用到）。
func nopCloser(s string) io.ReadCloser { return io.NopCloser(strings.NewReader(s)) }

// billingE2EHandler 造一个装配了 usage 统计与假上游的 handler。
//
// 复用既有的 `testPoolWith` 与 `regionBillingUpstream`（credit_multiplier_test.go）——
// 后者已能按区域返回不同的 /v3/config 体（倍率真值就从那里来）。
func billingE2EHandler(t *testing.T, accounts []*auth.Auth) (*Handler, *usage.Stats) {
	t.Helper()
	resetModelsCache()

	p := testPoolWith(accounts...)
	stats := usage.New("")

	// 假上游：配置端点回带 credits 的真值、聊天端点回带 usage 的 SSE。
	//
	// 为什么两区给**相反**的倍率（国服 0.03 计费 / 国际版 0 免费）：
	// 这是实测到的真实形态，也是"倍率必须按区域各自取"的唯一有效检验 ——
	// 两区同值的话，"区域取错"这类缺陷根本测不出来。
	up := sseAndConfigUpstream(t,
		configBodyWithMultiplier("glm-5.3", "x0.03"),
		configBodyWithMultiplier("glm-5.3", "x0.00"))

	return NewHandler(Config{Pool: p, Upstream: up, Usage: stats}), stats
}

// postChat 发一次 chat 请求，返回状态码与响应体。
func postChat(t *testing.T, h *Handler, body string) (int, string) {
	t.Helper()
	req := httptest.NewRequest(http.MethodPost, "/v1/chat/completions", strings.NewReader(body))
	req.Header.Set("Content-Type", "application/json")
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, req)
	return rec.Code, rec.Body.String()
}

// billingGroupsOf 从 snapshot 里取某模型的计费分组。
func billingGroupsOf(t *testing.T, snap map[string]any, model string) []map[string]any {
	t.Helper()
	mb, ok := snap["modelBilling"].(map[string]any)
	if !ok {
		t.Fatalf("modelBilling 缺失 —— 前端拿不到倍率。snapshot keys=%v", keysOf(snap))
	}
	raw, ok := mb[model]
	if !ok {
		t.Fatalf("modelBilling[%s] 缺失 —— 用量没被记录（RecordBilled 没调用？）。有：%v", model, keysOf(mb))
	}
	groups, ok := raw.([]map[string]any)
	if !ok || len(groups) == 0 {
		t.Fatalf("modelBilling[%s] 类型异常或为空: %#v", model, raw)
	}
	return groups
}

func keysOf(m map[string]any) []string {
	out := make([]string, 0, len(m))
	for k := range m {
		out = append(out, k)
	}
	return out
}

// TestBillingE2EUsageCarriesMultiplier 完整链路：请求 → 用量 → 带倍率的归属。
//
// 核心断言是**归属与倍率都被填对了**：WorkBuddy 国服账号的请求，
// 其用量必须出现在 modelBilling 里、标着 region=cn、且带上国服的倍率。
func TestBillingE2EUsageCarriesMultiplier(t *testing.T) {
	cn := &auth.Auth{
		UID: "cn-1", AccessToken: "at-cn", Domain: "copilot.tencent.com",
		ExpiresAt: 9999999999, SoonestExpireAt: 1 << 40,
	}
	h, stats := billingE2EHandler(t, []*auth.Auth{cn})

	code, body := postChat(t, h, `{"model":"glm-5.3","stream":true,"messages":[{"role":"user","content":"hi"}]}`)
	if code != http.StatusOK {
		t.Fatalf("请求应成功，实际 %d：%s", code, body)
	}

	snap := stats.Snapshot(0)

	// ① 用量确实被记下了（走的是我们新加的 RecordBilled 路径）。
	summary, _ := snap["summary"].(map[string]any)
	if summary == nil {
		t.Fatal("summary 缺失")
	}
	if n, _ := summary["records"].(int64); n == 0 {
		t.Fatalf("用量未被记录（records=0）—— RecordBilled 没被调用？snapshot=%+v", snap)
	}

	// ② 归属被填对。
	g := billingGroupsOf(t, snap, "glm-5.3")[0]
	if g["product"] != auth.ProductWorkBuddy {
		t.Errorf("product 应为 workbuddy，实际 %v", g["product"])
	}
	if g["region"] != "cn" {
		t.Errorf("region 应为 cn（账号域名是 copilot.tencent.com），实际 %v", g["region"])
	}
	if hm, _ := g["hasMultiplier"].(bool); !hm {
		t.Error("WorkBuddy 应有倍率概念（hasMultiplier=true）")
	}
	// ③ 倍率真值被取到（假上游给国服 x0.03）。
	if v, ok := g["creditMultiplier"].(float64); !ok || v != 0.03 {
		t.Errorf("国服倍率应为 0.03（取自 /v3/config），实际 %#v", g["creditMultiplier"])
	}
}

// TestBillingE2EInternationalRegionGetsOwnMultiplier 国际版账号取**它自己**的倍率。
//
// # 为什么这条最关键
//
// 假上游给两区**相反**的倍率（国服 0.03 计费、国际版 0 免费）——
// 这正是实测到的真实形态。若 fillBilling 把区域取错（或倍率总从国服取），
// 国际版会被标成 0.03，用户会以为它在扣费而不敢用。
//
// 只测国服会让这类缺陷完全漏网，故必须两区各测一次。
func TestBillingE2EInternationalRegionGetsOwnMultiplier(t *testing.T) {
	intl := &auth.Auth{
		UID: "intl-1", AccessToken: "at-intl", Domain: "www.workbuddy.ai",
		ExpiresAt: 9999999999, SoonestExpireAt: 1 << 40,
	}
	h, stats := billingE2EHandler(t, []*auth.Auth{intl})

	code, body := postChat(t, h, `{"model":"glm-5.3","stream":true,"messages":[{"role":"user","content":"hi"}]}`)
	if code != http.StatusOK {
		t.Fatalf("请求应成功，实际 %d：%s", code, body)
	}

	g := billingGroupsOf(t, stats.Snapshot(0), "glm-5.3")[0]
	if g["region"] != "intl" {
		t.Errorf("region 应为 intl（账号域名是 www.workbuddy.ai），实际 %v", g["region"])
	}
	if v, ok := g["creditMultiplier"].(float64); !ok || v != 0 {
		t.Errorf("国际版倍率应为 0（假上游给 x0.00），实际 %#v —— 倍率取错区域了", g["creditMultiplier"])
	}
}

// TestBillingE2ENonWorkBuddyHasNoMultiplier ZCode 账号的用量不带倍率。
//
// ZCode 没有倍率数据源。给它编一个数字（哪怕 0）都会误导用户 ——
// 0 意味着"确定的免费"，而事实是"这个平台没有这个概念"。
func TestBillingE2ENonWorkBuddyHasNoMultiplier(t *testing.T) {
	zc := &auth.Auth{
		UID: "zc-1", AccessToken: "cred", Domain: "api.z.ai",
		Product: auth.ProductZcode, ExpiresAt: 9999999999,
	}
	p := pool.New("")
	p.Add(zc)
	stats := usage.New("")
	fz := &fakeQoder{stream: sseWithUsage}
	h := NewHandler(Config{Pool: p, Upstream: nil, MaxRotate: 1, Zcode: fz, Usage: stats})

	code, body := postChat(t, h, `{"model":"glm-5.3","stream":true,"messages":[{"role":"user","content":"hi"}]}`)
	if code != http.StatusOK {
		t.Fatalf("请求应成功，实际 %d：%s", code, body)
	}

	g := billingGroupsOf(t, stats.Snapshot(0), "glm-5.3")[0]
	if g["product"] != auth.ProductZcode {
		t.Errorf("product 应为 zcode，实际 %v", g["product"])
	}
	if hm, _ := g["hasMultiplier"].(bool); hm {
		t.Error("ZCode 没有倍率概念，hasMultiplier 应为 false")
	}
	if _, has := g["creditMultiplier"]; has {
		t.Error("ZCode 不该有 creditMultiplier 键（会误导用户）")
	}
	// 非 WorkBuddy 不分区：region 应缺席（不是空串 —— 空串会被当成一个区域）。
	if r, has := g["region"]; has {
		t.Errorf("ZCode 不分区，region 应缺席，实际 %v", r)
	}
}

// TestBillingE2EMultiplierSurvivesJSONRoundTrip 倍率经 JSON 往返仍是数字/null。
//
// ⚠ 这条是为了钉住一个**已实际发生过的**缺陷：`Billing.Multiplier` 是
// `*float64`，若直接放进 map，序列化后是**指针地址**（实测出现
// `creditMultiplier: 0x15bb13fb22f8` 这样的值）—— 一个静默的错值，
// 界面会把它当成巨大的倍率显示。必须在 Snapshot 里解引用。
func TestBillingE2EMultiplierSurvivesJSONRoundTrip(t *testing.T) {
	cn := &auth.Auth{
		UID: "cn-1", AccessToken: "at", Domain: "copilot.tencent.com",
		ExpiresAt: 9999999999, SoonestExpireAt: 1 << 40,
	}
	h, stats := billingE2EHandler(t, []*auth.Auth{cn})
	if code, body := postChat(t, h, `{"model":"glm-5.3","stream":true,"messages":[{"role":"user","content":"hi"}]}`); code != http.StatusOK {
		t.Fatalf("请求应成功，实际 %d：%s", code, body)
	}

	raw, err := json.Marshal(stats.Snapshot(0))
	if err != nil {
		t.Fatalf("序列化失败: %v", err)
	}
	var parsed map[string]any
	if err := json.Unmarshal(raw, &parsed); err != nil {
		t.Fatalf("反序列化失败: %v", err)
	}
	mb := parsed["modelBilling"].(map[string]any)
	groups := mb["glm-5.3"].([]any)
	g := groups[0].(map[string]any)

	v, has := g["creditMultiplier"]
	if !has {
		t.Fatal("creditMultiplier 键应存在")
	}
	switch v.(type) {
	case nil:
		t.Error("本用例上游给了真值 x0.03，不该是 null（说明倍率没取到）")
	case float64:
		if v.(float64) != 0.03 {
			t.Errorf("倍率应为 0.03，实际 %v", v)
		}
	default:
		t.Errorf("creditMultiplier 应是数字或 null，实际 %T = %v（指针未解引用？）", v, v)
	}
}

// sseAndConfigUpstream 造一个**按路径分流**的假上游：
//
//	/v3/config 类路径 → 返回带 credits 的配置体（供能力/倍率探测）
//	其余（聊天）      → 返回带 usage 的 SSE
//
// 为什么需要分流：`regionBillingUpstream` 对所有路径回同一个体，
// 用它跑聊天会拿到 JSON 而不是 SSE，测不出用量采集。
func sseAndConfigUpstream(t *testing.T, cnConfig, intlConfig string) *upstream.Client {
	t.Helper()
	up := &upstream.Client{
		HTTP: &http.Client{Transport: roundTripFunc(func(r *http.Request) (*http.Response, error) {
			// 配置端点（能力真值 / 倍率来源）。
			if strings.Contains(r.URL.Path, "config") {
				body := cnConfig
				if strings.Contains(r.URL.Host, "intl") {
					body = intlConfig
				}
				return &http.Response{
					StatusCode: http.StatusOK,
					Header:     http.Header{"Content-Type": []string{"application/json"}},
					Body:       nopCloser(body),
				}, nil
			}
			// 聊天端点：回带 usage 的 SSE。
			return &http.Response{
				StatusCode: http.StatusOK,
				Header:     http.Header{"Content-Type": []string{"text/event-stream"}},
				Body:       nopCloser(sseWithUsage),
			}, nil
		})},
		ChatBaseCN:    "https://cn.example",
		BillingBaseCN: "https://cn.example",
		BaseIntl:      "https://intl.example",
	}
	return up
}
