package zcode

// captcha_link_test.go —— 钉住 3007 这个信号在**真实改写链路**上不被抹掉。
//
// # 为什么必须单独有这条（这是本文件存在的唯一理由）
//
// server 层判定「上游要求人机验证」用的是 `IsCaptchaRequiredBody(respBody)`，
// 而那个 respBody **不是上游原文** —— 它是 `StreamChat` 改写后的产物：
//
//	streamChatOnce 400
//	  → NormalizeErrorBody    {code:3007,msg:…} → {error:{message:…,code:3007}}
//	  → IsCaptchaRequiredBody 判据第一次命中（决定要不要去求解）
//	  → solveCaptcha 失败
//	  → appendCaptchaNote     往 error.message 追加附注、**重新序列化整张 map**
//	  → 才轮到 server 层**再判一次**
//
// `internal/server/captcha_required_test.go` 注入的是**假上游**
//（`ProductUpstream` 接口，直接给出改写后的形状）—— 于是这条链路上
// 「改写之后判据是否仍然命中」从未被任何测试覆盖过。
//
// 而它一旦失效，症状是**误导性的**：3007 会退化成「账号不可用（冷却/禁用）」，
// 唯一真实的信息（要人机验证）被埋进正文 —— 正是本次要修的缺陷本身。
//
// 故这里用进程内 httptest 当上游、跑**真实** `Client.StreamChat`，断言：
//
//	① 改写后判据仍命中（server 层不会看走眼）
//	② 数字业务码 3007 仍在 error.code（判据的硬依托）
//	③ 附注确实写进了 error.message（用户看得到「求解没成功」的原因）
//	④ 求解失败时不重试上游、且不给出可读流
//
// # 为什么不打真实上游
//
// 端点用 `SetAnthropicBaseForTest` 整体替换（见 client.go 的该字段注释）；
// 且 Cred **不带 jwt** ⇒ 走 coding-plan 通道。start-plan 通道是硬编码的
// zcode.z.ai，测试不该碰它。
//
// # 为什么不碰求解器单例
//
// 本包测试从不给 `SharedCaptchaSolver()` 设 dir / 外部求解服务，
// 故 `UnavailableReason()` 必非空 ⇒ `solveCaptcha` **确定性失败**
//（不起 Node 子进程、不发网络），`appendCaptchaNote` 分支必被执行 ——
// 而这正是要覆盖的分支。零共享状态污染、零耗时、可复现。

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

