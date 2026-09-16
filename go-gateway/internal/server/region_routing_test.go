package server

import (
	"bytes"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/pool"
	"workbuddy2api/internal/upstream"
)

// ---------------------------------------------------------------------------
// 区域感知路由回归
//
// 实测缺陷（2026-09-16）：glm-5.3 / glm-5.2 是**两区共有**的模型名，但两区是
// **不同的后端模型**：
//
//	国服   「旗舰模型，擅长复杂软件工程与长程 Agent 任务」 out=64000 → 能读图
//	国际版 「能力均衡，适合日常使用」                      out=48000 → 读不到图
//
// 国际版后端把图片替换成固定占位符：prompt_tokens 增量恒为 +29，
// 把图片从 937 字节换到 471 KB 都不变，模型只能回「无法查看图片」。
//
// 而选号只看到期日与冷却、不看区域，于是同一个 glm-5.3 随机命中两个后端 ——
// 用户看到「同一个模型时好时坏」。本组测试锁住「带图片时偏好国服」。
// ---------------------------------------------------------------------------

// cnAuth / intlAuth 造两个区域的账号，到期日固定为 farFuture。
//
// ExpiresAt 必须给一个远期值：为 0 时 NeedsRefresh 恒为 true，forwardChat 会
// 先去刷新 token，测试就变成在测刷新而不是在测选号。
//
// SoonestExpireAt 决定选号的到期分层档位：**同档内是加权随机**，因此要断言
// 「该选谁」就必须让它们的档位不同（见 cnAuthExpiring / intlAuthExpiring）。
// 默认值只用于「不关心选到谁」的用例。
const farFuture = 1 << 40

func cnAuth(uid string) *auth.Auth {
	return cnAuthExpiring(uid, farFuture)
}

func intlAuth(uid string) *auth.Auth {
	return intlAuthExpiring(uid, farFuture)
}

// cnAuthExpiring / intlAuthExpiring 指定「最近到期积分」的到期时刻。
//
// 到期日更早的账号会被优先选中（先烧快过期的额度），这是选号的核心策略，
// 也是测试用来确定性地指定「预期选到哪个账号」的手段。
func cnAuthExpiring(uid string, soonestExpireAt int64) *auth.Auth {
	return &auth.Auth{UID: uid, AccessToken: "t", Domain: "www.workbuddy.cn", ExpiresAt: 9999999999, SoonestExpireAt: soonestExpireAt}
}

func intlAuthExpiring(uid string, soonestExpireAt int64) *auth.Auth {
	return &auth.Auth{UID: uid, AccessToken: "t", Domain: "www.workbuddy.ai", ExpiresAt: 9999999999, SoonestExpireAt: soonestExpireAt}
}

// regionUpstream 造一个「国服与国际版都指向本地假上游」的 client。
//
// newFakeUpstream 只设了 ChatBaseCN；国际版账号会走 chatBase 的 BaseIntl 分支，
// 不设它就会真的去连 www.workbuddy.ai（测试变慢且依赖外网）。
func regionUpstream(t *testing.T) *upstream.Client {
	t.Helper()
	up := newFakeUpstream(t, func(string) (int, string, bool) {
		return http.StatusOK, sseOK, true
	})
	up.BaseIntl = "https://fake.example"
	return up
}

