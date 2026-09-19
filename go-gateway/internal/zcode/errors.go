package zcode

// errors.go 上游错误的分类。
//
// ## 为什么需要分类
//
// 上游的错误都是 HTTP 401 + 一个数字码，但**含义完全不同**，
// 排查方向也完全不同：
//
//	1001  认证参数未收到   → 我们没带 Authorization（**我们的 bug**）
//	1000  认证失败         → 凭证无效/过期（**用户要换凭证**）
//	VERIFY_*  签名校验失败  → 上游开始要求客户端签名（**要补实现**）
//
// 若不分类，三者都会显示成"认证失败"，用户只会去反复换凭证 ——
// 而 1001 换多少次都没用（是我们没发头）。
//
// ## 实测来源
//
// 见 signing.go 的对照实验表：A 组（完全不带 Authorization）得到 1001，
// B/C/D/E 组得到 1000。这个区分是实测确认的，不是推测。

import (
	"encoding/json"
	"fmt"
	"strings"
)

// ErrKind 错误分类。
type ErrKind string

const (
	// ErrNone 未分类（网络错误、5xx、无法解析的响应）。
	ErrNone ErrKind = ""
	// ErrAuthMissing 认证参数未收到（1001）—— 我们没带 Authorization。
	//
	// 这几乎总是**网关自身的配置/代码问题**，不是用户凭证问题。
	// 报错文案必须点明这一点，否则用户会去反复换凭证（无用功）。
	ErrAuthMissing ErrKind = "auth_missing"
	// ErrAuthFailed 认证失败（1000）—— 凭证无效或已过期。
	//
	// 这是用户能处理的情况：换凭证或重新登录。
	ErrAuthFailed ErrKind = "auth_failed"
	// ErrSigningRequired 上游要求客户端签名（VERIFY_SIGNATURE_*）。
	//
	// 我们**刻意没实现签名**（见 signing.go），因为实测不需要。
	// 若真出现这个错误，说明上游改了规则 —— 报错必须明确说这件事，
	// 而不是笼统的"认证失败"（那会让排查方向完全跑偏）。
	ErrSigningRequired ErrKind = "signing_required"
	// ErrRateLimited 限流（429 或上游的限流码）。
	ErrRateLimited ErrKind = "rate_limited"
	// ErrQuotaExhausted 额度耗尽。
	ErrQuotaExhausted ErrKind = "quota_exhausted"
	// ErrModelNotFound 模型不存在（11102）。
	//
	// 实测最常见的原因是**模型名大小写不对**（上游严格区分）：
	// `GLM-5.3` 报此错，而正确写法是 `glm-5.3`。
	// 报错文案要点明这一点 —— 否则用户会以为"上游不支持这个模型"。
	ErrModelNotFound ErrKind = "model_not_found"
	// ErrProviderDown 上游不可用（5xx）。
	ErrProviderDown ErrKind = "provider_down"

	// ErrPlanRequired 该账号**没有 coding plan 资格**（上游 3101）。
	//
	// 实测（2026-09-19，uitest/probe-zcode-all-chat.cjs）：把客户端那套
	// 请求头补全后，套餐通道从 401 变成
	//
	//	HTTP 403 {"code":3101,"msg":"coding plan is required"}
	//
	// 403（而非 401）说明**鉴权已经通过** —— 是账号侧没有这个套餐的资格。
	// 换句话说这不是凭证错、也不是我们请求写错。
	//
	// 为什么单独成类：它的处置与"额度耗尽"完全不同 ——
	// 额度耗尽要充值，而这个要**换一个有套餐的账号**或去官方领套餐。
	// 混在一起报会让用户白充钱。
	ErrPlanRequired ErrKind = "plan_required"

	// ErrNoResourcePack 该账号**在这条通道上没有可用资源包**（上游 429 + 1113）。
	//
	// ## 为什么必须与"额度耗尽"分开（所有者实测踩到的误导）
	//
	// 上游原文：
	//
	//	HTTP 429 rate_limit_error [1113][余额不足或无可用资源包,请充值。]
	//
	// 我们此前把这类 429 一律归类成 `ErrQuotaExhausted`，界面上就说
	// 「账号额度已耗尽：请为该账号充值」。
	//
	// **但实测矛盾**：同一个账号的额度查询（`billing/balance`）明明显示
	// GLM-5.3-Flash 有 3 亿 token、几乎没动用（remaining=299999978）。
	//
	// 原因：**额度与资源包在不同通道上**。
	//
	//	billing/balance     → zcode.z.ai 的 zcode-plan 通道（能看到额度）
	//	coding/paas/v4      → 另一条通道（这 1113 是从这条回的）
	//
	// 所以"额度耗尽"是**错的诊断** —— 用户会去充值，而他的额度就在那儿。
	// 真实原因是这条通道上没有可用资源包（客户端自己把它标成
	// `systemDisabledReason = "coding_plan_not_entitled"`，与此吻合）。
	//
	// 处置也不同：
	//
	//	ErrQuotaExhausted → 充值 / 等额度恢复
	//	ErrNoResourcePack → 换通道（start-plan）或换一个有资格的账号；
	//	                    充值**不一定**有用
	ErrNoResourcePack ErrKind = "no_resource_pack"

	// ErrCaptchaRequired 该操作需要**人机验证码**（上游 3007）。
	//
	// 实测：`/api/v1/zcode-plan/...` 对话通道回
	//
	//	HTTP 400 {"code":3007,"msg":"captcha verify failed"}
	//
	// 而客户端源码里，这类请求要带
	// `X-Aliyun-Captcha-Verify-Param`（见 billing/claim 的实现）。
	//
	// ⚠ **刻意不实现绕过**：验证码是服务端的防滥用机制。
	// 如实告诉用户"需要在官方客户端里完成一次验证"，
	// 而不是想办法自动过硬。
	ErrCaptchaRequired ErrKind = "captcha_required"
)

