package server

// clienttext_test.go 守住「客户端不会把我们的错误读成『API 密钥无效』」这条线。
//
// # 守的是什么（2026-09-29 所有者实测）
//
// 所有者装了修好**状态码归一**（403→502）的新包，实测**仍然**显示
// 「本轮运行失败 API 密钥无效」。查证结论是上一版修错了地方：
// 客户端判 AUTH 靠的是**文案里的数字**，不是 HTTP 状态码。
//
// DSH 的实际分类器（`@deepseek-ai/dsh-llm-pi-ai/lib/index.js:1376-1387`）：
//
//	function classifyPiAiError(message) {
//	  if (/\b(?:401|403)\b/.test(message)) return "AUTH";
//	  ...
//	}
//
// 而 message 由 `@earendil-works/pi-ai/dist/utils/error-body.js:111-118`
// 的 formatProviderError 把**响应体原文**拼进去（`"<status>: <body>"`）。
//
// ⇒ 只要对外文案里出现裸 401/403，界面就说「密钥无效」，与密钥对不对无关。
//
// 本文件把这个判据钉成**可自动化的断言**：任何一条面向客户端的文案，
// 都不许匹配 `\b(?:401|403)\b`。这样将来谁再加一条带 403 的文案，
// 会在 CI 里直接红，而不是等所有者装包实测才发现。

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"regexp"
	"strings"
	"testing"
	"time"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/qoder"
	"workbuddy2api/internal/upstream"
)

// authMisreadRe 是客户端正则的**逐字复刻**。
//
// 刻意不从 clienttext.go 复用 `authMisreadPattern`：那样一旦有人改了闸门的
// 正则（比如收窄成只认 403），这些测试会跟着一起变宽，测不出「客户端仍会
// 读成 AUTH」这件事。判据必须来自客户端，不能来自我们自己的实现。
//
// ⚠ 改这里之前先去读客户端那份源码，别照抄我们的实现。
var authMisreadRe = regexp.MustCompile(`\b(?:401|403)\b`)

// mustJSONString 把一个响应体序列化成字符串，供正则判据检查。
//
// 直接查序列化结果而不是查 map 里的单个字段：万一有人把 message 挪到别的
// 字段（或塞进 details 数组），只查 message 的测试会**静默放行**，
// 而客户端看到的是整个 JSON。
func mustJSONString(t *testing.T, v any) string {
	t.Helper()
	b, err := json.Marshal(v)
	if err != nil {
		t.Fatalf("序列化错误体失败（判据无法执行）：%v", err)
	}
	return string(b)
}

// assertNoAuthMisread 断言文案不会被客户端读成 AUTH。
func assertNoAuthMisread(t *testing.T, what, msg string) {
	t.Helper()
	if m := authMisreadRe.FindString(msg); m != "" {
		t.Errorf("%s 里出现了裸 %q —— 客户端会把它读成「API 密钥无效」"+
			"（DSH 的 classifyPiAiError 用文案正则判 AUTH，与 HTTP 状态码无关）：%q",
			what, m, msg)
	}
}

// TestClientSafeMessageRewritesBareAuthCodes 闸门必须把裸 401/403 改写成 4xx。
func TestClientSafeMessageRewritesBareAuthCodes(t *testing.T) {
	cases := []struct {
		name string
		in   string
		want string
	}{
		{
			"上一版真凶：紧贴中文的 403",
			"账号在本设备尚未完成登录（上游 403 业务码 11140），已冷却该账号并自动切换",
			"账号在本设备尚未完成登录（上游 4xx 业务码 11140），已冷却该账号并自动切换",
		},
		{
			"中文与数字之间没有空格（最容易被漏掉的形状）",
			"上游403业务码 11140",
			"上游4xx业务码 11140",
		},
		{
			"upstream.Error.Error() 的 (http NNN) 形状",
			"upstream account_unusable (http 403): {\"code\":11140}",
			"upstream account_unusable (http 4xx): {\"code\":11140}",
		},
		{
			"401 同样要改（客户端正则里两个值等价）",
			"账号登录态已失效（HTTP 401）",
			"账号登录态已失效（HTTP 4xx）",
		},
		{
			"同一句里两个都要改",
			"上游回 401 或 403 都算",
			"上游回 4xx 或 4xx 都算",
		},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			got := clientSafeMessage(c.in)
			if got != c.want {
				t.Errorf("clientSafeMessage(%q)\n 得到 %q\n 期望 %q", c.in, got, c.want)
			}
			assertNoAuthMisread(t, "闸门输出", got)
		})
	}
}