// TestImageRouteOnlyForKnownDifferingModels 只有确知两区**都**能读、或都不能读的模型才不约束。
//
// 范围刻意收窄：对两区都能读图的模型（hy3/kimi-*）做区域偏好只会白白放弃
// 一半账号的额度，而没有任何收益。
func TestImageRouteOnlyForKnownDifferingModels(t *testing.T) {
	cases := []struct {
		model   string
		hasImg  bool
		wantReg auth.Region
		wantReq bool
		why     string
	}{
		{"glm-5.3", true, auth.RegionCN, true, "实测国际版读不到图 → 必须迁移到国服，选不出要报错"},
		{"glm-5.2", true, auth.RegionCN, true, "同上"},
		{"GLM-5.3", true, auth.RegionCN, true, "模型名大小写不敏感"},
		{" glm-5.3 ", true, auth.RegionCN, true, "前后空白应被裁剪"},
		{"glm-5.3", false, auth.RegionAny, false, "纯文本不约束（两区文本能力都正常）"},
		{"hy3", true, auth.RegionAny, false, "实测两区都能读图 → 约束只会白白损失一半额度"},
		{"kimi-k2.6", true, auth.RegionAny, false, "同上"},
		{"deepseek-v4.1-flash", true, auth.RegionAny, false, "无实测结论 → 不臆断"},
		{"glm-5.1", true, auth.RegionAny, false, "国服专有，不存在两区歧义"},
		{"", true, auth.RegionAny, false, "取不到模型名时不做任何约束"},
	}
	for _, c := range cases {
		got := imageRouteFor(c.model, c.hasImg)
		if got.Region != c.wantReg || got.Required != c.wantReq {
			t.Errorf("imageRouteFor(%q, hasImage=%v) = {%v required=%v}，期望 {%v required=%v}（%s）",
				c.model, c.hasImg, got.Region, got.Required, c.wantReg, c.wantReq, c.why)
		}
	}
}

// TestMeasuredOverridesUpstreamClaim 实测结论必须覆盖上游的谎报。
//
// 这是本特性的**根因修复**：上游对 glm-5.3/glm-5.2 在两区都报
// supportsImages=true，而实测国际版读不到图。若只透传上游声明，
// 客户端会在本地就放行图片，用户拿到的是模型回「我无法查看图片」。
func TestMeasuredOverridesUpstreamClaim(t *testing.T) {
	yes := true
	cases := []struct {
		model  string
		region auth.Region
		upstr  *bool
		want   *bool
		why    string
	}{
		{"glm-5.3", auth.RegionIntl, &yes, boolP(false), "上游谎报 true，实测国际版读不到图 → 必须覆盖成 false"},
		{"glm-5.3", auth.RegionCN, &yes, boolP(true), "国服实测能读，与上游一致"},
		{"glm-5.3", auth.RegionIntl, nil, boolP(false), "上游未声明也要根据实测下发 false"},
		{"hy3", auth.RegionIntl, &yes, &yes, "hy3 无差异结论 → 原样透传上游"},
		{"hy3", auth.RegionIntl, nil, nil, "未实测时 nil 仍是 nil（三态不能变成 false）"},
	}
	for _, c := range cases {
		got := measuredOverride(c.model, c.region, c.upstr)
		if !sameBoolPtr(got, c.want) {
			t.Errorf("measuredOverride(%q, %v, %v) = %v，期望 %v（%s）",
				c.model, c.region, ptrStr(c.upstr), ptrStr(got), ptrStr(c.want), c.why)
		}
	}
}

func boolP(b bool) *bool { return &b }

func sameBoolPtr(a, b *bool) bool {
	if a == nil || b == nil {
		return a == nil && b == nil
	}
	return *a == *b
}

func ptrStr(p *bool) string {
	if p == nil {
		return "nil"
	}
	if *p {
		return "true"
	}
	return "false"
}

// TestRequestHasImageDetectsImageParts 识别图片分片。
func TestRequestHasImageDetectsImageParts(t *testing.T) {
	cases := []struct {
		name string
		body string
		want bool
	}{
		{"纯文本字符串 content", `{"messages":[{"role":"user","content":"你好"}]}`, false},
		{"纯文本分片", `{"messages":[{"role":"user","content":[{"type":"text","text":"你好"}]}]}`, false},
		{"标准 image_url 分片", `{"messages":[{"role":"user","content":[{"type":"text","text":"看图"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AAA"}}]}]}`, true},
		{"只有 image_url 键、无 type", `{"messages":[{"role":"user","content":[{"image_url":{"url":"http://x/y.png"}}]}]}`, true},
		{"input_image（responses 转换后）", `{"messages":[{"role":"user","content":[{"type":"input_image","image_url":"http://x/y.png"}]}]}`, true},
		{"Anthropic image 块", `{"messages":[{"role":"user","content":[{"type":"image","source":{"data":"AAA"}}]}]}`, true},
		{"无 messages", `{"model":"glm-5.3"}`, false},
		{"空 body", ``, false},
		{"非法 JSON", `{not json`, false},
		{"多轮里第二轮才带图", `{"messages":[{"role":"user","content":"第一轮"},{"role":"assistant","content":"好"},{"role":"user","content":[{"type":"image_url","image_url":{"url":"http://x"}}]}]}`, true},
	}
	for _, c := range cases {
		if got := requestHasImage([]byte(c.body)); got != c.want {
			t.Errorf("%s：requestHasImage = %v，期望 %v", c.name, got, c.want)
		}
	}
}

