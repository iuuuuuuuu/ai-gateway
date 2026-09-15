package server

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/upstream"
)

// ---------------------------------------------------------------------------
// 「单一模型」锁定（配合积分轮转模式）
//
// 语义：AllowedModel 非空时，只放行该模型，其余一律 400 拒绝。
// 为什么必须锁：轮转 = 「把这个账号的指定模型额度烧干净再换号」，
// 模型是策略的一部分；不锁的话客户端换个模型就能绕过轮转与额度控制。
// ---------------------------------------------------------------------------

// newLockedHandler 构造一个配了单一模型的 handler（账号池里有 1 个可用账号）。
//
// 复用同包已有的 testPoolWith（它已关掉加权随机的 flake 源），
// 不另造一套 pool 构造逻辑。
func newLockedHandler(t *testing.T, allowed string) *Handler {
	t.Helper()
	p := testPoolWith(&auth.Auth{
		UID:             "u1",
		AccessToken:     "t1",
		SoonestExpireAt: time.Now().Add(24 * time.Hour).Unix(),
	})
	return NewHandler(Config{
		Pool:         p,
		Upstream:     upstream.New(),
		MaxRotate:    1,
		AllowedModel: allowed,
	})
}

// post 发一个 chat/completions 请求，返回状态码与响应体。
func post(t *testing.T, h *Handler, body string) (int, string) {
	t.Helper()
	req := httptest.NewRequest(http.MethodPost, "/v1/chat/completions", strings.NewReader(body))
	rec := httptest.NewRecorder()
	h.mux.ServeHTTP(rec, req)
	return rec.Code, rec.Body.String()
}

// TestModelLockRejectsOtherModel 锁定后，其他模型必须被 400 拒绝。
func TestModelLockRejectsOtherModel(t *testing.T) {
	h := newLockedHandler(t, "deepseek-v4.1-flash")
	status, body := post(t, h, `{"model":"glm-5.3","messages":[{"role":"user","content":"hi"}]}`)

	if status != http.StatusBadRequest {
		t.Fatalf("非锁定模型应返回 400，实际 %d（body=%s）", status, body)
	}
	var parsed struct {
		Error struct {
			Code    string `json:"code"`
			Message string `json:"message"`
		} `json:"error"`
	}
	if err := json.Unmarshal([]byte(body), &parsed); err != nil {
		t.Fatalf("响应不是合法 JSON: %v (%s)", err, body)
	}
	// 错误码必须是 model_not_allowed，不能是 no_healthy_account ——
	// 后者会把用户引向排查账号，而真正的原因在请求里。
	if parsed.Error.Code != "model_not_allowed" {
		t.Errorf("错误码应为 model_not_allowed，实际 %q", parsed.Error.Code)
	}
	// 文案要说清「当前用什么、应该改什么」，否则用户不知道怎么办。
	if !strings.Contains(parsed.Error.Message, "deepseek-v4.1-flash") {
		t.Errorf("错误信息应包含被锁定的模型名，实际: %s", parsed.Error.Message)
	}
	if !strings.Contains(parsed.Error.Message, "glm-5.3") {
		t.Errorf("错误信息应包含客户端请求的模型名，实际: %s", parsed.Error.Message)
	}
}

// TestModelLockAllowsExactModel 锁定模型本身必须放行（其余环节照常，最终因无 mock 上游而失败于网络）。
func TestModelLockAllowsExactModel(t *testing.T) {
	h := newLockedHandler(t, "deepseek-v4.1-flash")
	status, body := post(t, h, `{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"hi"}]}`)

	// 关键断言：**不能**是 400 —— 说明没有被模型锁拒绝。
	// （真实上游不可达，所以最终会是 5xx，那是另一条路径，不属于本测试范围。）
	if status == http.StatusBadRequest {
		t.Fatalf("锁定模型本身不应被拒绝，实际 400: %s", body)
	}
}

// TestModelLockIsCaseInsensitive 模型名大小写不敏感（客户端写法不统一）。
func TestModelLockIsCaseInsensitive(t *testing.T) {
	h := newLockedHandler(t, "DeepSeek-V4.1-Flash")
	status, body := post(t, h, `{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"hi"}]}`)
	if status == http.StatusBadRequest {
		t.Fatalf("大小写不同不应被拒绝，实际 400: %s", body)
	}
}

// TestModelLockDisabledByDefault 未配置锁定时不限制任何模型（向后兼容）。
func TestModelLockDisabledByDefault(t *testing.T) {
	h := newLockedHandler(t, "")
	status, body := post(t, h, `{"model":"any-model","messages":[{"role":"user","content":"hi"}]}`)
	if status == http.StatusBadRequest {
		t.Fatalf("未锁定模型时不应有 400 拒绝，实际: %s", body)
	}
}

// TestModelLockRejectsMissingModel 请求体没带 model 时也应被拒绝
// （否则「不指定模型」就成了绕过锁定的后门）。
func TestModelLockRejectsMissingModel(t *testing.T) {
	h := newLockedHandler(t, "deepseek-v4.1-flash")
	status, body := post(t, h, `{"messages":[{"role":"user","content":"hi"}]}`)
	if status != http.StatusBadRequest {
		t.Fatalf("缺少 model 时应返回 400（不能成为绕过锁定的后门），实际 %d: %s", status, body)
	}
	if !strings.Contains(body, "model_not_allowed") {
		t.Errorf("应为 model_not_allowed，实际: %s", body)
	}
}

// TestErrorCodeForDistinguishesLock 错误码映射：锁定错误与账号不可用要分开。
func TestErrorCodeForDistinguishesLock(t *testing.T) {
	if got := errorCodeFor(&modelLockedError{requested: "a", allowed: "b"}); got != "model_not_allowed" {
		t.Errorf("锁定错误应映射为 model_not_allowed，实际 %s", got)
	}
	if got := errorCodeFor(errString("all accounts unavailable")); got != "no_healthy_account" {
		t.Errorf("其他错误应保持 no_healthy_account，实际 %s", got)
	}
}

// errString 简易 error 实现，避免引入 errors.New 之外的依赖。
type errString string

func (e errString) Error() string { return string(e) }
