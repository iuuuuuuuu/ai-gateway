package server

import (
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync/atomic"
	"testing"

	"workbuddy2api/internal/auth"
)

// ---------------------------------------------------------------------------
// 「空流账号必须被换掉、且被判定为失败」——所有者现场缺陷的集成回归
//
// # 现场（2026-09-20）
//
// 他装完 1.0.7 后：
//
//	「我刚安装之后报错 本轮运行失败empty upstream stream,
//	  然后我点击开始,这一次跑了一会儿,然后就又报这个错误,现在无法对话」
//
// 关键在**"现在无法对话"** —— 不只是报了一次错，而是**之后一直失败**。
//
// # 根因（两个缺陷叠加）
//
//  1. `ProbeFirstFrame` 只看 `data:` 前缀，而 `Stream` 要求 JSON 可解析 ——
//     上游只发 `data: [DONE]` 时 **probe 放行、Stream 报空流**。
//  2. 更致命：probe 放行后代码走到 `NoteSuccess(acct.UID)` ——
//     那个**只会返回空流**的坏账号被记成"成功"、**永不冷却**，
//     于是每次请求都优先选中它 → 用户**持续**无法对话。
//
// 修法：probe 改用 `ValidSSEFrame`（与 Stream 同口径），空流在
// **写 200 之前**就被判失败 → 走 applyErrorPolicy 冷却 + 换号重试。
//
// # 判据（三条合起来 = "会不会永久无法对话"）
//
//	① 请求最终**成功**（换到了正常账号）
//	② 坏账号被**记成失败**（cooling / disabled），不是成功
//	③ **连续多次**请求都成功（不会被坏账号反复挡住）
// ---------------------------------------------------------------------------

// 真实形状的正常流
const realSSEForRotate = "data: {\"id\":\"a\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"好\"}}]}\n\n" +
	"data: [DONE]\n\n"

// 只有 [DONE] 的"假正常"流 —— 上游 200 但**一帧内容都没有**
const doneOnlySSE = "data: [DONE]\n\n"

// TestEmptyStreamAccountRotatedAndUserGetsContent 核心回归。
//
// 上游：第 1 次请求返回空流，之后正常。
// 断言：用户最终拿到内容，且**看不到** empty upstream stream。
func TestEmptyStreamAccountRotatedAndUserGetsContent(t *testing.T) {
	var calls int32
	up := newFakeUpstream(t, func(string) (int, string, bool) {
		n := atomic.AddInt32(&calls, 1)
		if n == 1 {
			return 200, doneOnlySSE, true // ← 空流（现场缺陷的触发条件）
		}
		return 200, realSSEForRotate, true
	})
	p := testPoolWith(
		&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999},
		&auth.Auth{UID: "u2", AccessToken: "at2", ExpiresAt: 9999999999},
	)
	h := NewHandler(Config{Pool: p, Upstream: up, MaxRotate: 3})

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(`{"model":"glm-5.3","messages":[{"role":"user","content":"hi"}],"stream":true}`)))

	body := rec.Body.String()
	if strings.Contains(body, "empty upstream stream") {
		t.Errorf("用户不该看到 empty upstream stream（那是缺陷表现），实际响应: %s", body)
	}
	if rec.Code != http.StatusOK {
		t.Errorf("换号后应成功（code=200），实际 %d，响应: %s", rec.Code, body)
	}
	if !strings.Contains(body, "好") {
		t.Errorf("用户应拿到正常内容，实际: %s", body)
	}
	if n := atomic.LoadInt32(&calls); n < 2 {
		t.Errorf("空流后必须换号重试（应 >=2 次调用），实际 %d 次", n)
	}
}

// TestEmptyStreamAccountIsCooledNotMarkedSuccess 坏账号必须被记成失败。
//
// # 这是"永久无法对话"的真正成因
//
// 旧代码在 probe 放行后调 `NoteSuccess` —— 只回空流的账号被记成"成功"，
// **永不冷却**，于是**每次**请求都优先选中它。
// 断言：至少有一个账号进了冷却/停用（说明空流被当作失败处理了）。
func TestEmptyStreamAccountIsCooledNotMarkedSuccess(t *testing.T) {
	var calls int32
	up := newFakeUpstream(t, func(string) (int, string, bool) {
		n := atomic.AddInt32(&calls, 1)
		if n == 1 {
			return 200, doneOnlySSE, true
		}
		return 200, realSSEForRotate, true
	})
	p := testPoolWith(
		&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999},
		&auth.Auth{UID: "u2", AccessToken: "at2", ExpiresAt: 9999999999},
	)
	h := NewHandler(Config{Pool: p, Upstream: up, MaxRotate: 3})

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(`{"model":"glm-5.3","messages":[{"role":"user","content":"hi"}],"stream":true}`)))

	anyFailed := false
	for _, uid := range []string{"u1", "u2"} {
		if st, ok := p.Status(uid); ok {
			// 空流账号应被判定为不可用：冷却中 **或** 已停用 **或** 记了错误
			if st.Cooling || st.Disabled || st.ErrTotal > 0 {
				anyFailed = true
			}
		}
	}
	if !anyFailed {
		t.Error("只返回空流的账号必须被判定为失败（冷却/停用/记错）—— " +
			"若被记成成功，它会被**反复优先选中**，用户从此无法对话（这正是现场现象）")
	}
}

