package zcode

// client.go 上游调用：对话 / 额度 / 模型清单。
//
// ## 为什么只走 OpenAI 端点
//
// 上游同时提供 OpenAI 与 Anthropic 两种端点，而网关**内部就是 OpenAI 格式**
//（`/v1/chat/completions` 直通，另两个协议入口先转成 OpenAI 再进来）。
//
// 走上游 OpenAI 端点 = **零翻译**。走 Anthropic 端点则要双向翻译
//（请求 + SSE 流），多出两个易错点却没有任何收益。
//
// 顺带的好处：响应是**标准 OpenAI 形状**，可以直接复用网关既有的
// `upstream.Stream` / `upstream.Aggregate`，不需要像 Qoder 那样写翻译层。

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"sort"
	"strings"
	"time"
)

// Client ZCode 上游客户端。
type Client struct {
	// HTTP 出站客户端。nil 时用 New() 的默认（含连接池与超时）。
	HTTP *http.Client
	// Timeout 短请求（额度 / 模型清单）的总时长上限。
	Timeout time.Duration
	// Identity 身份头（伪装成官方客户端）。见 identity.go。
	Identity Identity

	// anthropicBaseOverride 仅测试用：覆盖 Anthropic 端点基址。
	//
	// # 为什么必须留这个口子
	//
	// start-plan 通道的端点是**硬编码的** zcode.z.ai（那是上游权威定义，
	// 不该可配）。但测试**绝不能打真实上游** —— 既会消耗用户额度，
	// 也会实打实地触发风控（本项目已经因为高频测试吃过 3012）。
	//
	// 故留一个只在测试里调用的覆盖点，让集成测试能打到 httptest 服务器。
	anthropicBaseOverride string
}

// SetAnthropicBaseForTest 覆盖 Anthropic 端点基址（**仅测试用**）。
//
// 名字里带 ForTest 是刻意的：生产代码调用它就是 bug。
func (c *Client) SetAnthropicBaseForTest(base string) { c.anthropicBaseOverride = base }

// New 生产默认客户端。
//
// ## ⚠ 必须基于 `http.DefaultTransport`，**不能**手工构造 `&http.Transport{}`
//
// 这是一个**确定性**的坑（实测对照，见下表），不是网络波动：
//
//	配置                                        8 次真实请求
//	──────────────────────────────────────────  ────────────
//	A 手工 `&http.Transport{}`                    ✓ 8/8
//	B 手工 + ForceAttemptHTTP2=true               ✓ 8/8
//	C **`(&http.Transport{}).Clone()`**           ✗ **0/8 全失败**
//	D `http.DefaultTransport.Clone()`             ✓ 8/8
//	E 手工 + 显式禁用 h2（TLSNextProto 空 map）    ✓ 8/8
//
// 复现脚本：`uitest/diag-zcode-transport-compare.cjs`
//
// ## 为什么 C 会坏
//
// 手工构造的 `&http.Transport{}` 里 `ForceAttemptHTTP2` 是 **false**，
// 而 `http.DefaultTransport` 是 **true**。Go 的 h2 自动升级在
// `onceSetNextProtoDefaults()` 里按这些字段决定是否注册 h2 处理器；
// `Clone()` 会**触发**那个 once。两条路径的时机不同 → ALPN 声明了 h2
// 但处理器没注册 → 服务端发 h2 帧、客户端按 h1 解析：
//
//	net/http: HTTP/1.x transport connection broken:
//	malformed HTTP response "\x00\x00\x12\x04..."
//
// 那串 `\x00\x00\x12\x04` 是 HTTP/2 的 SETTINGS 帧（type=4）。这个错误
// 信息**完全不提协议版本**，极易被误判成"上游返回坏数据"或"网络波动" ——
// 我最初就误判了两次（先怪 TLSNextProto，又怪网络）。
//
// ## 所以：以 DefaultTransport 为模板 Clone
//
// 它带正确的 `ForceAttemptHTTP2` / 超时 / 连接池默认值，
// 我们只覆盖需要的几项。
func New() *Client {
	// 以标准 Transport 为模板 —— 见上面的对照表，这一条是必需的。
	tr := http.DefaultTransport.(*http.Transport).Clone()
	tr.MaxIdleConns = 100
	tr.MaxIdleConnsPerHost = 20
	tr.IdleConnTimeout = 90 * time.Second
	return &Client{
		HTTP: &http.Client{
			Transport: tr,
			// 短请求 60s；流式对话另走无总超时的 client（见 streamHTTP）。
			Timeout: 60 * time.Second,
		},
		Timeout:  30 * time.Second,
		Identity: DefaultIdentity(),
	}
}

func (c *Client) http() *http.Client {
	if c.HTTP != nil {
		return c.HTTP
	}
	return http.DefaultClient
}

func (c *Client) timeout() time.Duration {
	if c.Timeout > 0 {
		return c.Timeout
	}
	return 30 * time.Second
}

// streamHTTP 流式请求专用 client：复用 Transport 但不设总超时。
//
// 为什么不直接用 c.http()：它带 60s 总超时，长回答会在中途被掐断
//（表现为"回答被截断"）。
//
// ## ⚠ 这里 clone 的是**已经正确的** Transport，不要再手工构造
//
// 见 `New()` 的对照表：`(&http.Transport{}).Clone()` 会破坏 h2 的 ALPN
// 协商（8/8 全失败）。本方法 clone 的是 `New()` 里基于
// `http.DefaultTransport` 做出来的那个，其 `ForceAttemptHTTP2` 是对的，
// 故 clone 后仍然正确。
func (c *Client) streamHTTP() *http.Client {
	base := c.http()
	tr, ok := base.Transport.(*http.Transport)
	if !ok || tr == nil {
		return &http.Client{Transport: base.Transport}
	}
	clone := tr.Clone()
	clone.ResponseHeaderTimeout = 60 * time.Second
	return &http.Client{Transport: clone}
}

// ---------------------------------------------------------------------------
// 对话
// ---------------------------------------------------------------------------

// ChatPath 对话端点路径（拼在 OpenAIBase 后）。
const ChatPath = "/chat/completions"

// Anthropic Messages 协议的声明头（官方客户端实测值）。
//
//	anthropic-version: 2023-06-01
//	anthropic-beta:    mid-conversation-system-2026-04-07
//
// ⚠ 这两个**不是可选的**：Anthropic 协议要求 version，而 beta 声明
// "本客户端支持 mid-conversation system turn"。不发 beta 可能被上游
// 当成**旧版客户端**而走不同的兼容路径。
const (
	anthropicVersion = "2023-06-01"
	anthropicBeta    = "mid-conversation-system-2026-04-07"
)

// userAgentRuntimeSuffix User-Agent 的 runtime 声明段（官方实测）。
//
// 官方完整值：
//
//	ZCode/3.14.0 ai-sdk/provider-utils/4.0.27 runtime/node.js/24
//
// 而我们此前只发 `ZCode/3.14.0`。差别在于官方**声明了它是 AI SDK 的
// Node runtime** —— 风控据此区分"官方客户端发的"与"裸脚本发的"是
// 完全可能的。成本为零，照发。
//
// ⚠ 版本号（4.0.27 / node.js/24）是抓包时那一刻的值，会随客户端升级变化。
// 这里硬编码是**有意的取舍**：与其编造一个"看起来合理"的动态值，
// 不如用一个**真实且曾被上游接受过**的值。它过期后最多是回到
// "官方认为我们是稍旧的客户端"，而不会更糟。
const userAgentRuntimeSuffix = " ai-sdk/provider-utils/4.0.27 runtime/node.js/24"