// TestClientSafeMessageLeavesBusinessCodesAlone 业务码**不得**被误伤。
//
// 这是闸门最容易出事的地方：上游业务码是四位数字，里面**恰好可能含有
// "401" 这个子串** —— 14018 的中间三位就是 401。若闸门没有词边界保护，
// 「账号额度已耗尽（上游 14018）」会被改成「上游 14xx8」，
// 用户拿到的归因就废了。
func TestClientSafeMessageLeavesBusinessCodesAlone(t *testing.T) {
	for _, code := range []string{
		"11140", // 账号在本设备未完成登录
		"14018", // 额度耗尽（★中间三位正是 "401"，最危险的一个）
		"11133", // 参数被模型提供商拒
		"11102", // 模型在这条通道上不存在
		"11115", // 上下文超长
		"10605", // 免费模型排队
		"6004",  // 模型级限流
		"3007",  // ZCode 人机验证
	} {
		msg := "上游业务码 " + code + "，请按提示处理"
		if got := clientSafeMessage(msg); got != msg {
			t.Errorf("业务码 %s 被闸门误伤了：\n 得到 %q\n 期望 %q（逐字不变）", code, got, msg)
		}
	}
}

// TestClientSafeMessageLeavesRetryableCodesAlone 429 / 5xx / 400 / 413 必须**原样保留**。
//
// 客户端那张表里这几条数字规则是**对的**：限流就该退避、上游故障就该重试、
// 请求写错了就该改请求。闸门只该治 401/403 这一对（→ AUTH，误导用户去反复
// 改密钥）。若哪天有人图省事把闸门写成「所有三位状态码都改写」，
// 客户端就再也认不出限流与上游故障了 —— 这条防的就是那种"顺手"。
func TestClientSafeMessageLeavesRetryableCodesAlone(t *testing.T) {
	for _, msg := range []string{
		"账号被上游限流（HTTP 429），已短暂冷却，稍后会自动重试",
		"上游服务异常（HTTP 503），已切换到其他账号",
		"上游服务异常（HTTP 500），已切换到其他账号",
		"上游返回 404（接口或模型不存在），已短暂冷却并切换账号",
		"请求体过大（HTTP 413）",
		"上游拒绝本次请求（HTTP 400）",
	} {
		if got := clientSafeMessage(msg); got != msg {
			t.Errorf("可重试状态码被闸门改掉了（会让客户端认不出限流/上游故障）：\n"+
				" 得到 %q\n 期望 %q（逐字不变）", got, msg)
		}
	}
}

// TestClientSafeMessageIsIdempotent 闸门必须幂等。
//
// 三个协议的出口各过一次，将来若有人把它装到更内层（同一条文案过两次），
// 幂等是唯一能保证不出错的属性。
func TestClientSafeMessageIsIdempotent(t *testing.T) {
	for _, msg := range []string{
		"上游 403 业务码 11140",
		"nothing to rewrite",
		"",
		"14018 与 401 与 403 混在一起",
	} {
		once := clientSafeMessage(msg)
		if twice := clientSafeMessage(once); twice != once {
			t.Errorf("闸门不幂等：%q → %q → %q", msg, once, twice)
		}
	}
}

