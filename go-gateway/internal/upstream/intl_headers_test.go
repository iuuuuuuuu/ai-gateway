package upstream

// intl_headers_test.go 锁定「国际版必须用 WorkBuddy 客户端身份」这个修复。
//
// # 守的是所有者 2026-09-27 报的真实缺陷
//
// 原话：「这难道不就是使用国际版就报错吗？我刚通过抓包，国际版是可以正常
// 对话的，你对比一下，然后把这个问题给我解决掉，解决国际版不能使用的
// 问题」。
//
// # 根因（抓包对照得出，不是推断）
//
// 同一账号、同一模型、同一端点 POST /v2/chat/completions：
//
//	官方国际版客户端（WorkBuddyAI.exe 5.6.2） → HTTP 200，正常流式
//	网关                                      → HTTP 403 code=11140 request illegal
//
// 差异在请求身份。官方客户端发的是：
//
//	User-Agent: WorkBuddy/5.6.2 WorkBuddy/5.6.2 CLI/2.147.0
//	X-IDE-Type/Name/Version: WorkBuddy / WorkBuddy / 5.6.2
//	X-Agent-Intent/Purpose/Type: craft / conversation / main
//	x-codebuddy-request: 1
//
// 而网关**两个区域一律**发国服 CLI 的 `CLI/2.63.2 CodeBuddy/2.63.2`
// —— 国际版据此判定「不是我们的客户端」，直接 11140 拒绝。
//
// 这些测试之所以必要：**UA 错了不会报错，只会 403**，而 403 在客户端
// 界面上还会被渲染成「API 密钥无效」（见 client_status_test.go），
// 排查方向会被彻底带偏。

import (
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

// 账号夹具复用 proxy_test.go 的 cnAuth() / intlAuth()（同一包，避免重名）。

// TestIntlUsesWorkBuddyClientUA 国际版必须发 WorkBuddy 客户端 UA。
//
// 这是本次修复的**核心断言**：用国服 CLI 的 UA 打国际版会 403 11140。
func TestIntlUsesWorkBuddyClientUA(t *testing.T) {
	req := httptest.NewRequest(http.MethodPost, "https://www.workbuddy.ai/v2/chat/completions", nil)
	ChatHeaders(req, intlAuth())
	ua := req.Header.Get("User-Agent")
	if !strings.HasPrefix(ua, "WorkBuddy/") {
		t.Fatalf("国际版 UA 必须以 WorkBuddy/ 开头（否则上游回 403 11140，"+
			"而客户端会把它显示成「API 密钥无效」）；实际 %q", ua)
	}
	if ua == clientUACN {
		t.Fatalf("国际版不得复用国服 CLI 的 UA（%q）—— 那正是本次缺陷", clientUACN)
	}
}

// TestCNDoesNotUseWorkBuddyUA 国服**不得**被改成国际版 UA。
//
// 防"修过头"：国服当前实测正常，动它的请求身份没有任何收益，
// 只会引入一类新的不确定性。
func TestCNDoesNotUseWorkBuddyUA(t *testing.T) {
	req := httptest.NewRequest(http.MethodPost, "https://copilot.tencent.com/v2/chat/completions", nil)
	ChatHeaders(req, cnAuth())
	ua := req.Header.Get("User-Agent")
	if ua != clientUACN {
		t.Fatalf("国服 UA 应保持 %q（国服实测正常，不该跟着改）；实际 %q", clientUACN, ua)
	}
	if strings.HasPrefix(ua, "WorkBuddy/") {
		t.Fatalf("国服不得发 WorkBuddy 客户端 UA：%q", ua)
	}
}

// TestIntlSendsIDEAndAgentHeaders 国际版必须声明客户端身份头。
//
// 这些头是抓包对照出来的差异项。缺了它们，上游仍可能把请求判成
// 非官方客户端 —— 单测能锁住"我们确实发了"，而线上是否**足够**
// 由真实请求验证（见文件头的实测记录）。
func TestIntlSendsIDEAndAgentHeaders(t *testing.T) {
	req := httptest.NewRequest(http.MethodPost, "https://www.workbuddy.ai/v2/chat/completions", nil)
	ChatHeaders(req, intlAuth())

	want := map[string]string{
		"X-IDE-Type":          "WorkBuddy",
		"X-IDE-Name":          "WorkBuddy",
		"X-IDE-Version":       "5.6.2",
		"X-Agent-Intent":      "craft",
		"X-Agent-Purpose":     "conversation",
		"X-Agent-Type":        "main",
		"x-codebuddy-request": "1",
	}
	for k, v := range want {
		if got := req.Header.Get(k); got != v {
			t.Errorf("国际版应发 %s=%q，实际 %q", k, v, got)
		}
	}
}

// TestCNHasNoIntlOnlyHeaders 国服路径不该带这些国际版专属头。
//
// 理由与 TestCNDoesNotUseWorkBuddyUA 相同：国服没要求它们，
// 多发等于改变国服的请求画像，而国服是当前的主力通道。
func TestCNHasNoIntlOnlyHeaders(t *testing.T) {
	req := httptest.NewRequest(http.MethodPost, "https://copilot.tencent.com/v2/chat/completions", nil)
	ChatHeaders(req, cnAuth())
	for _, k := range []string{
		"X-IDE-Type", "X-IDE-Name", "X-IDE-Version",
		"X-Agent-Intent", "X-Agent-Purpose", "X-Agent-Type", "x-codebuddy-request",
	} {
		if got := req.Header.Get(k); got != "" {
			t.Errorf("国服不该发国际版专属头 %s（实际 %q）", k, got)
		}
	}
}

// TestIntlOriginRefererStaysIntl 区域头必须跟随账号区域（既有不变式，一并钉住）。
//
// Origin/Referer 错了会让上游落到另一侧的服务，与 UA 是同一类
// 「请求身份」问题，放在一起防回归。
func TestIntlOriginRefererStaysIntl(t *testing.T) {
	req := httptest.NewRequest(http.MethodPost, "https://www.workbuddy.ai/v2/chat/completions", nil)
	ChatHeaders(req, intlAuth())
	if got := req.Header.Get("Origin"); got != originRefererIntl {
		t.Errorf("国际版 Origin 应为 %q，实际 %q", originRefererIntl, got)
	}
	if got := req.Header.Get("Referer"); got != originRefererIntl+"/" {
		t.Errorf("国际版 Referer 应为 %q，实际 %q", originRefererIntl+"/", got)
	}
}

// TestIntlHeadersSurviveNoRouteMarking 用户手动禁用（no_route）不影响请求身份。
//
// 禁用只表示"不接流量"，任务与请求身份构造都不该因此变化 ——
// 若有人把 no_route 混进区域判据，这条会红。
func TestIntlHeadersSurviveNoRouteMarking(t *testing.T) {
	a := intlAuth()
	a.NoRoute = true
	req := httptest.NewRequest(http.MethodPost, "https://www.workbuddy.ai/v2/chat/completions", nil)
	ChatHeaders(req, a)
	if ua := req.Header.Get("User-Agent"); !strings.HasPrefix(ua, "WorkBuddy/") {
		t.Errorf("no_route 账号仍是国际版，UA 应保持 WorkBuddy/ 前缀，实际 %q", ua)
	}
	if req.Header.Get("X-IDE-Type") != "WorkBuddy" {
		t.Error("no_route 不该影响国际版身份头")
	}
}
