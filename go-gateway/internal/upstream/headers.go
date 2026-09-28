// Package headers 构造三类上游请求头（common / chat / billing / refresh）。
// 规则来自 docs/api-reference.md §0/§4/§6。
package upstream

import (
	"net/http"

	"workbuddy2api/internal/auth"
)

const (
	clientUACN        = "CLI/2.63.2 CodeBuddy/2.63.2"
	clientUAIntl      = "WorkBuddy/5.6.2 WorkBuddy/5.6.2 CLI/2.147.0"
	originRefererCN   = "https://www.codebuddy.cn"
	originRefererIntl = "https://www.workbuddy.ai"
)

// originRefererFor 返回该账号所属区域对应的 Origin/Referer。
//
// 上游按 Origin 判定来源区域，跨区域发送会被拒或落到错误的服务，
// 因此必须跟随账号区域，不能恒为国服。
func originRefererFor(a *auth.Auth) string {
	if isIntl(a) {
		return originRefererIntl
	}
	return originRefererCN
}

// clientUAFor 返回该账号所属区域对应的 User-Agent。
//
// # 为什么必须按区域区分（2026-09-27 实测缺陷：国际版 11140 request illegal）
//
// 国际版上游按 UA 判定来源客户端。我们此前对两个区域**一律**发国服 CLI 的
// `CLI/2.63.2 CodeBuddy/2.63.2`，而官方国际版客户端（WorkBuddyAI.exe 5.6.2）
// 发的是 `WorkBuddy/5.6.2 WorkBuddy/5.6.2 CLI/2.147.0`。
//
// 实测（同一账号、同一模型、同一端点 POST /v2/chat/completions）：
//
//	官方国际版客户端 → HTTP 200，正常流式返回
//	网关（国服 UA）  → HTTP 403 {"code":11140,"msg":"request illegal"}
//
// 即**账号与内容都没问题**，是请求身份不被国际版接受。
func clientUAFor(a *auth.Auth) string {
	if isIntl(a) {
		return clientUAIntl
	}
	return clientUACN
}

// CommonHeaders 设置所有 API 共享的请求头。
func CommonHeaders(req *http.Request, a *auth.Auth) {
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Accept", "application/json, text/plain, */*")
	req.Header.Set("X-Requested-With", "XMLHttpRequest")
	origin := originRefererFor(a)
	req.Header.Set("Origin", origin)
	req.Header.Set("Referer", origin+"/")
	req.Header.Set("User-Agent", clientUAFor(a))
}

// ChatHeaders 在 common 之上加 chat 专属的账号头。
// 缺省字段用 X-No-* 约定（与 CodeBuddy 官方 CLI 一致）。
func ChatHeaders(req *http.Request, a *auth.Auth) {
	CommonHeaders(req, a)
	if a.AccessToken != "" {
		req.Header.Set("Authorization", "Bearer "+a.AccessToken)
	} else {
		req.Header.Set("X-No-Authorization", "1")
	}
	if a.UID != "" {
		req.Header.Set("X-User-Id", a.UID)
	} else {
		req.Header.Set("X-No-User-Id", "1")
	}
	if a.EnterpriseID != "" {
		req.Header.Set("X-Enterprise-Id", a.EnterpriseID)
	} else {
		req.Header.Set("X-No-Enterprise-Id", "1")
	}
	// 安全红线：绝不在 chat 请求里携带 X-Refresh-Token。
	if a.Domain != "" {
		req.Header.Set("X-Domain", a.Domain)
	} else {
		req.Header.Set("X-No-Department-Info", "1")
	}
	req.Header.Set("X-Product", "SaaS")
	// 国际版额外声明客户端身份（对照官方 WorkBuddyAI.exe 抓包，2026-09-27）。
	//
	// 这些头**只在国际版路径上发**：国服上游不认它们，多发的风险是
	// 让国服把请求当成未知客户端（国服当前实测良好，不动它）。
	if isIntl(a) {
		req.Header.Set("X-IDE-Type", "WorkBuddy")
		req.Header.Set("X-IDE-Name", "WorkBuddy")
		req.Header.Set("X-IDE-Version", "5.6.2")
		req.Header.Set("X-Agent-Intent", "craft")
		req.Header.Set("X-Agent-Purpose", "conversation")
		req.Header.Set("X-Agent-Type", "main")
		req.Header.Set("x-codebuddy-request", "1")
	}
}

// BillingHeaders billing 接口请求头。
func BillingHeaders(req *http.Request, a *auth.Auth) {
	req.Header.Set("Authorization", "Bearer "+a.AccessToken)
	req.Header.Set("Accept", "application/json")
	req.Header.Set("Content-Type", "application/json")
	if a.UID != "" {
		req.Header.Set("X-User-Id", a.UID)
	}
	if a.EnterpriseID != "" {
		req.Header.Set("X-Enterprise-Id", a.EnterpriseID)
		req.Header.Set("X-Tenant-Id", a.EnterpriseID)
	}
	if a.Domain != "" {
		req.Header.Set("X-Domain", a.Domain)
	}
}

// RefreshHeaders refresh 端点专属头（X-Refresh-Token 只允许出现在这里）。
func RefreshHeaders(req *http.Request, a *auth.Auth) {
	CommonHeaders(req, a)
	req.Header.Set("X-Refresh-Token", a.RefreshToken)
	if a.EnterpriseID != "" {
		req.Header.Set("X-Enterprise-Id", a.EnterpriseID)
	}
	req.Header.Set("X-Auth-Refresh-Source", "workbuddy")
}