// TestNoClientFacingMessageReadsAsAuth 是**核心回归**：把每一条面向客户端的
// 文案都真造出来，逐条断言不会被客户端读成 AUTH。
//
// 为什么不是「grep 源码里有没有 403」：文案是**拼**出来的（状态码、业务码、
// 区域名、模型名都是变量），静态扫描只能看到格式串，看不到最终文本。
// 而上一版漏掉的那个分支（`ErrServer` 把 status 原样写进文案）恰恰就是
// 一个格式串里看不出问题的形状 —— 只有真跑一遍才会暴露。
//
// 覆盖面刻意拉满：FriendlyMessage 的**每一个** ErrKind × 一串状态码。
func TestNoClientFacingMessageReadsAsAuth(t *testing.T) {
	// 上游响应的真实形状（含各业务码），用于喂给所有分支。
	bodies := []string{
		`{"code":11140,"msg":"request illegal"}`,
		`{"error":{"data":{"code":14018,"msg":"Credits exhausted."}}}`,
		`{"code":6004,"msg":"model rate limited"}`,
		`{"code":11133,"msg":"Invalid request parameters"}`,
		`{"code":11102,"msg":"model [Qwen3.8-Flash] service info not found"}`,
		`{"code":11115,"msg":"context too long"}`,
		`{"error":{"message":"the reasoning effort value is not supported by the current model"}}`,
		`{}`,
		"",
	}
	kinds := []upstream.ErrKind{
		upstream.ErrNone,
		upstream.ErrHardCredit,
		upstream.ErrSoftRate,
		upstream.ErrSessionDead,
		upstream.ErrNotFound,
		upstream.ErrServer,
		upstream.ErrClient,
		upstream.ErrModelRate,
		upstream.ErrContextTooLong,
		upstream.ErrEffortRejected,
		upstream.ErrModelNotInRegion,
		upstream.ErrAccountUnusable,
		upstream.ErrModelParamInvalid,
	}
	// ★ 401/403 必须**在**表里：上一版漏掉的正是 ErrServer 分支把 status
	// 原样写进文案，而那个分支在真实路径上只在 5xx 时被选中 ——
	// 一旦有人改动 Classify 的判定顺序，401/403 就会走进来。
	statuses := []int{0, 400, 401, 402, 403, 404, 413, 429, 500, 502, 503, 504}

	for _, kind := range kinds {
		for _, status := range statuses {
			for _, body := range bodies {
				msg := upstream.FriendlyMessage(kind, status, body)
				if msg == "" {
					continue // 空串表示"没有更优表述"，调用方会走别的回退路径
				}
				assertNoAuthMisread(t, "FriendlyMessage("+kind.String()+")", msg)
			}
		}
	}
}

// TestProtocolIndependentMessagesReadAsAuthSafe 其余几处独立的文案生成器。
//
// 它们与 FriendlyMessage 是**并列**的出口（不是它的分支），
// 各自带状态码或用户输入，故单独过一遍。
func TestProtocolIndependentMessagesReadAsAuthSafe(t *testing.T) {
	msgs := map[string]string{
		"egressIPBlockedMessage(2)":     egressIPBlockedMessage(2),
		"egressIPBlockedMessage(3)":     egressIPBlockedMessage(3),
		"unusualActivityMessage(60s)":   unusualActivityMessage(60 * time.Second),
		"unusualActivityMessage(1ms)":   unusualActivityMessage(time.Millisecond),
		"imageRegionUnavailableMessage": imageRegionUnavailableMessage(auth.RegionIntl),
		"modelNotInProductMessage": modelNotInProductMessage(
			"workbuddy", auth.RegionIntl, "deepseek-v4.1-flash",
			[]string{"glm-5.2", "glm-5.3"}),
		"modelNotInUpstreamMessage": modelNotInUpstreamMessage(
			qoder.RegionIntl, "deepseek-v4.1-flash", []string{"dfmodel", "qfmodel"}),
	}
	for name, msg := range msgs {
		if msg == "" {
			t.Errorf("%s 返回了空串（该文案是唯一归因来源，不能为空）", name)
			continue
		}
		assertNoAuthMisread(t, name, msg)
	}
}

// TestClientSafeMessageIsTheBackstop 闸门必须能兜住**上游原文里自带 403** 的情况。
//
// 为什么需要这一条：有两条文案是**原样转达上游原文**的
//（ErrEffortRejected 的 detail、ErrModelParamInvalid 的 extError.message），
// 这是刻意的 —— 用户要照着上游的话改参数。但上游那句话里**可能带状态码**
//（如 "HTTP 403: request rejected"），我们无法事先清洗。
//
// 这时闸门是唯一的防线，必须证明它真的兜得住。
func TestClientSafeMessageIsTheBackstop(t *testing.T) {
	// 上游原文里带 403 的思考档位拒绝（真实形状）。
	body := `{"error":{"message":"the reasoning effort value is not supported (HTTP 403)"}}`
	msg := upstream.FriendlyMessage(upstream.ErrEffortRejected, 400, body)
	if !strings.Contains(msg, "reasoning effort value is not supported") {
		t.Fatalf("前置契约失效：上游原文没有被保留，用户就拿不到「该换哪个档位」这条信息：%q", msg)
	}
	assertNoAuthMisread(t, "闸门兜住后的 ErrEffortRejected", clientSafeMessage(msg))

	// 上游原文里带 401 的参数拒绝。
	body401 := `{"code":11133,"extError":{"message":"rejected by provider (401)"}}`
	msg401 := upstream.FriendlyMessage(upstream.ErrModelParamInvalid, 400, body401)
	assertNoAuthMisread(t, "闸门兜住后的 ErrModelParamInvalid", clientSafeMessage(msg401))
}