// 上游业务码（实测确认，见 Classify 的注释）。
const (
	// CodeAuthMissing 没发认证头（**我们的 bug**，与凭证无关）。
	CodeAuthMissing = "1001"
	// CodeAuthFailed 凭证无效或已过期。
	CodeAuthFailed = "1000"
	// CodeQuotaExhausted 余额不足 / 无可用资源包。
	//
	// ⚠ 它配的是 **HTTP 429** —— 与限流同码。故业务码必须**先于**状态码判断，
	// 否则会把"去充值"误导成"稍后重试"。
	CodeQuotaExhausted = "1113"
	// CodeModelNotFound 模型不存在（**大小写敏感**）。
	CodeModelNotFound = "11102"
	// CodePlanRequired 没有 coding plan 资格（403）。
	//
	// 实测：补全客户端请求头后，套餐通道由 401 变 403 并带此码。
	CodePlanRequired = "3101"
	// CodeCaptchaRequired 需要人机验证码（400）。
	//
	// ⚠ 不实现绕过（防滥用机制）。见 ErrCaptchaRequired 的说明。
	CodeCaptchaRequired = "3007"
)

// Error 上游错误（带分类与原始响应）。
type Error struct {
	Kind   ErrKind
	Status int
	// Code 上游的业务码（如 "1000"）；可能为空。
	Code string
	// Msg 上游的文案（原样保留，供排障）。
	Msg string
}

func (e *Error) Error() string {
	if e == nil {
		return ""
	}
	parts := []string{fmt.Sprintf("HTTP %d", e.Status)}
	if e.Code != "" {
		parts = append(parts, "code="+e.Code)
	}
	if e.Msg != "" {
		parts = append(parts, e.Msg)
	}
	return strings.Join(parts, " ")
}

