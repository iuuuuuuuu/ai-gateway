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
	case ErrModelNotFound:
		// 最常见的原因是**模型名大小写**（上游严格区分）——
		// 不点明的话，用户会以为"上游不支持这个模型"
		return "ZCode 上游没有这个模型（上游：" + e.Msg + "）。" +
			"注意模型名**区分大小写**（如 glm-5.3 与 GLM-5.3 在上游是两个不同的名字）"
	case ErrProviderDown:
		return "ZCode 上游暂时不可用（" + e.Msg + "）。稍后会自动重试。"
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
		// 「余额不足或无可用资源包」—— 用户要去充值
		return ErrQuotaExhausted
	case CodeModelNotFound:
		// 「模型不存在」—— 多半是模型名不对（上游大小写敏感）
		return ErrModelNotFound
	}

	// 3. 文案里的**额度信号**要先于状态码判 —— 见下面 429 的注释。
	//
	// 上游有时不给数字码，只给中文文案（"余额不足，请充值"），
	// 而它配的仍是 429。若先按状态码判，会得到"限流"这个错误方向。
	lower := strings.ToLower(body)
	if strings.Contains(body, "余额不足") || strings.Contains(body, "请充值") ||
		strings.Contains(lower, "insufficient") || strings.Contains(lower, "quota") {
		return ErrQuotaExhausted
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