// StreamChat 发起流式对话，返回上游原始流。
//
// openAIBody 是客户端发来的 OpenAI 请求体 —— **原样透传**（见文件头注释：
// 上游收的也是 OpenAI 格式，无需翻译）。
//
// 返回值语义与 qoder.Client.ChatStream 一致：
//
//	rc != nil              → 成功，调用方负责 Close
//	rc == nil, status >= 400 → 上游拒绝，respBody 是原始响应体
//
// # 验证码（captcha）—— 2026-09-20 补上的接线
//
// start-plan 通道对不带 `X-Aliyun-Captcha-Verify-Param` 的请求一律回
//
//	HTTP 400 {"code":3007,"msg":"captcha verify failed"}
//
// 求解器（`captcha.go` 的 `Solve()`）**早已实现且有 277 行测试**，
// 但**从来没有生产代码调用它** —— 全仓 `CaptchaParam` 的赋值次数为 0，
// 于是 `applyHeaders` 里那个 `if cr.CaptchaParam != ""` **永远为假**，
// 我们**从不发**这个头 ⇒ 必然 3007。
//
// 那是一个典型的「写了但没接线」缺陷：每一段单看都对，合起来是死的。
// 本函数现在负责接线。
//
// ⚠ 求解是**有成本**的（起一个 Node 进程、约 3 秒，且**会上游限流**：
// 连续求解若干次后求解器报 `[pe-stall]` 且需等待恢复）。
// 故只在**确实需要时**解（见下），不做"每次都解"。
func (c *Client) StreamChat(ctx context.Context, cr *Cred, openAIBody []byte) (io.ReadCloser, int, []byte, error) {
	if cr == nil || cr.Credential == "" {
		return nil, 0, nil, fmt.Errorf("账号没有凭证")
	}

	// 先试**不带**验证码：多数情况下不需要它，能省一次求解（以及一次限流风险）。
	rc, status, body, err := c.streamChatOnce(ctx, cr, openAIBody)
	if err != nil || status != http.StatusBadRequest {
		return rc, status, body, err
	}
	// 只有明确是「验证码缺失/失败」才去解 —— 其他 400（参数错、模型无权限）
	// 解了也没用，白白消耗一次求解配额。
	if !IsCaptchaRequiredBody(body) {
		return rc, status, body, err
	}

	param, serr := c.solveCaptcha(ctx)
	if serr != nil || param == "" {
		// 求解失败**如实返回原始 3007**，并把原因拼进响应体。
		//
		// ⚠ 不能静默返回原来的 body：用户会看到"captcha verify failed"
		// 而不知道**我们连求解都没成功**（组件没装 / 正在冷却 / 求解器受限流），
		// 那两种情况的处置完全不同（装组件 vs 等一会儿）。
		if serr != nil {
			return nil, status, appendCaptchaNote(body, serr.Error()), nil
		}
		return rc, status, body, err
	}

	// 用**一次性** param 重试。
	//
	// ⚠ 必须复制 Cred 再改：`cr` 是池里共享的账号对象，
	// 直接写它的 CaptchaParam 会让这个一次性值**残留**在账号上，
	// 下一次请求带着它必回 3007（一次性语义），表现为"偶发失败"。
	// 复制一份既干净又不会污染池状态。
	retry := *cr
	retry.CaptchaParam = param
	retry.CaptchaRegion = c.captchaRegion()
	return c.streamChatOnce(ctx, &retry, openAIBody)
}

// streamChatOnce 发一次请求（不涉及验证码求解）。
//
// # 2026-09-20：改走 **Anthropic Messages** 端点
//
// 此前打的是 `{OpenAIBase}/chat/completions`（按量计费通道），
// 实测**恒回** `429 {"code":1113,"msg":"余额不足或无可用资源包"}` ——
// 而账号明明有 300 万 + 500 万 token/日。
//
// 原因是**通道错**：额度挂在 start-plan 上，而按量计费通道没有该账号的
// 资源包。抓包实测官方客户端打的是：
//
//	POST https://zcode.z.ai/api/v1/zcode-plan/anthropic/v1/messages
//
// 且官方 provider 清单里**所有** provider 的 schema 都是 `anthropic`
//（含 coding-plan），`openai:chat` 只存在于 templateRules（自定义模板）。
//
// 故这里：OpenAI 请求体 → **翻译** → Anthropic 请求体；
// 上游返回的 Anthropic SSE → **翻译** → OpenAI SSE。
func (c *Client) streamChatOnce(ctx context.Context, cr *Cred, openAIBody []byte) (io.ReadCloser, int, []byte, error) {
	// 解析出模型名（翻译需要它），并把请求体翻成 Anthropic 形状。
	model := modelNameOf(openAIBody)
	// 会话元数据：官方每次对话都发（见 AnthropicMeta 的注释）。
	//
	// device_id 用凭证上的 DeviceMid（与 x-device-mid 同源，保持一致）；
	// session_id 用与追踪头同一个**稳定**会话标识 —— 两者必须一致，
	// 否则上游会看到"头部说会话 A、体里说会话 B"的不一致。
	sessID := stableUUID("zcode-session:" + firstNonEmptyStr(cr.AccountID, cr.UID))
	anthBody, err := BuildAnthropicBodyWithMeta(openAIBody, model, AnthropicMeta{
		DeviceID:  cr.DeviceMid,
		SessionID: sessID,
	})
	if err != nil {
		return nil, 0, nil, fmt.Errorf("翻译请求体失败: %w", err)
	}
	// ⚠ **强制流式**，即使客户端要的是非流式。
	//
	// 为什么：上游的流式与非流式响应形状**不同**（SSE 事件 vs 单个 message
	// 对象），若两条路都自己翻译，就有两套要同步维护的代码 + 两套测试。
	// 而网关的非流式路径本来就是"读完流再聚合"（见 forward.go 的
	// `productUp.Aggregate(rc, …)`）—— 即**它本来就期望拿到流**。
	//
	// 故这里统一按流式请求上游，非流式的聚合交给已有的 AggregateOpenAI
	//（它解析的正是我们翻译产物那种 OpenAI SSE）。一条路径，一处真相。
	anthBody = forceStream(anthBody)

	rawURL := c.anthropicMessagesURLFor(cr)
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, rawURL, bytes.NewReader(anthBody))
	if err != nil {
		return nil, 0, nil, err
	}
	c.applyHeaders(req, cr, true)

	resp, err := c.streamHTTP().Do(req)
	if err != nil {
		return nil, 0, nil, fmt.Errorf("连接 ZCode 上游失败: %w", err)
	}
	if resp.StatusCode >= 400 {
		raw, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
		resp.Body.Close()
		// ⚠ 错误体也可能是 Anthropic 形状 —— 翻成 OpenAI 形状再返回，
		// 否则调用方（server 层）解析不出错误信息，用户只看到"上游错误"。
		return nil, resp.StatusCode, NormalizeErrorBody(raw, resp.StatusCode), nil
	}
	// 上游是 Anthropic SSE → 翻成 OpenAI SSE。
	//
	// ⚠ 翻译是**流式**的（io.Pipe），不缓冲整个响应 ——
	// 缓冲会让"首字节延迟"变成"整段生成延迟"，用户看到长时间空白。
	return TranslateStream(resp.Body, model), resp.StatusCode, nil, nil
}

// forceStream 把请求体里的 `stream` 强制设为 true。
//
// 见 streamChatOnce 里"为什么强制流式"的说明。解析失败时原样返回
//（不该因为一个可选字段解析不动就让整个请求失败）。
func forceStream(body []byte) []byte {
	var m map[string]any
	if err := json.Unmarshal(body, &m); err != nil {
		return body
	}
	if v, ok := m["stream"].(bool); ok && v {
		return body
	}
	m["stream"] = true
	out, err := json.Marshal(m)
	if err != nil {
		return body
	}
	return out
}

