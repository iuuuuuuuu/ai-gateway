package server

// 模型在当前通道/区域上不存在（上游 11102）时的处理契约（所有者 2026-09-20）。
//
// # 所有者原话
//
//	「这个模型,如果是排队,就应该直接报错出来要排队多久,
//	  而不是说这个模型不能用」
//
// 他说对了一半：**消息确实在误导**，但 11102 不是排队（排队是 10605，
// 已有「免费模型正在排队…预计等待 N 秒」的可读文案）。
//
// 11102 的真实含义是**区域不匹配** —— 同名模型可能只在某一侧上游存在。
// 它此前被归进通用 ErrClient ⇒ 逐账号轮转 ⇒ 最后包装成
// `503 no_healthy_account` + 「all accounts unavailable (cooling/disabled)」，
// 于是所有者看到的是"账号全挂了/模型不能用"，而真相是**模型名没指定区域**。
//
// # 三条契约（与 effort_rejected_test.go 同一套）
//
//	① 状态码用上游真实状态（400），不是 503
//	② 错误码单独成型（model_not_in_region），让客户端能识别
//	③ 消息说清"等也没用"并给出出路（换模型 / 加区域前缀），且带回上游原文
//
// 另：**不轮转、不冷却账号** —— 请求侧错误，换号毫无意义。
import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync/atomic"
	"testing"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/upstream"
)

// 上游真实原文（所有者 2026-09-20 的对话报错）。
const modelNotInRegionBody = `{"code":11102,"msg":"model [Qwen3.8-Flash] service info not found","requestId":"5125ef0e"}`

// modelNotInRegionHandler 上游一律回 400 + 11102 原文。
func modelNotInRegionHandler(t *testing.T) (*Handler, *int32) {
	t.Helper()
	resetModelsCache()
	var calls int32
	p := testPoolWith(
		&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999, Domain: "copilot.tencent.com", SoonestExpireAt: 1 << 40},
		&auth.Auth{UID: "u2", AccessToken: "at2", ExpiresAt: 9999999999, Domain: "copilot.tencent.com", SoonestExpireAt: 1 << 40},
		&auth.Auth{UID: "u3", AccessToken: "at3", ExpiresAt: 9999999999, Domain: "copilot.tencent.com", SoonestExpireAt: 1 << 40},
	)
	h := NewHandler(Config{
		Pool:      p,
		MaxRotate: 3,
		Upstream: newFakeUpstream(t, func(string) (int, string, bool) {
			atomic.AddInt32(&calls, 1)
			return http.StatusBadRequest, modelNotInRegionBody, false
		}),
	})
	return h, &calls
}

// TestModelNotInRegionDoesNotRotate 请求侧错误**不该**对着每个账号重传一遍。
//
// 判据是**上游调用次数**：若仍轮转，3 个账号会被打 3 次。
// 这与 effort/context 两类既有特判的取向一致。
func TestModelNotInRegionDoesNotRotate(t *testing.T) {
	h, calls := modelNotInRegionHandler(t)

	body := `{"model":"Qwen3.8-Flash","messages":[{"role":"user","content":"hi"}]}`
	req := httptest.NewRequest(http.MethodPost, "/v1/chat/completions", strings.NewReader(body))
	req.Header.Set("Content-Type", "application/json")
	w := httptest.NewRecorder()
	// ⚠ 走 `ServeHTTP`（路由器）而不是直接调 h.chatCompletions ——
	// 后者是小写方法，跨包不可见；且走路由才能覆盖真实的入口链路
	//（与 effort_rejected_test.go 同一做法）。
	h.ServeHTTP(w, req)

	if got := atomic.LoadInt32(calls); got != 1 {
		t.Errorf("上游应只被调 1 次（请求侧错误，轮转无意义），实际 %d 次 —— "+
			"说明 11102 仍走了轮转路径，会把同一个请求对着每个账号重传", got)
	}
}

