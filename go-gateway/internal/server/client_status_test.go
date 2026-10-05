package server

// client_status_test.go 「上游 403 不能被客户端读成『API 密钥无效』」的回归测试。
//
// # 守的是所有者实测报告的一个误导性缺陷（2026-09-20）
//
// 所有者原话：
//
//	「密钥明明是正确的，不知道怎么对话过程中就提示『本轮运行失败 API 密钥无效』，
//	  而且我还是可以通过我配置的密钥获取到模型的名称」
//
// 查证：**密钥从头到尾都是对的**。
//
// # ★ 判据更正（2026-09-29，此前这里写错了）
//
// 本文件原先声称客户端是按**状态码**判的：
//
//	if (status === 401 || status === 403) return "AUTH";   // dsh-llm-deepseek
//
// **那条路径不生效**：DSH 里 `ai` provider 的 namespace 是 `llm-pi-ai`
//（见 `dsh-config-manager` 的 providers.json），走的是 pi-ai 适配层。
// 真正的判据是**文案文本正则**（`dsh-llm-pi-ai/lib/index.js:1376-1387`）：
//
//	function classifyPiAiError(message) {
//	  if (/\b(?:401|403)\b/.test(message)) return "AUTH";
//	  ...
//	}
//
// 错误文案由 `pi-ai/dist/utils/error-body.js:111-118` 的 `formatProviderError`
// 把 `(status): body` **原样拼进 message** 得到。
// ⇒ **只要文案里出现裸 "401"/"403"，HTTP 状态码是什么都无所谓，一律判 AUTH。**
//
// 于是：上游 403（排队/额度/风控/WAF）→ 文案里带 "403" → 界面说"密钥无效"。
// 用户会去**反复改密钥**，而真正原因是上游拒绝 —— 最坏的一类误导。
//
// # 为什么本文件只测状态码、文案另有一处
//
// 状态码归一是**必要但不充分**的防线（挡住按状态码判的分支）。
// 真正兜住 DSH 的是文案闸门 `clientSafeMessage`（clienttext.go），
// 其回归断言在 `clienttext_test.go`。两者都改掉才算修完 ——
// 上一版只做了状态码归一，所有者实测**仍然**报「API 密钥无效」。
//
// # 为什么用单测
//
// 复现需要"上游恰好返回 403"这一外部条件（且会真打上游），
// 而状态码判据是一个纯函数映射，单测能精确锁定，零风险。
import (
	"net/http"
	"testing"
)

// TestUpstream403Becomes502 上游 403 必须被改写成 502。
//
// 这是本缺陷的**核心断言**：若这里回 403，客户端就会显示"API 密钥无效"。
func TestUpstream403Becomes502(t *testing.T) {
	if got := clientFacingStatus(http.StatusForbidden); got != http.StatusBadGateway {
		t.Fatalf("上游 403 必须改写成 502（否则客户端读成「API 密钥无效」，"+
			"而真实原因是上游拒绝）；实际 %d", got)
	}
}

// TestAuth401Stays401 **我们自己的鉴权 401 必须保持 401**。
//
// 这条防"修过头"：401 提示"密钥无效"是**正确**的行为
//（用户确实没带对密钥），不能被这次改动抹掉。
func TestAuth401Stays401(t *testing.T) {
	if got := clientFacingStatus(http.StatusUnauthorized); got != http.StatusUnauthorized {
		t.Errorf("鉴权失败的 401 必须保持 401（那种情况下提示密钥无效是对的）；实际 %d", got)
	}
}

// TestNo403LeaksToClient 扫一遍常见状态码，确认**没有任何一个**会变成 403。
//
// 403 在客户端被硬编码成 AUTH，故我们不该主动送出 403。
// 用表驱动而非只测 403 一个值：将来若有人加别的映射，
// 这条能挡住"顺手引入 403"的改动。
func TestNo403LeaksToClient(t *testing.T) {
	for _, s := range []int{
		400, 401, 402, 403, 404, 405, 408, 409, 413, 422, 429,
		500, 501, 502, 503, 504, 505,
	} {
		if got := clientFacingStatus(s); got == http.StatusForbidden {
			t.Errorf("状态 %d 被映射成 403 —— 客户端会把它读成「API 密钥无效」，"+
				"不该有任何路径送出 403", s)
		}
	}
}

// TestOtherStatusesUnchanged 其它状态码**逐字不变**。
//
// 只改 403 一个；其余保持原样，避免"顺手把 429 也改掉"之类
// 破坏了客户端既有的重试/提示逻辑（429 会让客户端走 RATE_LIMIT，那是对的）。
func TestOtherStatusesUnchanged(t *testing.T) {
	cases := map[int]int{
		http.StatusBadRequest:           http.StatusBadRequest,
		http.StatusUnauthorized:         http.StatusUnauthorized,
		http.StatusTooManyRequests:      http.StatusTooManyRequests,
		http.StatusInternalServerError:  http.StatusInternalServerError,
		http.StatusServiceUnavailable:   http.StatusServiceUnavailable,
		http.StatusGatewayTimeout:       http.StatusGatewayTimeout,
		http.StatusOK:                   http.StatusOK,
	}
	for in, want := range cases {
		if got := clientFacingStatus(in); got != want {
			t.Errorf("状态 %d 应保持 %d（本次只该改 403），实际 %d", in, want, got)
		}
	}
}