// anthropicMessagesURL 拼 Anthropic Messages 端点。
//
// # 端点来源（**不硬编码**）
//
// 官方把"套餐 → 端点"的映射放在两个地方，优先级如下：
//
//	① `cdn-zcode.z.ai/zcode/config/zcode-builtin-23.json` 的 providerRules
//	   （账号级，最权威）：`account:*start-plan*` → zcode.z.ai/api/v1/zcode-plan/anthropic
//	② 默认回落 `StartPlanBase`
//
// 抓包实测的映射（providerRules）：
//
//	account:zai-start-plan              → https://zcode.z.ai/api/v1/zcode-plan/anthropic
//	account:bigmodel-start-plan         → 同上
//	account:zai-individual-coding-plan  → https://api.z.ai/api/anthropic
//	account:bigmodel-individual-coding-plan → https://open.bigmodel.cn/api/anthropic
//	account:*-offpeak-idle-plan         → https://zcode.z.ai/api/v1/off-peak/anthropic
//
// ⚠ 当前实现只用"start-plan 与否"做二分（够用且可验证），
// 完整映射表见 `uitest/ZCODE-对话请求权威规格.md`。
func anthropicMessagesURL(cr *Cred) string {
	base := StartPlanBase
	if cr != nil && !cr.isStartPlan() {
		// coding-plan（按量）通道：走服务商自己的 anthropic 端点。
		if b := cr.Provider.AnthropicBase(); b != "" {
			base = b
		}
	}
	return base + AnthropicMessagesPath
}

// anthropicMessagesURLFor 在 anthropicMessagesURL 之上叠加测试覆盖。
//
// 生产路径等于 anthropicMessagesURL；测试时被 SetAnthropicBaseForTest
// 指向 httptest —— 这样测试**不打真实上游**（不耗额度、不触风控）。
func (c *Client) anthropicMessagesURLFor(cr *Cred) string {
	if c != nil && c.anthropicBaseOverride != "" {
		return strings.TrimRight(c.anthropicBaseOverride, "/") + AnthropicMessagesPath
	}
	return anthropicMessagesURL(cr)
}

// NormalizeErrorBody 把上游的错误体统一成 OpenAI 的 error 形状。
//
// # 为什么必须做
//
// server 层的错误处理按 **OpenAI 形状**解析（`error.message`）。
// 上游返回的是 Anthropic 形状：
//
//	{"type":"error","error":{"type":"invalid_request_error","message":"..."}}
//	或 {"code":3007,"msg":"captcha verify failed"}
//
// 不转的话用户看到的是**空错误**或"未知错误"，而真正的原因
//（验证码/额度/参数）就藏在原文里 —— 那是最难排查的状态。
func NormalizeErrorBody(body []byte, status int) []byte {
	var m map[string]any
	if err := json.Unmarshal(body, &m); err != nil {
		// 不是 JSON（HTML 错误页等）：包一层，至少让用户看到原文片段
		msg := strings.TrimSpace(string(body))
		if len(msg) > 500 {
			msg = msg[:500] + "…"
		}
		b, _ := json.Marshal(map[string]any{
			"error": map[string]any{
				"message": msg,
				"type":    "upstream_error",
				"code":    status,
			},
		})
		return b
	}
	// 已是 OpenAI 形状 → 原样
	if _, ok := m["error"].(map[string]any); ok {
		return body
	}
	msg := ""
	typ := "upstream_error"
	// Anthropic 形状：{"type":"error","error":{"type":"…","message":"…"}}
	if e, ok := m["error"].(map[string]any); ok {
		msg = strOr(e["message"], "")
		if t := strOr(e["type"], ""); t != "" {
			typ = t
		}
	}
	// ZCode 业务码形状：{"code":3007,"msg":"captcha verify failed"}
	if msg == "" {
		msg = strOr(m["msg"], "")
	}
	// 有些是 {"error":{"code":…,"msg":…}} 或 {"message":"…"}
	if msg == "" {
		msg = strOr(m["message"], "")
	}
	if msg == "" {
		msg = strings.TrimSpace(string(body))
	}
	out := map[string]any{
		"error": map[string]any{
			"message": msg,
			"type":    typ,
			"code":    firstNonNil(m["code"], status),
		},
	}
	b, err := json.Marshal(out)
	if err != nil {
		return body
	}
	return b
}

// firstNonNil 返回第一个非 nil 的值（用于错误码回落）。
func firstNonNil(vs ...any) any {
	for _, v := range vs {
		if v != nil {
			return v
		}
	}
	return nil
}

// IsCaptchaRequiredBody 报告响应体是否是"需要验证码"（3007）。
func IsCaptchaRequiredBody(body []byte) bool {
	if len(body) == 0 {
		return false
	}
	s := string(body)
	// 上游两种形状都见过：`code:3007` 与中文/英文文案。
	// 用 code 为主判据（文案可能变），文案为辅。
	return strings.Contains(s, "3007") ||
		strings.Contains(s, "captcha verify failed") ||
		strings.Contains(s, "验证码")
}

// appendCaptchaNote 在**错误响应体**里补一句可读说明。
//
// # ⚠ 2026-09-20 修正：此前是"往 JSON 后面拼文本"，那会**破坏 JSON**
//
// 旧实现直接 `append(body, note...)`，产物形如：
//
//	{"error":{...}}
//	[网关说明] 求解未成功…
//
// 那是**非法 JSON** —— server 层的错误处理会解析失败，
// 于是用户看到的是"未知错误"，而我们精心写的说明反而**谁也看不到**。
// 这个缺陷是被 `TestStreamChatTranslatesErrorBody` 抓出来的：
// 它断言错误体必须是合法 JSON。
//
// 现在改成：**把说明放进 error.message**（结构不变，说明也能透出）。
func appendCaptchaNote(body []byte, reason string) []byte {
	note := fmt.Sprintf(
		"（网关附注：上游要求人机验证，但求解未成功，故未重试。原因：%s"+
			"；若为组件缺失请确认安装包含 assets/zcode-captcha，"+
			"若为冷却/限流则稍后会自动恢复）", reason)

	var m map[string]any
	if err := json.Unmarshal(body, &m); err == nil {
		if e, ok := m["error"].(map[string]any); ok {
			if msg, ok := e["message"].(string); ok && msg != "" {
				e["message"] = msg + " " + note
				if out, err := json.Marshal(m); err == nil {
					return out
				}
			}
		}
		// 没有 error.message 就补一个（保持结构合法）
		m["error"] = map[string]any{
			"message": note,
			"type":    "captcha_solve_failed",
		}
		if out, err := json.Marshal(m); err == nil {
			return out
		}
	}
	// 原 body 不是 JSON（HTML 错误页等）：包成 JSON 再附注
	b, err := json.Marshal(map[string]any{
		"error": map[string]any{
			"message": note + " 上游原始响应：" + strings.TrimSpace(string(body)),
			"type":    "captcha_solve_failed",
		},
	})
	if err != nil {
		return body
	}
	return b
}

// solveCaptcha 用共享求解器求一个 verifyParam。
//
// 求解器是**进程级共享**的（`SharedCaptchaSolver`），因为它自带冷却状态 ——
// 每个 Client 各持一个会让冷却形同虚设（上游限流是按我们的出口算的，
// 不是按实例算的）。
func (c *Client) solveCaptcha(ctx context.Context) (string, error) {
	s := SharedCaptchaSolver()
	if s == nil {
		return "", fmt.Errorf("求解器未初始化")
	}
	if reason := s.UnavailableReason(); reason != "" {
		return "", fmt.Errorf("求解器不可用：%s", reason)
	}
	return s.Solve(ctx)
}

// captchaRegion 验证码所属区域。
//
// 与 param 成对；实测缺它就是 3007（见 cred.go 的 CaptchaRegion 注释）。
// 取不到时回落 "cn"（本项目所有实测样本都是 cn）。
func (c *Client) captchaRegion() string {
	if s := SharedCaptchaSolver(); s != nil {
		if r := s.Region(); r != "" {
			return r
		}
	}
	return "cn"
}

