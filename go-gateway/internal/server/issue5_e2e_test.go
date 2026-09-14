package server

import (
	"fmt"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/pool"
	"workbuddy2api/internal/session"
)

// issue5Pool 构造 n 个账号的池与会话路由（账号名 accta..acctn）。
func issue5Pool(n int) (*pool.Pool, *session.Router, []string, *bindStore) {
	uids := make([]string, 0, n)
	auths := make([]*auth.Auth, 0, n)
	for i := 0; i < n; i++ {
		uid := fmt.Sprintf("acct%c", 'a'+i)
		uids = append(uids, uid)
		auths = append(auths, &auth.Auth{UID: uid, AccessToken: "at-" + uid, ExpiresAt: 9999999999})
	}
	st := newBindStore()
	sess := session.New(session.Config{
		TTL:       time.Minute,
		Store:     st,
		Available: func() []string { return uids },
	})
	return testPoolWith(auths...), sess, uids, st
}

// issue5Chat 发一次带会话键的 chat 请求，返回 HTTP 状态码与响应体。
func issue5Chat(h *Handler, key string) (int, string) {
	body := fmt.Sprintf(`{"model":"glm-5.2","messages":[],"metadata":{"conversation_id":%q}}`, key)
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/chat/completions", strings.NewReader(body)))
	return rec.Code, rec.Body.String()
}

// issue #5 症状之四端到端：14 个账号 + 粘性会话。
//
// 用户反馈：「长对话中出现错误后，就算切账号发送『继续』，也还是这个报错，
// 重启客户端也无用」。本测试验证：当长对话原本绑定的账号持续失败时，
// 网关能解绑并轮换到其它健康账号，而不是死守一个坏号。
func TestIssue5_LongConversationRotatesAwayFromFailingAccount(t *testing.T) {
	p, sess, _, st := issue5Pool(14)

	// accta 是所有请求都会失败的坏号。
	up := newFakeUpstream(t, func(authz string) (int, string, bool) {
		if authz == "Bearer at-accta" {
			return 429, `{"error":"rate limited"}`, false
		}
		return 200, sseOK, true
	})
	h := NewHandler(Config{Pool: p, Upstream: up, Session: sess, SoftCooldown: time.Minute})

	// 长对话原先绑定在坏号上。
	sess.Bind("conv-long", "accta")

	// 第一次请求：坏号失败 → 应解绑并轮换到健康号成功。
	if code, body := issue5Chat(h, "conv-long"); code != 200 {
		t.Fatalf("长对话未能从坏号轮换到健康号: code=%d body=%s", code, strings.TrimSpace(body))
	}
	if uid, ok := st.lastUID("conv-long"); !ok || uid == "accta" {
		t.Fatalf("绑定未收敛到健康账号，仍是 %s (ok=%v, binds=%v)", uid, ok, st.binds)
	}

	// 后续多轮「继续」应持续成功（不再反复撞坏号）。
	for i := 2; i <= 11; i++ {
		if code, body := issue5Chat(h, "conv-long"); code != 200 {
			t.Fatalf("第 %d 轮续聊失败: code=%d body=%s（用户反馈的『发送继续仍报错』）",
				i, code, strings.TrimSpace(body))
		}
	}
}

// issue #5 症状之三端到端：切负载均衡新窗口不应因部分账号冷却而整体不可用。
//
// 用户反馈：「切负载均衡新窗口，会提示尝试信息，并且提示冷却」。
// 只要池里还有健康账号，新会话就必须能被分配到可用账号。
func TestIssue5_NewSessionUsableWhileSomeAccountsCooling(t *testing.T) {
	p, sess, uids, _ := issue5Pool(14)

	// 让一半账号进入软冷却，模拟「频繁不可用」的中间状态。
	for i := 0; i < len(uids)/2; i++ {
		p.Cooldown(uids[i], pool.CoolSoft, time.Hour, "429 rate limit")
	}

	up := newFakeUpstream(t, func(authz string) (int, string, bool) {
		return 200, sseOK, true
	})
	h := NewHandler(Config{Pool: p, Upstream: up, Session: sess, SoftCooldown: time.Minute})

	// 多个全新会话（切新窗口）都应可用。
	for i := 0; i < 20; i++ {
		key := fmt.Sprintf("conv-new-%d", i)
		if code, body := issue5Chat(h, key); code != 200 {
			t.Fatalf("新会话 %s 不可用: code=%d body=%s（用户反馈的『切新窗口提示冷却』）",
				key, code, strings.TrimSpace(body))
		}
	}
}