// TestImageRequestRoutesToCNAccount 带图片的 glm-5.3 请求必须落到国服账号。
//
// 这是本特性的核心断言：池里同时有国服与国际版账号时，图片请求不能
// 随机命中读不到图的国际版后端。
func TestImageRequestRoutesToCNAccount(t *testing.T) {
	// 国际版账号到期更早 —— 按原有分层选号它会被优先选中，
	// 因此这个用例只有在区域偏好真正生效时才会通过。
	p := testPoolWith(
		intlAuthExpiring("intl-1", 1<<40),
		cnAuthExpiring("cn-1", (1<<40)+86400*30),
	)
	h := NewHandler(Config{Pool: p, Upstream: regionUpstream(t)})

	body := []byte(`{"model":"glm-5.3","stream":true,"messages":[{"role":"user","content":[{"type":"text","text":"看图"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AAA"}}]}]}`)
	res, _, err := h.forwardChat(body, true, "")
	if err != nil {
		t.Fatalf("forwardChat 失败: %v", err)
	}
	defer res.Stream.Close()
	if res.UID != "cn-1" {
		t.Fatalf("带图片的 glm-5.3 应路由到国服账号，实际 %q（国际版后端读不到图）", res.UID)
	}
}

// TestPlainTextGlmStillUsesAnyAccount 纯文本 glm-5.3 不受区域偏好影响。
//
// 关键回归保护：区域偏好只该作用于图片请求。若误伤纯文本，会白白放弃
// 一半账号的额度（且用户完全无感，只表现为额度消耗变慢）。
func TestPlainTextGlmStillUsesAnyAccount(t *testing.T) {
	p := testPoolWith(
		intlAuthExpiring("intl-1", 1<<40),        // 到期更早 → 分层选号优先选它
		cnAuthExpiring("cn-1", (1<<40)+86400*30), // 晚 30 天 → 落选
	)
	h := NewHandler(Config{Pool: p, Upstream: regionUpstream(t)})

	body := []byte(`{"model":"glm-5.3","stream":true,"messages":[{"role":"user","content":"你好"}]}`)
	res, _, err := h.forwardChat(body, true, "")
	if err != nil {
		t.Fatalf("forwardChat 失败: %v", err)
	}
	defer res.Stream.Close()
	if res.UID != "intl-1" {
		t.Fatalf("纯文本请求不该受区域偏好影响，应仍按到期分层选中 intl-1，实际 %q", res.UID)
	}
}

// TestImageRequestErrorsWhenNoCNAccount 池里没有国服账号时必须**明确报错**，而不是静默跨区。
//
// 这是本次语义变更的核心：偏好改成「强制迁移」。
// 静默跨区会让国际版后端把图片换成占位符，模型回「我无法查看图片」——
// 用户看到的是一个**成功**的响应，完全无从判断问题出在哪。
// 明确报错才能让用户知道「去加一个国服账号」。
func TestImageRequestErrorsWhenNoCNAccount(t *testing.T) {
	p := testPoolWith(intlAuth("intl-1"), intlAuth("intl-2"))
	h := NewHandler(Config{Pool: p, Upstream: regionUpstream(t)})

	body := []byte(`{"model":"glm-5.3","stream":true,"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"http://x"}}]}]}`)
	_, status, err := h.forwardChat(body, true, "")
	if err == nil {
		t.Fatal("池里没有国服账号时不该静默跨区，应报错")
	}
	if status == http.StatusOK {
		t.Fatalf("应返回失败状态，实际 %d", status)
	}
	f := failureOf(err)
	if f == nil || f.Kind != FailureImageRegionUnavailable {
		t.Fatalf("应为 FailureImageRegionUnavailable，实际 %v（err=%v）", f, err)
	}
	// 文案必须说清「缺哪个区域的账号」与「怎么办」。
	for _, want := range []string{"国服", "账号"} {
		if !strings.Contains(err.Error(), want) {
			t.Errorf("错误文案应包含 %q，实际: %s", want, err.Error())
		}
	}
}