// applyHeaders 写上游请求头。
//
// # 本函数已按**抓包实测的权威规格**对齐（2026-09-20）
//
// 此前这里的头是**推断**出来的，而且有一条推断被实测推翻。现在有官方客户端
// 成功请求的逐字抓包（Reqable，HTTP 200 + 流式内容），照它对齐。
//
// ## 与旧实现的四处差异（都是实测驱动，不是猜）
//
//	① **补 x-api-key**：官方同时发 authorization 与 x-api-key，**同值**（都是 JWT）。
//	   Anthropic 协议里 x-api-key 才是标准认证位置；只发 Authorization
//	   等于少了协议要求的头。
//
//	② **补 anthropic-version / anthropic-beta**：官方发
//	   `2023-06-01` 与 `mid-conversation-system-2026-04-07`。
//	   第二个是协议扩展声明 —— 不发可能被上游当成**旧版客户端**。
//
//	③ **User-Agent 要带 runtime 段**：官方是
//	   `ZCode/3.14.0 ai-sdk/provider-utils/4.0.27 runtime/node.js/24`
//	   而我们只发 `ZCode/3.14.0`。差别在于官方**声明了它是 AI SDK /
//	   Node runtime** —— 风控可能据此区分"官方客户端"与"裸脚本"。
//
//	④ **补 x-query-id / x-session-id**（见 TraceHeaders 的注释更正）。
//
// ## 认证头的形状
//
//	OpenAI 端点     Authorization: Bearer {credential}
//	Anthropic 端点  x-api-key: {jwt}  +  Authorization: Bearer {jwt}
//	                +  anthropic-version: 2023-06-01
//
// ⚠ 注意 Anthropic 端点用的是 **jwt**，不是 `{apiKey}.{secret}` 形态的
// `credential` —— 后者是按量计费通道（`open.bigmodel.cn/api/coding/paas/v4`）
// 用的。两者不能混（混了就是 `1113 余额不足或无可用资源包`）。
func (c *Client) applyHeaders(req *http.Request, cr *Cred, stream bool) {
	// ---- 认证：authorization 与 x-api-key 都发（官方实测同值）----
	//
	// ⚠ 用 **jwt**（start-plan 通道的凭证）。`cr.Credential` 是
	// `{apiKey}.{secret}` 形态，属于**另一条通道**；两者混用会回 1113。
	// 若该账号没有 jwt（只导入了 credential），则回落 credential ——
	// 那样至少能走按量通道，而不是一个头都不发。
	token := strings.TrimSpace(cr.JWT)
	if token == "" {
		token = strings.TrimSpace(cr.Credential)
	}
	if token != "" {
		req.Header.Set("Authorization", "Bearer "+token)
		req.Header.Set("X-Api-Key", token)
	}
	req.Header.Set("Content-Type", "application/json")
	if stream {
		req.Header.Set("Accept", "text/event-stream")
	} else {
		req.Header.Set("Accept", "application/json")
	}
	// ---- Anthropic 协议声明（官方必发）----
	req.Header.Set("Anthropic-Version", anthropicVersion)
	if !stream {
		// ⚠ beta 头只声明"我们支持 mid-conversation system"这个扩展。
		// 流式与非流式都发 —— 官方是流式抓的，非流式同协议同要求。
	}
	req.Header.Set("Anthropic-Beta", anthropicBeta)

	// 身份头（伪装成官方客户端）
	for k, v := range c.Identity.Headers() {
		if v != "" {
			req.Header.Set(k, v)
		}
	}
	// 追踪头（含 x-query-id / x-session-id）。
	//
	// ⚠ 这里要**带上账号标识**：`x-session-id` 与 `x-zcode-trace-id` 必须是
	// 会话级稳定的（抓包实测：官方相隔 58 分钟的两次请求，这两个值完全相同），
	// 而旧实现每请求随机 —— 那在高频请求下与脚本无异，很可能就是 3012 的真因。
	// Identity 是从配置构造的**共享值**，故这里做一次副本再填入账号信息，
	// 不改动 c.Identity 本身（那会让并发的不同账号互相覆盖）。
	id := c.Identity
	id.AccountID = firstNonEmptyStr(cr.AccountID, cr.UID)
	for k, v := range id.TraceHeaders() {
		if v != "" {
			req.Header.Set(k, v)
		}
	}
	// 验证码头（仅在启用且已求到 param 时）。
	//
	// ⚠ param 是**一次性**的：调用方每次请求都要现取一个（见 captcha.go）。
	if cr.CaptchaParam != "" {
		req.Header.Set("X-Aliyun-Captcha-Verify-Param", cr.CaptchaParam)
		if cr.CaptchaRegion != "" {
			req.Header.Set("X-Aliyun-Captcha-Verify-Region", cr.CaptchaRegion)
		}
	}
}

// ---------------------------------------------------------------------------
// 额度
// ---------------------------------------------------------------------------

// Quota 账号额度。
type Quota struct {
	// Remaining 剩余额度（各 balance 项求和）。
	Remaining int64
	// Total 总额度。
	Total int64
	// Used 已用额度。
	Used int64
	// Entries 明细（可能有多项，如"编码套餐"+"赠送"）。
	Entries []QuotaEntry
	// Plans 已生效的套餐（含**套餐整体到期** `ends_at`）。
	//
	// ⚠ 这个字段长期被我们忽略 —— 而体验套餐的到期时间就在里面。
	// 见 Plan 的说明。
	Plans []Plan
	// ServerTime 上游服务器时间（Unix 秒）；0 = 未提供。
	ServerTime int64
}

// PlanExpiry 套餐整体到期的**最早**时刻（Unix 秒）；0 = 无套餐或全部未知。
//
// 与 `SoonestExpiry()` 的区别（两者**不能混用**）：
//
//	SoonestExpiry()  各模型桶的**每日周期**结束（实测是当天 23:59:59）
//	PlanExpiry()     套餐整体的到期（实测 2026-09-23 23:59:59）
//
// 只取前者，用户会以为"明天额度就没了"；只取后者，他会以为
// "今天用不完就浪费了"。界面两个都要显示，且要说清各自含义。
func (q *Quota) PlanExpiry() int64 {
	if q == nil {
		return 0
	}
	var earliest int64
	for _, p := range q.Plans {
		if p.EndsAt <= 0 {
			continue
		}
		if earliest == 0 || p.EndsAt < earliest {
			earliest = p.EndsAt
		}
	}
	return earliest
}

// ActivePlan 取优先级最高的生效套餐（用于展示套餐名与说明）。
func (q *Quota) ActivePlan() *Plan {
	if q == nil {
		return nil
	}
	var best *Plan
	for i := range q.Plans {
		p := &q.Plans[i]
		// 只认 active；上游可能回别的状态（如 expired）
		if p.Status != "" && p.Status != "active" {
			continue
		}
		if best == nil || p.Priority > best.Priority {
			best = p
		}
	}
	return best
}

// QuotaEntry 单条额度明细。
type QuotaEntry struct {
	ShowName  string
	Remaining int64
	Total     int64
	Used      int64
	UnitType  string
	// ExpiresAt **每日周期**的结束时刻（Unix 秒）；0 = 未知。
	//
	// ⚠ 不是套餐到期。见 Quota.PlanExpiry 的说明。
	ExpiresAt int64
	// PeriodStart / PeriodEnd 该桶的计费周期（Unix 秒）。
	//
	// 与 ExpiresAt 通常相同，但上游两个都给了，故都留下 ——
	// 取哪个都不该猜。
	PeriodStart int64
	PeriodEnd   int64
	// PlanID / EntitlementID 该桶属于哪个套餐的哪个授权项。
	// 用于把桶与 Plan 对上（展示"这项来自体验套餐"）。
	PlanID        string
	EntitlementID string
	// GrantUnits 该桶的**每日赠送量**（来自套餐授权，token）。
	GrantUnits float64
}