// TestCaptchaSignalSurvivesClientRewrite 3007 经真实改写后仍能被 server 层认出。
func TestCaptchaSignalSurvivesClientRewrite(t *testing.T) {
	var hits int
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		hits++
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusBadRequest)
		// 上游原样形状（ZCode 业务码：顶层 code + msg）
		io.WriteString(w, `{"code":3007,"msg":"captcha verify failed"}`)
	}))
	defer srv.Close()

	c := New()
	c.SetAnthropicBaseForTest(srv.URL + "/api/anthropic")
	cr := &Cred{Provider: ProviderZAI, Credential: "k.s"} // 无 jwt ⇒ coding-plan

	rc, status, body, err := c.StreamChat(context.Background(), cr,
		[]byte(`{"model":"glm-5.3","messages":[{"role":"user","content":"hi"}]}`))
	if err != nil {
		t.Fatalf("不该返回 transport 错误: %v", err)
	}
	if rc != nil {
		rc.Close()
		t.Fatal("求解失败时不该给出可读流（server 层会把它当成成功流去解析）")
	}
	if status != http.StatusBadRequest {
		t.Errorf("状态码应保留上游的 400，实际 %d", status)
	}
	// 没有 param 就不该重试：重试只会再吃一次 3007，白耗一次上游配额。
	if hits != 1 {
		t.Errorf("求解失败后不该重试上游，实际请求 %d 次", hits)
	}

	// ---- ① 本文件的核心：判据作用在**改写后**的产物上仍命中 ----
	if !IsCaptchaRequiredBody(body) {
		t.Fatalf("改写后 server 层判据失效 —— 3007 会退化成「账号不可用」，用户看到误导信息。改写产物：%s", body)
	}

	var m map[string]any
	if err := json.Unmarshal(body, &m); err != nil {
		t.Fatalf("改写后必须是合法 JSON（旧实现往 JSON 后拼文本，用户只看到「未知错误」）：%s", body)
	}
	e, ok := m["error"].(map[string]any)
	if !ok {
		t.Fatalf("错误体应包成 OpenAI 的 {error:{…}} 形状，实际 %s", body)
	}

	// ---- ② 数字业务码必须原样保留 ----
	//
	// 这是判据的**硬依托**：`zcode.Classify` 的 extractCode 只认字符串 code，
	// 认不出归一后的数字 3007（故 server 层不能改用 Classify）；
	// 附注文案本身也不含「3007」/「验证码」，一旦 code 丢了就再也判不出来。
	code, ok := e["code"].(float64)
	if !ok {
		t.Fatalf("error.code 应保留上游数字业务码，实际 %#v（%s）", e["code"], body)
	}
	if int(code) != 3007 {
		t.Errorf("error.code 应为 3007，实际 %v", code)
	}
	if !strings.Contains(string(body), "3007") {
		t.Errorf("改写产物里必须还能找到 3007（判据的唯一硬依托），实际 %s", body)
	}

	// ---- ③ 附注要写进 message（用户与 server 层都靠它）----
	msg, _ := e["message"].(string)
	if !strings.Contains(msg, "captcha verify failed") {
		t.Errorf("上游原文必须保留（server 层的 captchaUpstreamDetail 靠它给出原因），实际 %q", msg)
	}
	if !strings.Contains(msg, "网关附注") || !strings.Contains(msg, "人机验证") {
		t.Errorf("应追加「求解未成功」的附注，实际 %q", msg)
	}
}

// TestNonCaptcha400GetsNoCaptchaNote 无关的 400 不该被误判成人机验证。
//
// 判据是**宽匹配**（`3007` / `captcha verify failed` / `验证码` 三者之一），
// 而求解是**有成本**的（起 Node、约 3 秒、且会上游限流）——
// 把无关 400 判成验证码会白白消耗求解配额，且把错误信息引向错误方向。
//
// 上游真实观察到的形状（大小写敏感导致的模型不存在）：
//
//	400 {"code":11102,"msg":"model [GLM-5.3] service info not found"}
func TestNonCaptcha400GetsNoCaptchaNote(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusBadRequest)
		io.WriteString(w, `{"code":11102,"msg":"model [GLM-5.3] service info not found"}`)
	}))
	defer srv.Close()

	c := New()
	c.SetAnthropicBaseForTest(srv.URL + "/api/anthropic")
	cr := &Cred{Provider: ProviderZAI, Credential: "k.s"}

	_, status, body, err := c.StreamChat(context.Background(), cr,
		[]byte(`{"model":"GLM-5.3","messages":[{"role":"user","content":"hi"}]}`))
	if err != nil {
		t.Fatalf("不该返回 transport 错误: %v", err)
	}
	if status != http.StatusBadRequest {
		t.Errorf("状态码应为 400，实际 %d", status)
	}
	if IsCaptchaRequiredBody(body) {
		t.Fatalf("无关 400 被判成人机验证 —— 会白白消耗求解配额：%s", body)
	}
	if strings.Contains(string(body), "网关附注") {
		t.Errorf("无关 400 不该被追加验证码附注：%s", body)
	}

	var m map[string]any
	if err := json.Unmarshal(body, &m); err != nil {
		t.Fatalf("错误体应是合法 JSON，实际 %s", body)
	}
	e, _ := m["error"].(map[string]any)
	if code, _ := e["code"].(float64); int(code) != 11102 {
		t.Errorf("error.code 应保留 11102，实际 %#v（%s）", e["code"], body)
	}
}
