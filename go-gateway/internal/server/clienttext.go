package server

import (
	"log"
	"regexp"
)

// clienttext.go 守住**所有**面向客户端的错误文案的最后一道闸门。
//
// # 为什么需要一道闸门，而不是把每条文案写干净就完事
//
// 2026-09-29 现场：所有者装了修好状态码归一（403→502）的新包，实测**仍然**
// 显示「本轮运行失败 API 密钥无效」。查证结论是**上一版修错了地方** ——
// 客户端判 AUTH 靠的不是 HTTP 状态码，而是**文案里的数字**。
//
// DSH 的实际分类器（`@deepseek-ai/dsh-llm-pi-ai/lib/index.js:1376-1387`）：
//
//	function classifyPiAiError(message) {
//	  if (/\b(?:401|403)\b/.test(message)) return "AUTH";
//	  if (isQuotaExceededError(message)) return QUOTA_EXCEEDED_CODE;
//	  if (/\b429\b|rate.?limit/i.test(message)) return "RATE_LIMIT";
//	  if (/\b413\b|…/i.test(message)) return "INVALID_REQUEST";
//	  if (/\b400\b|invalid.?request/i.test(message)) return "INVALID_REQUEST";
//	  if (/\b5\d\d\b/.test(message)) return "SERVER";
//	  …
//	}
//
// 而 message 的构成是 pi-ai 把**响应体原文**拼进去的
//（`@earendil-works/pi-ai/dist/utils/error-body.js:111-118` 的
// formatProviderError：`"<status>: <body>"`）。
//
// 于是只要响应体里出现「403」这三个字符，客户端就判 AUTH —— 与 HTTP 状态码
// 无关、与密钥对不对无关。上一版把 403 改成 502 却没动文案，所以没修好。
//
// # 为什么只治 401 / 403
//
// 同一张表里还有 429→RATE_LIMIT、5xx→SERVER 两条数字规则，但它们是**对的**：
// 限流就该让客户端退避、上游故障就该让客户端重试。只有 401/403→AUTH 是
// **误导**——它把「上游拒绝了这个账号」说成「你的密钥不对」，把用户引向
// 一个**不可能修好的方向**（反复改密钥），所以只改这一对。
//
// # 为什么还要留这道闸门（而不是只把文案写干净）
//
// 文案出口有十几处：FriendlyMessage 的十个分支、egressIPBlockedMessage、
// errText 的兜底（直接透 `err.Error()`，而 upstream.Error.Error() 的格式是
// `"upstream %s (http %d): %s"`）、以及各协议各自的兜底分支。逐个盯住
// 不可能长期可靠 —— 上一版就是漏在其中一个分支上。
//
// 闸门装在**所有出口的必经之路**（三个协议的 error 构造器 + 流式 error 事件），
// 任何一条文案里带了 401/403 都会被就地消解成 4xx，客户端再也读不出 AUTH。
// 它正常情况下**不该触发**（源码里的文案已经是干净的），触发即说明有分支
// 漏了，故留一条日志当报警。

// authMisreadPattern 匹配会被客户端读成 AUTH 的**裸三位状态码**。
//
// 只匹配 401 / 403 两个值。注意 \b 的语义：Go 的 \w 与 JS 一致都是
// `[0-9A-Za-z_]`，中文不算词字符，故「上游403业务码」这种**紧贴中文**的
// 写法同样会被匹配到（这正是上一版漏掉的那个形状）。
//
// 四位以上业务码（11140 / 14018 / 3007）**不会**被误伤：`\b403\b` 要求
// 数字两侧都是非词字符，而 "11140" 里的 "140" 左侧是 "1"（词字符）→ 无边界。
var authMisreadPattern = regexp.MustCompile(`\b(?:401|403)\b`)

// clientSafeMessage 是面向客户端的错误文案的**唯一**出口。
//
// 把 401/403 就地改写成 4xx：保留「这是客户端错误类别」的诊断价值，
// 同时让客户端的正则**再也匹配不到**（"4xx" 不匹配 \b401\b，也不匹配
// \b5\d\d\b、\b429\b、\b400\b、\b413\b）。
//
// 只改这两个数字，其余内容**逐字保留** —— 尤其不能碰上游原文里那些
// 用户需要照抄的东西（模型名、参数字段名、token 数）。
func clientSafeMessage(msg string) string {
	if !authMisreadPattern.MatchString(msg) {
		return msg
	}
	// 报警：走到这里说明有文案出口没写干净，需要回去补 —— 但**不能**因此
	// 把错误发出去，客户端优先。
	log.Printf("clienttext: 文案里出现裸 401/403，已就地改写为 4xx（请修正该文案的产出点）：%q", msg)
	return authMisreadPattern.ReplaceAllStringFunc(msg, func(s string) string {
		return s[:1] + "xx"
	})
}
