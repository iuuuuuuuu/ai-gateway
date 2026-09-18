package zcode

// signing.go **刻意不实现**客户端签名（Client Request Signing V4）。
//
// 这个文件存在的唯一目的是**记录为什么不做**，以及留下后路 ——
// 而不是留一段注释在某处容易被忽略的代码里。
//
// ## 参考实现里有什么
//
// TriDefender/zcode-api 的 `src/proxy/client-signing.ts`（593 行）实现了一整套：
//
//	· Ed25519 每请求签名
//	· 8 位工作量证明（PoW，`POW_BITS = 8`）
//	· HKDF 密钥派生（salt `WD_CLIENT_SIGN_KDF_SALT`，
//	  info `getSignKey_hmac` / `ed25519_priv`）
//	· 门控握手（`/api/v1/agent/configs` + `/api/paas/c1f3a7e2/v2/client`）
//	· 密钥缓存、负缓存、401 重试阶梯、永久旁路
//
// ## 为什么本包不做
//
// **实测证明上游不校验签名头。** 对照实验（5 组，一次只改一个变量）：
//
//	组  请求头                      上游响应
//	──  ──────────────────────────  ──────────────────────────────────────────
//	A   完全不带头                   401 {"code":"1001","message":"Authentication
//	                                     parameter not received in Header"}
//	B   仅 Authorization             401 {"code":"1000","message":"Authentication Failed"}
//	C   身份头 + Authorization       401 {"code":"1000","message":"Authentication Failed"}
//	D   身份头 + **假签名头**         401 {"code":"1000","message":"Authentication Failed"}
//	E   身份头 + **空签名值**         401 {"code":"1000","message":"Authentication Failed"}
//
// **D/E 与 C 的响应完全相同** ⇒ 上游不看签名头。
//
// 复跑脚本：`uitest/probe-zcode-signing-control.cjs`
//
// ## 三条独立证据
//
//  1. 上表的直接实测；
//  2. 参考实现**本身是 fail-open 的** —— 门控不可达、握手失败、连续两次
//     401 都退化成"不签名"。若签名是硬门槛，这个设计早就不可用了；
//  3. 三个端点（api.z.ai 的 OpenAI 与 Anthropic、open.bigmodel.cn）在
//     **完全不带签名**时都回凭证错误（而非签名错误），说明请求已到达业务层。
//
// ## 代价与后路
//
// **代价**：若上游将来真的要求签名，我们会失败。
//
// **后路**（三层）：
//
//  1. `errors.go` 的分类**识别** `VERIFY_SIGNATURE_INVALID` /
//     `VERIFY_APIKEY_EXPIRED`，一旦出现就报"上游开始要求客户端签名"，
//     而不是笼统的"认证失败" —— 让失败**可观测、可归因**；
//  2. 凭证解析保留 `{apiKey}.{secret}` 两段式（签名需要拆分它），
//     故届时不需要改数据模型；
//  3. 参考实现的算法有完整注释与测试，可直接移植。
//
// 换句话说：**现在不做，是因为收益为零而成本是 600 行密码学代码；
// 但失败路径是明确且可观测的，不是"赌它不需要"。**

// SigningNotImplemented 说明性常量：本包不发送任何签名头。
//
// 存在的意义：让"我们没实现签名"这件事在代码里**显式可见**，
// 而不是靠"搜不到相关代码"来推断。
const SigningNotImplemented = true

// verifyErrorCodes 上游表示"签名校验失败"的错误码/文案。
//
// 用途：错误分类时识别它们并给出明确提示（见文件头注释的"后路 1"）。
//
// 参考实现的 `VERIFY_SIGNATURE_INVALID` / `VERIFY_APIKEY_EXPIRED`
//（`client-signing.ts` 顶部常量）。
var verifyErrorCodes = []string{
	"VERIFY_SIGNATURE_INVALID",
	"VERIFY_APIKEY_EXPIRED",
}

// IsVerifyFailure 报告响应体是否表示签名校验失败。
//
// 返回 true 时，调用方应报"上游开始要求客户端签名"而不是"认证失败" ——
// 这两种原因的排查方向完全不同（前者要补签名实现，后者要换凭证）。
func IsVerifyFailure(body string) bool {
	for _, code := range verifyErrorCodes {
		if containsFold(body, code) {
			return true
		}
	}
	return false
}

// containsFold 大小写不敏感的子串查找（不引入 strings 依赖的小实现）。
func containsFold(haystack, needle string) bool {
	if len(needle) == 0 {
		return true
	}
	if len(haystack) < len(needle) {
		return false
	}
	// 逐位置比较，两个字符串都转小写
	for i := 0; i+len(needle) <= len(haystack); i++ {
		match := true
		for j := 0; j < len(needle); j++ {
			if lowerByte(haystack[i+j]) != lowerByte(needle[j]) {
				match = false
				break
			}
		}
		if match {
			return true
		}
	}
	return false
}

func lowerByte(b byte) byte {
	if b >= 'A' && b <= 'Z' {
		return b + ('a' - 'A')
	}
	return b
}