// TestProtocolErrorBodiesNeverReadAsAuth 三个协议的**错误体构造器**必须都过闸门。
//
// 闸门装在构造器里（不是调用点），所以这条同时守住三件事：
//  1. 三个协议都真的接了闸门（漏一个就等于漏一条路）；
//  2. 流式 error 事件与普通错误走同一套构造器（Anthropic 的 error 事件、
//     Responses 的 response.failed 都复用）；
//  3. 构造器不会把 message 挪到别的字段上绕过闸门。
func TestProtocolErrorBodiesNeverReadAsAuth(t *testing.T) {
	const dirty = "上游拒绝（HTTP 403），已冷却该账号"

	rec := httptest.NewRecorder()
	writeOpenAIError(rec, http.StatusBadGateway, "no_healthy_account", dirty)
	assertNoAuthMisread(t, "OpenAI 错误体", rec.Body.String())

	assertNoAuthMisread(t, "Anthropic 错误体",
		mustJSONString(t, anthropicError(http.StatusBadGateway, "api_error", dirty)))

	assertNoAuthMisread(t, "Responses 错误体",
		mustJSONString(t, responsesError(http.StatusBadGateway, "upstream_error", dirty)))

	// 流式失败事件走的是各自 state 的方法，必须同样干净。
	assertNoAuthMisread(t, "Responses response.failed 事件",
		mustJSONString(t, newResponsesStreamState("glm-5.2").failedEvent(dirty)))

	assertNoAuthMisread(t, "Anthropic 流式 error 事件",
		mustJSONString(t, anthropicError(200, "upstream_error", dirty)))
}

// TestExhaustedProtocolBodiesNeverReadAsAuth 端到端：账号池耗尽时，
// **三个协议**对外回的错误体都不能被读成 AUTH。
//
// 与上一条的区别：那条测构造器（喂脏文案），这条测真实转发路径
//（脏文案来自上游 403/11140 的真实报文），证明"上游 403 → 客户端 AUTH"
// 这条因果链在三个入口上都被切断了。
func TestExhaustedProtocolBodiesNeverReadAsAuth(t *testing.T) {
	cases := []struct {
		name string
		path string
		body string
	}{
		{
			"chat/completions",
			"/v1/chat/completions",
			unusableChatBody(false),
		},
		{
			"messages",
			"/v1/messages",
			`{"model":"glm-5.2","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}`,
		},
		{
			"responses",
			"/v1/responses",
			`{"model":"glm-5.2","input":"hi"}`,
		},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			// 上游**永远**回 403/11140：三个协议都会走耗尽路径。
			up, _ := accountUnusableUpstream(t, func(string) (int, string, bool) {
				return 403, accountUnusableBody, false
			})
			h, _ := newUnusableTestHandler(t, up, 0, workBuddyAuth("only", "at-only"))

			req := httptest.NewRequest(http.MethodPost, c.path, strings.NewReader(c.body))
			req.Header.Set("Content-Type", "application/json")
			rec := httptest.NewRecorder()
			h.ServeHTTP(rec, req)

			if rec.Code == http.StatusUnauthorized || rec.Code == http.StatusForbidden {
				t.Fatalf("上游 403 漏成了鉴权类状态码 %d：%s", rec.Code, rec.Body.String())
			}
			assertNoAuthMisread(t, c.name+" 耗尽响应体", rec.Body.String())
			// 归因不能丢：用户要知道该去重登账号，而不是去改密钥。
			if !strings.Contains(rec.Body.String(), "11140") {
				t.Errorf("耗尽文案里没有 11140 归因：%s", rec.Body.String())
			}
		})
	}
}