// TestEmptyStreamDoesNotPermanentlyLockChat 「现在无法对话」的直接判据。
//
// 连续 3 次独立请求都必须成功。旧代码下：第一次把坏账号记成"成功"，
// 之后每次都被它挡住 —— 第 2、3 次必红。
func TestEmptyStreamDoesNotPermanentlyLockChat(t *testing.T) {
	var calls int32
	up := newFakeUpstream(t, func(string) (int, string, bool) {
		n := atomic.AddInt32(&calls, 1)
		// 前两次给空流（模拟"点击开始 → 又报一次"），之后正常
		if n <= 2 {
			return 200, doneOnlySSE, true
		}
		return 200, realSSEForRotate, true
	})
	p := testPoolWith(
		&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999},
		&auth.Auth{UID: "u2", AccessToken: "at2", ExpiresAt: 9999999999},
		&auth.Auth{UID: "u3", AccessToken: "at3", ExpiresAt: 9999999999},
	)
	h := NewHandler(Config{Pool: p, Upstream: up, MaxRotate: 3})

	for i := 1; i <= 3; i++ {
		rec := httptest.NewRecorder()
		h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/chat/completions",
			strings.NewReader(`{"model":"glm-5.3","messages":[{"role":"user","content":"hi"}],"stream":true}`)))
		body := rec.Body.String()
		if strings.Contains(body, "empty upstream stream") {
			t.Fatalf("第 %d 次请求用户看到 empty upstream stream（缺陷复现），响应: %s", i, body)
		}
		if !strings.Contains(body, "好") {
			t.Fatalf("第 %d 次请求没拿到内容，响应: %s", i, body)
		}
	}
}

// TestAllAccountsEmptyStreamReportsClearError 全池都空流时要**如实报错**，
// 而不是让用户看到 `empty upstream stream` 这种内部术语。
func TestAllAccountsEmptyStreamReportsClearError(t *testing.T) {
	up := newFakeUpstream(t, func(string) (int, string, bool) {
		return 200, doneOnlySSE, true
	})
	p := testPoolWith(
		&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999},
		&auth.Auth{UID: "u2", AccessToken: "at2", ExpiresAt: 9999999999},
	)
	h := NewHandler(Config{Pool: p, Upstream: up, MaxRotate: 3})

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(`{"model":"glm-5.3","messages":[{"role":"user","content":"hi"}],"stream":true}`)))

	body := rec.Body.String()
	if strings.Contains(body, "empty upstream stream") {
		t.Errorf("面向用户的错误不该是内部术语 `empty upstream stream`（用户无法据此排查），"+
			"实际: %s", body)
	}
	if rec.Code < 400 {
		t.Errorf("全池空流应报错（>=400），实际 code=%d", rec.Code)
	}
	// 至少要有中文说明，让用户知道发生了什么
	if !strings.Contains(body, "空流") && !strings.Contains(body, "上游") {
		t.Errorf("错误信息应说明是上游问题（含「空流」或「上游」），实际: %s", body)
	}
}

// TestEmptyStreamDoesNotPenalizeWithPermanentDisable 空流不该**永久停用**账号。
//
// 空流可能是上游临时抽风 —— 直接禁用会让用户损失一个正常账号。
// 正确做法是冷却（过一段时间还能用），由 applyErrorPolicy 分类决定。
func TestEmptyStreamDoesNotPenalizeWithPermanentDisable(t *testing.T) {
	up := newFakeUpstream(t, func(string) (int, string, bool) {
		return 200, doneOnlySSE, true
	})
	p := testPoolWith(&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999})
	h := NewHandler(Config{Pool: p, Upstream: up, MaxRotate: 1})

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(`{"model":"glm-5.3","messages":[{"role":"user","content":"hi"}],"stream":true}`)))

	if st, ok := p.Status("u1"); ok {
		if st.Disabled {
			t.Error("单次空流**不该永久停用**账号（可能只是上游临时抽风）—— " +
				"应按冷却处理，由 applyErrorPolicy 决定时长")
		}
	}
	_ = io.Discard
}