// FriendlyMessage 面向用户的错误说明。
//
// 每种分类给一句**能指导下一步动作**的话，而不是复述上游原文 ——
// 上游原文（如"身份验证失败。"）不告诉用户该做什么。
func (e *Error) FriendlyMessage() string {
	if e == nil {
		return "未知错误"
	}
	switch e.Kind {
	case ErrAuthMissing:
		return "网关没有向 ZCode 上游发送认证信息（上游返回「认证参数未收到」）。" +
			"这通常是网关配置问题而非凭证问题，请检查该账号的凭证是否为空。"
	case ErrAuthFailed:
		return "ZCode 凭证无效或已过期（上游返回「认证失败」）。" +
			"请在 ZCode 控制台重新生成凭证，或在账号页重新登录。"
	case ErrSigningRequired:
		return "ZCode 上游开始要求**客户端签名**（" + e.Msg + "）。" +
			"本网关出于实测结论未实现该签名（详见 internal/zcode/signing.go），" +
			"现在需要补上 —— 这是代码问题，不是凭证问题。"
	case ErrRateLimited:
		return "ZCode 上游限流（" + e.Msg + "）。稍后会自动重试其他账号。"
	case ErrQuotaExhausted:
		// 点明"要去充值"，而不是"稍后重试" —— 后者会让用户白等
		return "ZCode 账号额度已耗尽，请为该账号充值或更换账号（上游：" + e.Msg + "）"
	case ErrNoResourcePack:
		// ⚠ 这段文案**刻意不提"充值"**。实测该账号的 billing/balance
		// 显示额度充足（3 亿 token 几乎未用），而这条通道回 1113 ——
		// 说明问题在"这条通道没有可用资源包"，不在额度。
		//
		// 提"充值"会把用户引向一个解决不了问题的方向（我此前就是这么
		// 误导他的）。
		return "该 ZCode 账号在**这条通道上没有可用资源包**（上游：" + e.Msg + "）。" +
			"注意这**不一定是额度不足** —— 实测有账号额度查询显示数亿 token、几乎未用，" +
			"却仍回这个错，因为额度与资源包在不同通道上。" +
			"建议改用其他账号，或先在 ZCode 官方客户端里确认该账号的套餐资格。"
	case ErrModelNotFound:
		// 最常见的原因是**模型名大小写**（上游严格区分）——
		// 不点明的话，用户会以为"上游不支持这个模型"
		return "ZCode 上游没有这个模型（上游：" + e.Msg + "）。" +
			"注意模型名**区分大小写**（如 glm-5.3 与 GLM-5.3 在上游是两个不同的名字）"
	case ErrProviderDown:
		return "ZCode 上游暂时不可用（" + e.Msg + "）。稍后会自动重试。"
	case ErrPlanRequired:
		// 与"额度耗尽"要**分开说**：那个要充值，这个充值也没用
		return "该 ZCode 账号**没有 Coding Plan 资格**，因此套餐通道不可用" +
			"（上游：" + e.Msg + "）。请注意这不是凭证问题（凭证是有效的），" +
			"充值也解决不了 —— 需要换一个有套餐的账号，" +
			"或先在 ZCode 官方客户端里开通/领取套餐。"
	case ErrCaptchaRequired:
		// 如实说明，并明确我们**不绕过**验证码
		return "该 ZCode 操作需要**人机验证**（上游：" + e.Msg + "）。" +
			"出于对服务端防滥用机制的尊重，本网关**不会绕过验证码** —— " +
			"请在 ZCode 官方客户端里完成一次验证后重试。"
	default:
		if e.Msg != "" {
			return "ZCode 上游返回错误：" + e.Msg
		}
		return fmt.Sprintf("ZCode 上游返回 HTTP %d", e.Status)
	}
}

// Classify 按状态码与响应体分类。
//
// 判定顺序**很重要**（按特异性从高到低）：
//
//  1. 签名错误（VERIFY_*）—— 最特异，且含义与其它完全不同
//  2. 业务码（1001 / 1000 / 1113 / 11102）—— 实测确认的区分
//  3. HTTP 状态码兜底（429 / 5xx / 402）
//  4. 其余 → ErrNone（不猜测）
//
// ## ⚠ 业务码必须**先于** HTTP 状态码判断
//
// 实测（uitest/diag-zcode-upstream-error.cjs）：额度不足返回的是
//
//	HTTP 429  {"error":{"code":"1113","message":"余额不足或无可用资源包,请充值。"}}
//
// 若先按 429 判，会得到 `ErrRateLimited` → 界面提示"请求过于频繁，请稍后重试"
// —— **完全错误的方向**：用户会一直重试，而正确动作是去充值。
//
// 上游用 429 表示两种完全不同的情况（限流 / 余额不足），
// 只有业务码能区分。
func Classify(status int, body string) ErrKind {
	// 1. 签名相关（最特异）
	if IsVerifyFailure(body) {
		return ErrSigningRequired
	}

	code := extractCode(body)

	// 2. 业务码（**必须先于状态码**，见上面的说明）
	switch code {
	case CodeAuthMissing:
		// 「认证参数未收到」—— 我们没带 Authorization
		return ErrAuthMissing
	case CodeAuthFailed:
		// 「认证失败」—— 凭证无效
		return ErrAuthFailed
	case CodeQuotaExhausted:
		// 1113「余额不足或无可用资源包，请充值」
		//
		// ⚠ 归为 ErrNoResourcePack 而**不是** ErrQuotaExhausted。
		// 实测（2026-09-19）：同一个账号的 billing/balance 显示
		// GLM-5.3-Flash 有 3 亿 token、几乎没动，而 coding/paas/v4 通道
		// 回这个 1113 —— 说明**额度与资源包在不同通道上**。
		//
		// 报"额度已耗尽"会让用户去充值，而他的额度就在那儿。故细分为
		// "这条通道上没有可用资源包"，处置建议也不同（换通道/换账号）。
		return ErrNoResourcePack
	case CodeModelNotFound:
		// 「模型不存在」—— 多半是模型名不对（上游大小写敏感）
		return ErrModelNotFound
	case CodePlanRequired:
		// 「coding plan is required」—— 账号没有套餐资格
		//（403 而非 401 = 鉴权已过，是账号侧的事）
		return ErrPlanRequired
	case CodeCaptchaRequired:
		// 「captcha verify failed」—— 需要人机验证。
		// ⚠ 业务码判断必须在状态码之前：这里的 3007 配的是 HTTP 400，
		// 若不特判会掉进 default（无分类），界面上就是一句空洞的
		// "上游返回 HTTP 400"，用户完全不知道该怎么办。
		return ErrCaptchaRequired
	}

	// 3. 文案里的**额度信号**要先于状态码判 —— 见下面 429 的注释。
	//
	// 上游有时不给数字码，只给中文文案（"余额不足，请充值"），
	// 而它配的仍是 429。若先按状态码判，会得到"限流"这个错误方向。
	//
	// ⚠ 归为 ErrNoResourcePack 而不是 ErrQuotaExhausted —— 理由同上：
	// 实测有账号"额度充足但该通道无资源包"，报"额度耗尽"是误导。
	lower := strings.ToLower(body)
	if strings.Contains(body, "余额不足") || strings.Contains(body, "请充值") ||
		strings.Contains(lower, "insufficient") {
		return ErrNoResourcePack
	}

	// 4. HTTP 状态码兜底
	switch {
	case status == 429:
		return ErrRateLimited
	case status >= 500:
		return ErrProviderDown
	case status == 402:
		// 402 Payment Required：额度相关
		return ErrQuotaExhausted
	}

	// 5. 其余文案兜底（上游有时不返回数字码）
	switch {
	case strings.Contains(lower, "service info not found"):
		return ErrModelNotFound
	case strings.Contains(lower, "rate limit") || strings.Contains(lower, "too many"):
		return ErrRateLimited
	case strings.Contains(lower, "authentication failed") || strings.Contains(lower, "身份验证失败"):
		return ErrAuthFailed
	}

	return ErrNone
}

