package server

// /debug/accounts/{uid}/revive 的端到端用例。
//
// 端点存在的理由与 pool.ReviveDisabled 一致：disabled 会持久化进 state.json，
// 而它此前**只有写入方没有任何清除入口**，被自动判定为死号的账号在网关内
// 永远救不回来（重新导入凭证也无效）。本文件锁住端点这一层的三件事：
//  1. 真的把账号放回流量池（含「复活后能真正被选中」）；
//  2. 鉴权与既有 /debug/* 一致（未鉴权一律 401）——它比只读视图更敏感；
//  3. 错误语义：uid 不存在 → 404；本就启用 → 200 + revived=false（幂等）。

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/pool"
	"workbuddy2api/internal/upstream"
)

// reviveReq 发一次复活请求；token 为空时不带任何鉴权头。
func reviveReq(t *testing.T, h *Handler, uid, token string) *httptest.ResponseRecorder {
	t.Helper()
	req := httptest.NewRequest("POST", "/debug/accounts/"+uid+"/revive", nil)
	if token != "" {
		req.Header.Set("Authorization", "Bearer "+token)
	}
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, req)
	return rec
}

// decodeJSON 解析响应体，失败即 Fatal（避免后续断言在 nil map 上 panic）。
func decodeJSON(t *testing.T, rec *httptest.ResponseRecorder) map[string]any {
	t.Helper()
	var resp map[string]any
	if err := json.Unmarshal(rec.Body.Bytes(), &resp); err != nil {
		t.Fatalf("响应不是 JSON: %v body=%s", err, rec.Body)
	}
	return resp
}

// TestDebugReviveReturnsAccountToPool 端点主路径：禁用 → POST 复活 → 能重新被选中。
func TestDebugReviveReturnsAccountToPool(t *testing.T) {
	p := testPoolWith(&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999})
	p.Disable("u1", "12153 session dead")
	h := NewHandler(Config{Pool: p, Upstream: upstream.New()})

	rec := reviveReq(t, h, "u1", "")
	if rec.Code != http.StatusOK {
		t.Fatalf("code=%d body=%s", rec.Code, rec.Body)
	}
	resp := decodeJSON(t, rec)
	if resp["revived"] != true {
		t.Errorf("revived=%v want true", resp["revived"])
	}
	if resp["disabled"] != false {
		t.Errorf("disabled=%v want false", resp["disabled"])
	}
	// 复活前的禁用原因必须透出：复活后池里已清空，这是唯一的排查线索。
	if resp["disabled_reason_before"] != "12153 session dead" {
		t.Errorf("disabled_reason_before=%v want 12153 session dead", resp["disabled_reason_before"])
	}

	// 关键断言：账号真的回到了流量池（不只是 Status 里的一个布尔值）。
	if got := p.Pick(); got == nil || got.UID != "u1" {
		t.Fatalf("复活后账号应能重新被选中，实际 %+v", got)
	}
}

// TestDebugReviveIsIdempotent 复活一个本来就启用的账号：200 + revived=false，不是错误。
//
// 客户端（宿主）会把这个调用当成幂等操作重试，若第二次返回 4xx/5xx，
// 用户会看到「复活失败」而实际上账号一直是好的。
func TestDebugReviveIsIdempotent(t *testing.T) {
	p := testPoolWith(&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999})
	h := NewHandler(Config{Pool: p, Upstream: upstream.New()})

	rec := reviveReq(t, h, "u1", "")
	if rec.Code != http.StatusOK {
		t.Fatalf("本就启用应返回 200，实际 code=%d body=%s", rec.Code, rec.Body)
	}
	resp := decodeJSON(t, rec)
	if resp["revived"] != false {
		t.Errorf("revived=%v want false（幂等：没有状态变化）", resp["revived"])
	}
	if resp["disabled"] != false {
		t.Errorf("disabled=%v want false", resp["disabled"])
	}
}

// TestDebugReviveUnknownUID 未知 uid → 404，且错误体里说明「为什么可能不在池中」。
func TestDebugReviveUnknownUID(t *testing.T) {
	p := testPoolWith(&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999})
	h := NewHandler(Config{Pool: p, Upstream: upstream.New()})

	rec := reviveReq(t, h, "nope", "")
	if rec.Code != http.StatusNotFound {
		t.Fatalf("code=%d want 404 body=%s", rec.Code, rec.Body)
	}
	resp := decodeJSON(t, rec)
	if resp["uid"] != "nope" {
		t.Errorf("uid=%v want nope", resp["uid"])
	}
	// 打错 uid 时最常见的真实原因是「刚导入还没进池」，提示必须给出这条出路。
	if hint, _ := resp["hint"].(string); hint == "" {
		t.Error("404 响应应给出排查提示（hint）")
	}
}

