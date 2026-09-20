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
}

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
func (c *Client) streamChatOnce(ctx context.Context, cr *Cred, openAIBody []byte) (io.ReadCloser, int, []byte, error) {
	rawURL := cr.Provider.OpenAIBase() + ChatPath
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, rawURL, bytes.NewReader(openAIBody))
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
		return nil, resp.StatusCode, raw, nil
	}
	return resp.Body, resp.StatusCode, nil, nil
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

// appendCaptchaNote 在原始响应体里补一句可读说明（保留原 body 不动其结构）。
//
// 为什么是"拼接"而不是重新构造 JSON：上游 body 的形状随版本变，
// 我们**没有把握**解析它再序列化回等价的 JSON；而这段文字只是给人看的，
// 拼在后面既能被用户看到，又不会破坏调用方对原始 body 的判断。
func appendCaptchaNote(body []byte, reason string) []byte {
	note := fmt.Sprintf(
		"\n\n[网关说明] 上游要求人机验证，但**求解未成功**，因此没有重试。原因：%s\n"+
			"[网关说明] 若求解器组件缺失，请确认安装包内含 assets/zcode-captcha；"+
			"若是冷却/限流，稍后会自动恢复（求解器自身有冷却保护）。",
		reason)
	return append(append([]byte{}, body...), note...)
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
// ## 认证头的两种形状（按端点区分）
//
//	OpenAI 端点     Authorization: Bearer {credential}
//	Anthropic 端点  x-api-key: {credential}  +  Authorization: Bearer {credential}
//	                +  anthropic-version: 2023-06-01
//
// 本包**只走 OpenAI 端点**，故只发 Authorization。Anthropic 那套保留在
// 注释里，便于将来切换时不用重新逆向。
//
// ## 身份头是可选的
//
// 实测（见 signing.go 的对照实验）：不带身份头也能通过认证。
// 照发的理由是"让代理在指纹层与官方客户端不可区分"（参考实现的注释口径），
// 成本为零而"被风控识别为第三方代理"的代价可能很高。
//
// 但**不因缺头而失败** —— 头缺失只是少一层伪装。
func (c *Client) applyHeaders(req *http.Request, cr *Cred, stream bool) {
	req.Header.Set("Authorization", "Bearer "+cr.Credential)
	req.Header.Set("Content-Type", "application/json")
	if stream {
		req.Header.Set("Accept", "text/event-stream")
	} else {
		req.Header.Set("Accept", "application/json")
	}
	// 身份头（伪装成官方客户端）
	for k, v := range c.Identity.Headers() {
		if v != "" {
			req.Header.Set(k, v)
		}
	}
	// 追踪头：start-plan（JWT）通道**只发这三个**。
	//
	// 参考实现（zcode2api 的 identity.py）明确记载：
	//
	//	「通道差异（关键，**误发会触发上游 3012 "unusual activity"**）：
	//	  start-plan（JWT 通道）：只发 x-request-id / x-zcode-session-type /
	//	  x-zcode-trace-id 三个头，**不发** x-query-id / x-session-id。」
	//
	// 故这里只补三个；`x-query-id` / `x-session-id` **刻意不发**。
	for k, v := range c.Identity.TraceHeaders() {
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