// ErrNoJWT 表示该账号没有 JWT，无法查询额度。
//
// 单独成型是刻意的：界面据此显示"额度未知（需要 OAuth 登录）"，
// 而不是显示 0 —— 0 会被用户误读成"额度耗尽"。
//
// ⚠ 只有**缺 JWT** 才该走到这里。曾经的实现还有第二个原因（缺 deviceMid）
// 也会查不到，但那个是**我们自己能补的**，不该让用户看到"需要重新登录"。
var ErrNoJWT = fmt.Errorf("该账号没有 JWT（仅导入了凭证），无法查询额度；请在账号页重新登录")

// FetchQuota 查询额度。
//
// ## 两个必需条件（缺任一都查不到）
//
//  1. **JWT**（`zcodejwttoken`）—— billing 接口用它，不是 `{apiKey}.{secret}`。
//     故只导入了 Credential 的账号查不到额度 → 返回 ErrNoJWT。
//  2. **X-Device-Mid**（UUID 形态）—— 控制面硬要求，缺了回
//     `400 {"code":3001,"msg":"parameter error"}`。
//
// ⚠ 第 2 条是我最初漏掉的：我把那个 3001 误判成"只导入凭证所以查不到额度"，
// 于是界面上做了「额度未知」这个状态、并在文档里写成"需要重新登录"。
// **那个结论是错的** —— 补上这个头之后，只要有 JWT 就能查到完整额度
//（实测：3 亿 token 的活动计划，见 `probe-zcode-quota-auth2.cjs`）。
//
// 两个条件的区别在于**用户能否自己解决**：
//   · 缺 JWT    → 用户要在客户端里走一次 OAuth（我们无法代劳）
//   · 缺 DeviceMid → **我们自己补**（随机 UUID 即可，实测上游只校验格式）
func (c *Client) FetchQuota(ctx context.Context, cr *Cred) (*Quota, error) {
	if cr == nil {
		return nil, fmt.Errorf("账号为空")
	}
	if strings.TrimSpace(cr.JWT) == "" {
		return nil, ErrNoJWT
	}

	ctx, cancel := context.WithTimeout(ctx, c.timeout())
	defer cancel()

	// 参考实现的查询串：app_version + platform（billing 网关要求稳定指纹）
	q := fmt.Sprintf("/api/v1/zcode-plan/billing/balance?app_version=%s&platform=%s",
		c.Identity.AppVersion, c.Identity.PlatformArch())

	// ⚠ 用 **QuotaHost**，不是 BizHost。
	//
	// 实测两个服务商的额度查询都走 zcode.z.ai；用各自 BizHost 会得到
	// `HTTP 200 {"code":500,"msg":"404 NOT_FOUND"}` —— 那是 200，
	// 会被误当成"查到了但没额度"。详见 provider.go 的 QuotaHost 注释。
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, cr.Provider.QuotaHost()+q, nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Authorization", "Bearer "+cr.JWT)
	req.Header.Set("Accept", "application/json")
	// ⚠ 控制面**必须**带 X-Device-Mid（见 identity.go 的注释）。
	//
	// 漏发它上游回 `400 code=3001 parameter error` —— 那个错误看起来像
	// "参数写错了"，实际原因是缺这个头，很容易被误判成"这个账号查不到额度"。
	deviceMid := cr.DeviceMid
	if !IsUUID(deviceMid) {
		// 凭证文件里没有（或形态不对）时现场补一个，并写回让它稳定下来
		deviceMid = NewDeviceMid()
		cr.DeviceMid = deviceMid
	}
	for k, v := range c.Identity.ControlPlaneHeaders(deviceMid) {
		if v != "" {
			req.Header.Set(k, v)
		}
	}

	resp, err := c.http().Do(req)
	if err != nil {
		return nil, fmt.Errorf("查询 ZCode 额度失败: %w", err)
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if resp.StatusCode >= 400 {
		return nil, NewError(resp.StatusCode, string(raw))
	}

	var doc struct {
		Code int    `json:"code"`
		Msg  string `json:"msg"`
		Data struct {
			Balances []struct {
				ShowName    string `json:"show_name"`
				Remaining   any    `json:"remaining_units"`
				Total       any    `json:"total_units"`
				Used        any    `json:"used_units"`
				UnitType    string `json:"unit_type"`
				UnitTypeAlt string `json:"unitType"`
				ExpiresAt   any    `json:"expires_at"`
				ExpiresAlt  any    `json:"expiresAt"`
				// 以下三个是**套餐归属**：这个桶来自哪个套餐的哪个授权项。
				PlanID             string `json:"plan_id"`
				EntitlementID      string `json:"entitlement_id"`
				PeriodStart        any    `json:"period_start"`
				PeriodEnd          any    `json:"period_end"`
			} `json:"balances"`
			// Plans 已生效的套餐。
			//
			// ⚠ 这个字段长期被我们**完全忽略**，而体验套餐的整体到期
			//（`ends_at`）就在里面 —— 见 Plan 的说明。
			Plans []struct {
				UserPlanID  string  `json:"user_plan_id"`
				PlanID      string  `json:"plan_id"`
				Name        string  `json:"name"`
				Description string  `json:"description"`
				Priority    float64 `json:"priority"`
				Status      string  `json:"status"`
				StartsAt    any     `json:"starts_at"`
				EndsAt      any     `json:"ends_at"`
				// endsAt 的驼峰别名（上游两种写法都出现过）
				EndsAtAlt any `json:"endsAt"`
				Entitlements []struct {
					EntitlementID string   `json:"entitlement_id"`
					ShowName      string   `json:"show_name"`
					Meter         string   `json:"meter"`
					UnitType      string   `json:"unit_type"`
					Capabilities  []string `json:"capabilities"`
					GrantUnits    any      `json:"grant_units"`
					Period        string   `json:"period"`
					EffectiveAt   any      `json:"effective_at"`
				} `json:"entitlements"`
			} `json:"plans"`
			ServerTime int64 `json:"server_time"`
		} `json:"data"`
	}
	if err := json.Unmarshal(raw, &doc); err != nil {
		return nil, fmt.Errorf("额度响应解析失败: %w（原始内容：%s）", err, truncate(string(raw), 160))
	}
	// 业务码非 0 = 失败（上游用 HTTP 200 + code 报错）
	if doc.Code != 0 {
		return nil, &Error{Kind: Classify(resp.StatusCode, string(raw)), Status: resp.StatusCode,
			Code: fmt.Sprintf("%d", doc.Code), Msg: doc.Msg}
	}

	out := &Quota{ServerTime: doc.Data.ServerTime}

	// 先把套餐的「模型 → 每日赠送量」建成索引，供下面的余额项关联。
	// 这样界面上能说清"这 300 万是体验套餐送的"，而不是一个孤立的数字。
	type grantKey struct{ planID, entID string }
	grants := make(map[grantKey]float64)
	for _, p := range doc.Data.Plans {
		for _, e := range p.Entitlements {
			grants[grantKey{p.PlanID, e.EntitlementID}] = toFloat64(e.GrantUnits)
		}
	}

	for _, b := range doc.Data.Balances {
		k := grantKey{b.PlanID, b.EntitlementID}
		e := QuotaEntry{
			ShowName:  b.ShowName,
			Remaining: toInt64(b.Remaining),
			Total:     toInt64(b.Total),
			Used:      toInt64(b.Used),
			UnitType:  firstNonEmpty(b.UnitType, b.UnitTypeAlt),
			// 字段名 snake_case 与 camelCase 都接受（参考实现两种都读，
			// 实测两种都存在）
			ExpiresAt:   toInt64(firstAny(b.ExpiresAt, b.ExpiresAlt)),
			PeriodStart: toInt64(b.PeriodStart),
			PeriodEnd:   toInt64(b.PeriodEnd),
			PlanID:      b.PlanID,

			EntitlementID: b.EntitlementID,
			GrantUnits:    grants[k],
		}
		out.Entries = append(out.Entries, e)
		out.Remaining += e.Remaining
		out.Total += e.Total
		out.Used += e.Used
	}

	// 解析套餐 —— **这是本函数此前缺失的部分**。
	for _, p := range doc.Data.Plans {
		pl := Plan{
			UserPlanID:  p.UserPlanID,
			PlanID:      p.PlanID,
			Name:        p.Name,
			Description: p.Description,
			Priority:    p.Priority,
			Status:      p.Status,
			// ⚠ `ends_at` 是**套餐到期**（实测 2026-09-23 23:59:59），
			// 与余额项的 `expires_at`（每日周期结束，当天 23:59:59）不同。
			EndsAt:   toInt64(firstAny(p.EndsAt, p.EndsAtAlt)),
			StartsAt: toInt64(p.StartsAt),
		}
		for _, e := range p.Entitlements {
			pl.Entitlements = append(pl.Entitlements, PlanEntitlement{
				EntitlementID: e.EntitlementID,
				ShowName:      e.ShowName,
				Meter:         e.Meter,
				UnitType:      e.UnitType,
				Capabilities:  e.Capabilities,
				GrantUnits:    toFloat64(e.GrantUnits),
				Period:        e.Period,
				EffectiveAt:   toInt64(e.EffectiveAt),
			})
		}
		out.Plans = append(out.Plans, pl)
	}
	return out, nil
}

// Plan 一个已生效的套餐（`billing/balance` 的 `data.plans[]`）。
//
// # 为什么必须有这个结构（所有者的实测反馈）
//
//	「这个体验套餐是 9月23号23:59 过期时间，这是我刚登录的新账号赠送的
//	  额度，要区分好」
//
// 我们此前**只读 `balances`，完全忽略 `plans`** —— 而套餐级的到期时间
// （`ends_at`）就在 `plans` 里。实测（2026-09-19）：
//
//	plans[0].plan_id   = "zcode-v3-start-plan-0817"
//	plans[0].name      = "ZCode Start Plan"
//	plans[0].description = "免费 GLM 旗舰模型体验"
//	plans[0].status    = "active"
//	plans[0].starts_at = 1789781287 → 2026-09-19 09:28:07 (UTC+8)
//	plans[0].ends_at   = 1790179199 → **2026-09-23 23:59:59 (UTC+8)**  ← 与他说的一致
//
// ⚠ 注意 `balances[].expires_at` 是**每日周期**的结束（实测是当天 23:59:59），
// 而 `plans[].ends_at` 才是**套餐整体**的到期。两者语义不同：
//
//	balances[].period_end  = 2026-09-19 23:59:59  ← 今天结束，明天重置
//	plans[].ends_at        = 2026-09-23 23:59:59  ← 套餐到期，之后归零
//
// 只展示前者会让用户以为"明天就没了"；只展示后者会让他以为"今天用完就没了"。
// 两个都要，且要标清含义。
type Plan struct {
	UserPlanID string `json:"user_plan_id"`
	PlanID     string `json:"plan_id"`
	Name       string `json:"name"`
	// Description 上游给的说明，实测是"免费 GLM 旗舰模型体验"。
	Description string  `json:"description"`
	Priority    float64 `json:"priority"`
	// Status 实测 "active"。
	Status string `json:"status"`
	// StartsAt / EndsAt 套餐生效与**到期**（Unix 秒）。
	EndsAt   int64 `json:"ends_at"`
	StartsAt int64 `json:"starts_at"`
	// Entitlements 该套餐在各模型上的授权（含每日赠送量 grant_units）。
	Entitlements []PlanEntitlement `json:"entitlements"`
}

// PlanEntitlement 套餐在**某个模型**上的授权项。
type PlanEntitlement struct {
	EntitlementID string   `json:"entitlement_id"`
	ShowName      string   `json:"show_name"`
	Meter         string   `json:"meter"`
	UnitType      string   `json:"unit_type"`
	Capabilities  []string `json:"capabilities"`
	// GrantUnits 赠送量（token）。实测体验套餐是 GLM-5.3 300 万、
	// GLM-5.3-Flash 500 万 —— 与所有者说的一致。
	GrantUnits float64 `json:"grant_units"`
	// Period 重置周期，实测 "daily"。
	Period string `json:"period"`
	// EffectiveAt 生效时刻（Unix 秒）。
	EffectiveAt int64 `json:"effective_at"`
}

// SoonestExpiry 返回明细中最早的到期时刻（Unix 秒）；0 = 全部未知。
//
// 用途：跨产品路由的**到期分层**（先烧快过期的额度）。
func (q *Quota) SoonestExpiry() int64 {
	var earliest int64
	for _, e := range q.Entries {
		if e.ExpiresAt <= 0 {
			continue
		}
		if earliest == 0 || e.ExpiresAt < earliest {
			earliest = e.ExpiresAt
		}
	}
	return earliest
}

// ---------------------------------------------------------------------------
// 模型清单
// ---------------------------------------------------------------------------

// Model 一个可用模型。
type Model struct {
	ID            string `json:"id"`
	Name          string `json:"name"`
	ContextWindow int64  `json:"contextWindow"`
	MaxOutput     int64  `json:"maxOutputTokens"`
	Reasoning     bool   `json:"reasoning"`
	// Vision 是否视觉模型。
	//
	// ⚠ 必须来自上游的 `capabilities.vision`，**不能用"id 里含 v"的启发式**。
	//
	// 实测（2026-09-19，uitest/probe-zcode-models.cjs）：上游当前返回的
	// `GLM-5.3-Flash` 带 `capabilities.vision=true`，而它的 id 里**没有 v** ——
	// 启发式会漏判，用户会以为模型不支持图片。
	//
	// （参考实现用的就是启发式，`routes-openai.ts` 的注释自认是 heuristic。）
	Vision bool `json:"vision"`
	// Source 该模型信息的来源（"upstream" / "builtin"）。
	//
	// 为什么记录来源：本项目既有约定是**不硬编码模型清单**。内置清单只是
	// 离线兜底，界面应能区分"上游真值"与"内置兜底"，否则清单过期时无人知道。
	Source string `json:"source"`
}

// ConfigPath 上游**免认证**的客户端配置端点（提供模型清单）。
//
// 实测（2026-09-19）：`GET https://zcode.z.ai/api/v1/client/configs` 返回 200，
// **无需任何认证**，含：
//
//	data.builtinModels[]   {modelId, name, contextWindow, maxCompletionTokens,
//	                        capabilities.vision, reasoning.defaultLevel}
//	data.builtinProviders[] {id, schema, baseUrl, models[]}
//
// ## 为什么用这个而不是硬编码
//
// 实测它只返回 **2 个模型**（GLM-5.3 / GLM-5.3-Flash），而参考实现硬编码了
// **11 个** —— 那 9 个是过时的。硬编码会让用户看到一堆用不了的模型。
//
// ## 为什么不用 `{base}/models`
//
// 那个端点存在（假凭证返回 401，说明活着），但**需要认证**；
// 而本接口免认证，且在拿到凭证前（登录阶段）就能用来展示可用模型。
const ConfigPath = "/api/v1/client/configs"

// ConfigOrigin 免认证配置端点的 origin。
const ConfigOrigin = "https://zcode.z.ai"

// FetchModels 拉取模型清单。
//
// ## ⚠ 两个来源的语义**不同**，必须合并（实测踩过坑）
//
//	来源                              给出什么                        大小写
//	────────────────────────────────  ──────────────────────────────  ────────
//	{provider}/api/coding/paas/v4/models  **API 真正接受的 model 值**    小写 glm-5.3
//	zcode.z.ai/api/v1/client/configs      ZCode **客户端**的展示信息     大写 GLM-5.3
//
// 我第一版只用了 `client/configs`，于是把客户端的展示名当成了 API 的模型名，
// 实测被上游拒绝：
//
//	400 {"code":11102,"msg":"model [GLM-5.3] service info not found"}
//
// **上游严格区分大小写** —— 展示名不能当请求参数用。
//
// ## 合并策略
//
//	① 以**账号端点的 /models** 为权威 ID 来源（那是 API 接受的值）
//	② 用 client/configs 的元信息（上下文窗口、视觉、推理）**按大小写不敏感**去补
//	③ 账号端点拿不到时，退化为只用 client/configs（并**统一转小写**，
//	   因为实测 API 接受的是小写）
func (c *Client) FetchModels(ctx context.Context, cr *Cred) ([]Model, error) {
	// 元信息（可能拿不到，不致命）
	meta := map[string]Model{}
	if ms, err := c.fetchConfigModels(ctx); err == nil {
		for _, m := range ms {
			meta[strings.ToLower(m.ID)] = m
		}
	}

	// ① 权威 ID 来源：账号自己的端点
	if cr != nil && cr.Credential != "" {
		if models, err := c.fetchProviderModels(ctx, cr); err == nil && len(models) > 0 {
			out := make([]Model, 0, len(models))
			for _, m := range models {
				id := strings.ToLower(strings.TrimSpace(m.ID))
				if id == "" {
					continue
				}
				if b, ok := meta[id]; ok {
					// 用端点给的 **id 原值**（保持 API 接受的大小写），
					// 元信息只补能力字段
					b.ID = m.ID
					b.Source = "upstream"
					out = append(out, b)
					continue
				}
				out = append(out, Model{ID: m.ID, Name: m.ID, Source: "upstream"})
			}
			if len(out) > 0 {
				return out, nil
			}
		}
	}

	// ② 退化为 client/configs（统一转小写 —— 实测 API 接受小写）
	if len(meta) > 0 {
		out := make([]Model, 0, len(meta))
		for _, m := range meta {
			m.ID = strings.ToLower(m.ID)
			m.Name = firstNonEmpty(m.Name, m.ID)
			m.Source = "upstream"
			out = append(out, m)
		}
		sort.Slice(out, func(i, j int) bool { return out[i].ID < out[j].ID })
		return out, nil
	}

	return nil, fmt.Errorf("拉取模型清单失败（账号端点与 config 端点都不可用）")
}

// fetchConfigModels 从免认证的 client/configs 拉模型清单。
func (c *Client) fetchConfigModels(ctx context.Context) ([]Model, error) {
	ctx, cancel := context.WithTimeout(ctx, c.timeout())
	defer cancel()

	req, err := http.NewRequestWithContext(ctx, http.MethodGet, ConfigOrigin+ConfigPath, nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Accept", "application/json")
	// 这个端点免认证，但带上身份头更接近官方客户端行为
	for k, v := range c.Identity.Headers() {
		if v != "" {
			req.Header.Set(k, v)
		}
	}

	resp, err := c.http().Do(req)
	if err != nil {
		return nil, fmt.Errorf("拉取 ZCode 客户端配置失败: %w", err)
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 4<<20))
	if resp.StatusCode >= 400 {
		return nil, NewError(resp.StatusCode, string(raw))
	}

	var doc struct {
		Code int `json:"code"`
		Data struct {
			BuiltinModels []struct {
				ModelID             string `json:"modelId"`
				Name                string `json:"name"`
				ContextWindow       int64  `json:"contextWindow"`
				MaxCompletionTokens int64  `json:"maxCompletionTokens"`
				Capabilities        struct {
					Vision bool `json:"vision"`
				} `json:"capabilities"`
				Reasoning *struct {
					DefaultLevel string `json:"defaultLevel"`
				} `json:"reasoning"`
			} `json:"builtinModels"`
		} `json:"data"`
	}
	if err := json.Unmarshal(raw, &doc); err != nil {
		return nil, fmt.Errorf("客户端配置解析失败: %w（原始内容：%s）", err, truncate(string(raw), 160))
	}
	if doc.Code != 0 {
		return nil, fmt.Errorf("客户端配置返回错误码 %d", doc.Code)
	}

	out := make([]Model, 0, len(doc.Data.BuiltinModels))
	for _, m := range doc.Data.BuiltinModels {
		id := strings.TrimSpace(m.ModelID)
		if id == "" {
			continue
		}
		out = append(out, Model{
			ID:            id,
			Name:          firstNonEmpty(m.Name, id),
			ContextWindow: m.ContextWindow,
			MaxOutput:     m.MaxCompletionTokens,
			// 视觉以 capabilities.vision 为准（不用启发式，见 Model.Vision 的注释）
			Vision:   m.Capabilities.Vision,
			Reasoning: m.Reasoning != nil,
			Source:   "upstream",
		})
	}
	if len(out) == 0 {
		return nil, fmt.Errorf("客户端配置里没有模型（builtinModels 为空）")
	}
	return out, nil
}

// fetchProviderModels 从账号自己端点的 /models 拉清单（备选路径）。
func (c *Client) fetchProviderModels(ctx context.Context, cr *Cred) ([]Model, error) {
	ctx, cancel := context.WithTimeout(ctx, c.timeout())
	defer cancel()

	rawURL := cr.Provider.OpenAIBase() + ModelListPath
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, rawURL, nil)
	if err != nil {
		return nil, err
	}
	c.applyHeaders(req, cr, false)

	resp, err := c.http().Do(req)
	if err != nil {
		return nil, fmt.Errorf("拉取 ZCode 模型清单失败: %w", err)
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if resp.StatusCode >= 400 {
		return nil, NewError(resp.StatusCode, string(raw))
	}

	var doc struct {
		Data []struct {
			ID string `json:"id"`
		} `json:"data"`
	}
	if err := json.Unmarshal(raw, &doc); err != nil {
		return nil, fmt.Errorf("模型清单解析失败: %w（原始内容：%s）", err, truncate(string(raw), 160))
	}
	if len(doc.Data) == 0 {
		return nil, fmt.Errorf("上游模型清单为空（原始内容：%s）", truncate(string(raw), 160))
	}

	// 用内置清单补齐能力字段（该端点通常只给 id）
	builtin := builtinModelIndex()
	out := make([]Model, 0, len(doc.Data))
	for _, m := range doc.Data {
		if strings.TrimSpace(m.ID) == "" {
			continue
		}
		if b, ok := builtin[strings.ToLower(m.ID)]; ok {
			b.Source = "upstream"
			out = append(out, b)
			continue
		}
		// 上游有但我们清单里没有的新模型：保守默认值 + 标注来源，
		// 而不是丢弃它（丢弃会让用户"看不到自己套餐里的新模型"）。
		out = append(out, Model{ID: m.ID, Name: m.ID, Source: "upstream"})
	}
	return out, nil
}

// ModelListPath 账号端点的模型清单路径（备选路径用）。
const ModelListPath = "/models"

// BuiltinModels 内置模型清单（**离线兜底**，非真值来源）。
//
// 来源：参考实现的 `provider/models.ts`（硬编码 11 个）。
//
// ⚠ **这是兜底，不是权威来源。** 实测（2026-09-19）上游 `client/configs`
// 当前只返回 **2 个模型**（GLM-5.3 / GLM-5.3-Flash），这 11 个里的大多数
// 已经过时。仅在上游接口不可用时使用，且 Source 标为 "builtin"
// 让界面能区分。
//
// 视觉标记沿用参考实现（`glm-4.6v` / `glm-5v-turbo`）—— 但注意那是**启发式**
// 的结果，实测上游的真实视觉模型是 `GLM-5.3-Flash`（id 里没有 v）。
// 这进一步说明：**不要依赖内置清单**，它只是网络不通时的降级显示。
func BuiltinModels() []Model {
	out := make([]Model, 0, len(builtinModels))
	for _, m := range builtinModels {
		m.Source = "builtin"
		out = append(out, m)
	}
	return out
}

func builtinModelIndex() map[string]Model {
	idx := make(map[string]Model, len(builtinModels))
	for _, m := range builtinModels {
		idx[strings.ToLower(m.ID)] = m
	}
	return idx
}

// builtinModels 内置清单（来源：参考实现 provider/models.ts）。
var builtinModels = []Model{
	{ID: "glm-4.5-air", Name: "GLM 4.5 Air", ContextWindow: 131072, MaxOutput: 98304, Reasoning: true},
	{ID: "glm-4.6", Name: "GLM 4.6", ContextWindow: 200000, MaxOutput: 131072, Reasoning: true},
	{ID: "glm-4.6v", Name: "GLM 4.6V", ContextWindow: 131072, MaxOutput: 32768},
	{ID: "glm-4.7", Name: "GLM 4.7", ContextWindow: 200000, MaxOutput: 131072, Reasoning: true},
	{ID: "glm-5", Name: "GLM 5", ContextWindow: 200000, MaxOutput: 64000, Reasoning: true},
	{ID: "glm-5-turbo", Name: "GLM 5 Turbo", ContextWindow: 200000, MaxOutput: 64000, Reasoning: true},
	{ID: "glm-5v-turbo", Name: "GLM 5V Turbo", ContextWindow: 200000, MaxOutput: 131072},
	{ID: "glm-5.1", Name: "GLM 5.1", ContextWindow: 200000, MaxOutput: 64000, Reasoning: true},
	{ID: "glm-5.2", Name: "GLM 5.2", ContextWindow: 1000000, MaxOutput: 128000, Reasoning: true},
	{ID: "glm-5.3", Name: "GLM 5.3", ContextWindow: 1000000, MaxOutput: 128000, Reasoning: true},
	{ID: "glm-5.3-flash", Name: "GLM 5.3 Flash", ContextWindow: 1000000, MaxOutput: 128000, Reasoning: true},
}

// ---------------------------------------------------------------------------
// 非流式聚合
// ---------------------------------------------------------------------------

// AggregateOpenAI 把标准 OpenAI SSE 流聚合成一个 chat.completion 响应。
//
// ## 为什么本包要实现它（而不复用 upstream.Aggregate）
//
// server 包的 `ProductUpstream` 接口要求每个产品**自洽** ——
// 它不能反向依赖 `upstream` 包（那会让"产品适配"与"WorkBuddy 实现"耦合）。
//
// ## 为什么这么简单（对比 Qoder）
//
// ZCode 上游的响应**本来就是标准 OpenAI 格式**（这是选 OpenAI 端点的收益），
// 故这里只需把 delta 拼起来，不需要像 Qoder 那样先解开嵌套的 `body` 字段。
func AggregateOpenAI(r io.Reader, model string) (map[string]any, error) {
	var content, reasoning strings.Builder
	finish := ""
	id := ""
	created := int64(0)
	modelOut := ""

	br := bufio.NewReaderSize(r, 64*1024)
	for {
		line, err := br.ReadString('\n')
		trimmed := strings.TrimSpace(line)
		if strings.HasPrefix(trimmed, "data:") {
			payload := strings.TrimSpace(strings.TrimPrefix(trimmed, "data:"))
			if payload == "[DONE]" {
				break
			}
			if payload != "" {
				var chunk struct {
					ID      string `json:"id"`
					Created int64  `json:"created"`
					Model   string `json:"model"`
					Choices []struct {
						Delta struct {
							Content          string `json:"content"`
							ReasoningContent string `json:"reasoning_content"`
						} `json:"delta"`
						Message struct {
							Content          string `json:"content"`
							ReasoningContent string `json:"reasoning_content"`
						} `json:"message"`
						FinishReason string `json:"finish_reason"`
					} `json:"choices"`
				}
				if json.Unmarshal([]byte(payload), &chunk) == nil {
					if chunk.ID != "" {
						id = chunk.ID
					}
					if chunk.Created != 0 {
						created = chunk.Created
					}
					if chunk.Model != "" {
						modelOut = chunk.Model
					}
					if len(chunk.Choices) > 0 {
						c := chunk.Choices[0]
						// delta 是流式增量；message 是非流式整体（有些实现两者都给）
						content.WriteString(firstNonEmpty(c.Delta.Content, c.Message.Content))
						reasoning.WriteString(firstNonEmpty(c.Delta.ReasoningContent, c.Message.ReasoningContent))
						if c.FinishReason != "" {
							finish = c.FinishReason
						}
					}
				}
			}
		}
		if err != nil {
			break // io.EOF 或读错误：已读到的内容仍返回
		}
	}

	if finish == "" {
		finish = "stop"
	}
	if id == "" {
		id = "chatcmpl-zcode"
	}
	if created == 0 {
		created = time.Now().Unix()
	}
	if modelOut == "" {
		modelOut = model
	}

	msg := map[string]any{"role": "assistant", "content": content.String()}
	// reasoning_content 只在非空时给：空字符串会让部分客户端显示空的思考块
	if reasoning.Len() > 0 {
		msg["reasoning_content"] = reasoning.String()
	}
	return map[string]any{
		"id":      id,
		"object":  "chat.completion",
		"created": created,
		"model":   modelOut,
		"choices": []any{map[string]any{
			"index":         0,
			"message":       msg,
			"finish_reason": finish,
		}},
	}, nil
}

// modelOf 从 OpenAI 请求体里取 model 字段。
func modelOf(body []byte) string {
	var b struct {
		Model string `json:"model"`
	}
	if err := json.Unmarshal(body, &b); err != nil {
		return ""
	}
	return b.Model
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

// toInt64 把上游可能返回的多种数值形态转成 int64。
//
// 上游有时返回数字、有时返回字符串（如 "1234"）。只认数字会让额度
// 静默变成 0 —— 而 0 会被误读成"额度耗尽"。
func toInt64(v any) int64 {
	switch x := v.(type) {
	case float64:
		return int64(x)
	case int64:
		return x
	case int:
		return int64(x)
	case string:
		var n float64
		if _, err := fmt.Sscanf(strings.TrimSpace(x), "%f", &n); err == nil {
			return int64(n)
		}
	case json.Number:
		if n, err := x.Int64(); err == nil {
			return n
		}
	}
	return 0
}

// toFloat64 与 toInt64 同理，但保留小数。
//
// 为什么单独要一个：`grant_units` 这类**赠送量**可能带小数
//（参考实现 zcode2api 就用 `float(units)` 读它）。
// 用 toInt64 会截断，而额度数字被截断会让"总量对不上赠送量"，
// 进而把体验套餐误判成付费套餐。
func toFloat64(v any) float64 {
	switch x := v.(type) {
	case float64:
		return x
	case int64:
		return float64(x)
	case int:
		return float64(x)
	case string:
		var n float64
		if _, err := fmt.Sscanf(strings.TrimSpace(x), "%f", &n); err == nil {
			return n
		}
	case json.Number:
		if n, err := x.Float64(); err == nil {
			return n
		}
	}
	return 0
}

func firstAny(vals ...any) any {
	for _, v := range vals {
		if v == nil {
			continue
		}
		switch x := v.(type) {
		case string:
			if strings.TrimSpace(x) != "" {
				return v
			}
		case float64:
			if x != 0 {
				return v
			}
		default:
			return v
		}
	}
	return nil
}