// TestDebugReviveRequiresAuth 必须与既有 /debug/* 一样走鉴权。
//
// 本端点比只读视图**更**敏感：它会改变选号结果。未鉴权暴露等于给同网段
// 任何人一个「把死号放回流量池」的开关（也可能是把余额已耗尽的号放回去）。
func TestDebugReviveRequiresAuth(t *testing.T) {
	p := testPoolWith(&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999})
	p.Disable("u1", "12153 session dead")
	h := NewHandler(Config{Pool: p, Upstream: upstream.New(), APIKey: "secret"})

	// 无 token → 401，且账号**必须仍处于禁用**（鉴权失败不得有任何副作用）。
	rec := reviveReq(t, h, "u1", "")
	if rec.Code != http.StatusUnauthorized {
		t.Fatalf("无 token: code=%d want 401", rec.Code)
	}
	if st, _ := p.Status("u1"); !st.Disabled {
		t.Error("鉴权失败的请求不得产生副作用（账号不该被复活）")
	}

	// 错误 token → 401
	if rec := reviveReq(t, h, "u1", "wrong"); rec.Code != http.StatusUnauthorized {
		t.Errorf("错误 token: code=%d want 401", rec.Code)
	}

	// 正确 token → 200 且真的复活
	rec = reviveReq(t, h, "u1", "secret")
	if rec.Code != http.StatusOK {
		t.Fatalf("正确 token: code=%d body=%s", rec.Code, rec.Body)
	}
	if resp := decodeJSON(t, rec); resp["revived"] != true {
		t.Errorf("revived=%v want true", resp["revived"])
	}
	if st, _ := p.Status("u1"); st.Disabled {
		t.Error("带正确 token 的请求应真的复活账号")
	}
}

// TestDebugReviveReportsNoRoute 复活成功但账号仍被用户标「不接流量」时，
// 响应必须**如实说明**，否则用户会以为复活没生效。
//
// 这里刻意同时覆盖 no_route 与冷却两种情况：两者都会让「复活成功」看起来
// 像是没起作用（账号仍不接流量），而它们的处置方式完全不同
//（no_route 要用户去界面关开关；冷却只能等）。
func TestDebugReviveReportsNoRoute(t *testing.T) {
	p := testPoolWith(&auth.Auth{
		UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999,
		NoRoute: true, // 用户手动「停止接流量」
	})
	p.Disable("u1", "12153 session dead")
	h := NewHandler(Config{Pool: p, Upstream: upstream.New()})

	rec := reviveReq(t, h, "u1", "")
	if rec.Code != http.StatusOK {
		t.Fatalf("code=%d body=%s", rec.Code, rec.Body)
	}
	resp := decodeJSON(t, rec)
	if resp["revived"] != true {
		t.Errorf("系统禁用位应被清除: %v", resp)
	}
	if resp["no_route"] != true {
		t.Errorf("no_route=%v want true（用户手工轴不受复活影响）", resp["no_route"])
	}
	notes, _ := resp["notes"].([]any)
	if len(notes) == 0 {
		t.Fatal("复活成功但仍不接流量时必须给出说明（notes）")
	}
	if !notesContain(notes, "不接流量") {
		t.Errorf("notes 应说明 no_route 是用户开关、复活不动它: %v", notes)
	}
}

// TestDebugReviveReportsCooling 复活后仍处冷却/熔断期时，响应要说明「还要等」。
func TestDebugReviveReportsCooling(t *testing.T) {
	p := testPoolWith(&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999})
	p.Cooldown("u1", pool.CoolHard, time.Hour, "余额不足")
	p.Disable("u1", "12153 session dead")
	h := NewHandler(Config{Pool: p, Upstream: upstream.New()})

	resp := decodeJSON(t, reviveReq(t, h, "u1", ""))
	if resp["revived"] != true {
		t.Errorf("revived=%v want true", resp["revived"])
	}
	if resp["cooling"] != true {
		t.Errorf("cooling=%v want true（复活不清冷却）", resp["cooling"])
	}
	notes, _ := resp["notes"].([]any)
	if !notesContain(notes, "冷却") {
		t.Errorf("notes 应说明账号仍在冷却期: %v", notes)
	}
}

// notesContain 判断 notes 里是否有一条包含 substr（notes 是 []any）。
func notesContain(notes []any, substr string) bool {
	for _, n := range notes {
		if s, ok := n.(string); ok && strings.Contains(s, substr) {
			return true
		}
	}
	return false
}