// TestModelNotInRegionStatusAndCode 状态码透出上游真实值，错误码单独成型。
func TestModelNotInRegionStatusAndCode(t *testing.T) {
	h, _ := modelNotInRegionHandler(t)

	body := `{"model":"Qwen3.8-Flash","messages":[{"role":"user","content":"hi"}]}`
	req := httptest.NewRequest(http.MethodPost, "/v1/chat/completions", strings.NewReader(body))
	req.Header.Set("Content-Type", "application/json")
	w := httptest.NewRecorder()
	// ⚠ 走 `ServeHTTP`（路由器）而不是直接调 h.chatCompletions ——
	// 后者是小写方法，跨包不可见；且走路由才能覆盖真实的入口链路
	//（与 effort_rejected_test.go 同一做法）。
	h.ServeHTTP(w, req)

	if w.Code != http.StatusBadRequest {
		t.Errorf("状态码应为 400（上游真实值），实际 %d —— "+
			"503 会让客户端以为「稍后重试就好」，而这里等多久都不会出现", w.Code)
	}

	var env struct {
		Error struct {
			Code    string `json:"code"`
			Message string `json:"message"`
		} `json:"error"`
	}
	if err := json.Unmarshal(w.Body.Bytes(), &env); err != nil {
		t.Fatalf("响应不是预期 JSON：%v\n%s", err, w.Body.String())
	}
	if env.Error.Code != "model_not_in_region" {
		t.Errorf("错误码应为 model_not_in_region，实际 %q —— "+
			"落进 no_healthy_account 会让用户去查账号池，方向完全错", env.Error.Code)
	}
}

// TestModelNotInRegionMessageIsActionable 消息要说清"等没用"并给出出路。
func TestModelNotInRegionMessageIsActionable(t *testing.T) {
	h, _ := modelNotInRegionHandler(t)

	body := `{"model":"Qwen3.8-Flash","messages":[{"role":"user","content":"hi"}]}`
	req := httptest.NewRequest(http.MethodPost, "/v1/chat/completions", strings.NewReader(body))
	req.Header.Set("Content-Type", "application/json")
	w := httptest.NewRecorder()
	// ⚠ 走 `ServeHTTP`（路由器）而不是直接调 h.chatCompletions ——
	// 后者是小写方法，跨包不可见；且走路由才能覆盖真实的入口链路
	//（与 effort_rejected_test.go 同一做法）。
	h.ServeHTTP(w, req)

	var env struct {
		Error struct {
			Message string `json:"message"`
		} `json:"error"`
	}
	_ = json.Unmarshal(w.Body.Bytes(), &env)
	msg := env.Error.Message

	// 必须带模型名 —— 用户要知道是哪个模型出的问题
	if !strings.Contains(msg, "Qwen3.8-Flash") {
		t.Errorf("消息应含模型名，实际 %q", msg)
	}
	// 必须说清是"不存在"而不是"排队"（所有者正是把两者混为一谈）
	if !strings.Contains(msg, "不存在") {
		t.Errorf("消息应说明该模型在此通道不存在，实际 %q", msg)
	}
	// 必须给出出路：换模型 / 用区域前缀
	if !strings.Contains(msg, "区域") {
		t.Errorf("消息应给出「用平台:区域:模型名明确指定」这条出路，实际 %q", msg)
	}
	// 带回上游原文，便于排查
	if !strings.Contains(msg, "service info not found") {
		t.Errorf("消息应保留上游原文，实际 %q", msg)
	}
	// ⚠ 不该出现「账号全部不可用」这类误导性说法
	if strings.Contains(msg, "账号") {
		t.Errorf("消息不该提到账号（那是账号池耗尽的措辞，会误导排查方向），实际 %q", msg)
	}
}

// TestClassifyModelNotInRegion 分类器要把 11102 认成独立种类。
//
// 只测分类（不经过完整 HTTP 路径）—— 它是上面三条契约的前提。
func TestClassifyModelNotInRegion(t *testing.T) {
	if got := upstream.Classify(http.StatusBadRequest, modelNotInRegionBody); got != upstream.ErrModelNotInRegion {
		t.Errorf("11102 应分类为 ErrModelNotInRegion，实际 %v（%s）", got, got)
	}
	// 文案特征也能认出来（部分上游不回 code）
	if !upstream.IsModelNotInRegion(`{"msg":"model [X] service info not found"}`) {
		t.Error("文案含 service info not found 时应判为 ErrModelNotInRegion")
	}
	// 反面：正常的 400 不该被误判
	if upstream.IsModelNotInRegion(`{"code":11115,"msg":"context too long"}`) {
		t.Error("上下文超长不该被判成模型不存在")
	}
	if upstream.Classify(http.StatusTooManyRequests, `{"code":6004}`) == upstream.ErrModelNotInRegion {
		t.Error("限流不该被判成模型不存在")
	}
}