// TestPlainTextWorksWithoutCNAccount 纯文本不受区域约束影响（上面那条只针对图片）。
func TestPlainTextWorksWithoutCNAccount(t *testing.T) {
	p := testPoolWith(intlAuth("intl-1"))
	h := NewHandler(Config{Pool: p, Upstream: regionUpstream(t)})

	body := []byte(`{"model":"glm-5.3","stream":true,"messages":[{"role":"user","content":"你好"}]}`)
	res, _, err := h.forwardChat(body, true, "")
	if err != nil {
		t.Fatalf("纯文本请求不该受区域约束，实际错误: %v", err)
	}
	defer res.Stream.Close()
	if res.UID != "intl-1" {
		t.Fatalf("应选中唯一的国际版账号，实际 %q", res.UID)
	}
}

// TestImageRequestFallsBackWhenNoCNAccount 已被
// TestImageRequestErrorsWhenNoCNAccount 取代（语义从「偏好」改成「强制」）。

// TestImageRequestErrorsWhenCooledCNAccount 国服账号冷却时同样报错，不退而求其次。
//
// 「国服号都在冷却」是**暂时**缺号，但仍然不能跨区：跨区会让模型回
// 「我无法查看图片」，用户以为模型不支持，实际是网关选错了后端。
// 报错文案里应提示可稍后重试或去掉图片。
func TestImageRequestErrorsWhenCooledCNAccount(t *testing.T) {
	p := testPoolWith(cnAuth("cn-cooling"), intlAuth("intl-ok"))
	p.Cooldown("cn-cooling", pool.CoolSoft, time.Hour, "测试冷却")
	h := NewHandler(Config{Pool: p, Upstream: regionUpstream(t)})

	body := []byte(`{"model":"glm-5.3","stream":true,"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"http://x"}}]}]}`)
	_, _, err := h.forwardChat(body, true, "")
	if err == nil {
		t.Fatal("国服账号冷却时不该跨区到读不到图的国际版账号，应报错")
	}
	if f := failureOf(err); f == nil || f.Kind != FailureImageRegionUnavailable {
		t.Fatalf("应为 FailureImageRegionUnavailable，实际 %v（err=%v）", f, err)
	}

	// 同一账号池下，纯文本请求仍应正常走 intl-ok（不受区域强制约束）。
	textBody := []byte(`{"model":"glm-5.3","stream":true,"messages":[{"role":"user","content":"你好"}]}`)
	res, _, err := h.forwardChat(textBody, true, "")
	if err != nil {
		t.Fatalf("纯文本请求不该受影响，实际错误: %v", err)
	}
	defer res.Stream.Close()
	if res.UID != "intl-ok" {
		t.Fatalf("纯文本应走唯一可用的 intl-ok，实际 %q", res.UID)
	}
}

// TestRegionRoutingThroughHTTP 端到端：走完整 HTTP 栈验证区域路由。
func TestRegionRoutingThroughHTTP(t *testing.T) {
	p := testPoolWith(intlAuth("intl-1"), cnAuth("cn-1"))
	h := NewHandler(Config{Pool: p, Upstream: regionUpstream(t)})

	body, _ := json.Marshal(map[string]any{
		"model":  "glm-5.3",
		"stream": true,
		"messages": []any{map[string]any{
			"role": "user",
			"content": []any{
				map[string]any{"type": "text", "text": "看图"},
				map[string]any{"type": "image_url", "image_url": map[string]any{"url": "data:image/png;base64,AAA"}},
			},
		}},
	})
	req := httptest.NewRequest(http.MethodPost, "/v1/chat/completions", bytes.NewReader(body))
	rec := httptest.NewRecorder()
	h.mux.ServeHTTP(rec, req)
	if rec.Code != http.StatusOK {
		t.Fatalf("/v1/chat/completions 应返回 200，实际 %d：%s", rec.Code, rec.Body.String())
	}
}