// NewError 构造分类后的错误。
func NewError(status int, body string) *Error {
	return &Error{
		Kind:   Classify(status, body),
		Status: status,
		Code:   extractCode(body),
		Msg:    extractMessage(body),
	}
}

// extractCode 从响应体里取业务码。
//
// 兼容两种形状（实测两种都存在）：
//
//	{"error":{"code":"1000","message":"Authentication Failed"}}
//	{"error":{"message":"Authentication Failed","type":"1000"}}
//
// 后者是 Anthropic 端点返回的形状（把码放在 type 字段）。
func extractCode(body string) string {
	var doc struct {
		Error struct {
			Code string `json:"code"`
			Type string `json:"type"`
		} `json:"error"`
		// 有些端点在顶层放 code
		Code any `json:"code"`
	}
	if err := json.Unmarshal([]byte(body), &doc); err != nil {
		return ""
	}
	if doc.Error.Code != "" {
		return doc.Error.Code
	}
	// Anthropic 形状：码在 type
	if doc.Error.Type != "" && isDigits(doc.Error.Type) {
		return doc.Error.Type
	}
	// 顶层 code（可能是数字）
	switch v := doc.Code.(type) {
	case string:
		return v
	case float64:
		return fmt.Sprintf("%.0f", v)
	}
	return ""
}

// extractMessage 从响应体里取可读文案。
func extractMessage(body string) string {
	var doc struct {
		Error struct {
			Message string `json:"message"`
			Msg     string `json:"msg"`
		} `json:"error"`
		Message string `json:"message"`
		Msg     string `json:"msg"`
	}
	if err := json.Unmarshal([]byte(body), &doc); err != nil {
		// 不是 JSON：截断原文（可能是 HTML 错误页）
		return truncate(body, 200)
	}
	for _, m := range []string{doc.Error.Message, doc.Error.Msg, doc.Message, doc.Msg} {
		if strings.TrimSpace(m) != "" {
			return strings.TrimSpace(m)
		}
	}
	return truncate(body, 200)
}

func isDigits(s string) bool {
	if s == "" {
		return false
	}
	for i := 0; i < len(s); i++ {
		if s[i] < '0' || s[i] > '9' {
			return false
		}
	}
	return true
}

func truncate(s string, n int) string {
	s = strings.TrimSpace(s)
	if len(s) <= n {
		return s
	}
	return s[:n] + "…"
}
