package server

// captcha_link_test.go —— 把「真实 client 的改写产物」喂给 server 层判据。
//
// # 为什么必须有这一层（同包 captcha_required_test.go 覆盖不到的地方）
//
// 那 4 条用例注入的是**手写**的响应体常量（`captchaBody`），
// 形状是「client 改写之后应该长什么样」的**假设**。假设会漂：
// 本项目就出过一次 —— `appendCaptchaNote` 早期实现往 JSON 后面拼文本，
// 产出**非法 JSON**，用户只看到「未知错误」；而当时的用例测的是别的形状，
// 照样通过。手写夹具无法发现「client 不再产出这个形状」。
//
// server 层的判据（`zcode.IsCaptchaRequiredBody`）作用在**client 改写后**的
// 字节上，所以必须有一条测试：让**真实** `zcode.Client` 去面对一个回 3007 的
// 上游，把它吐出来的字节**原样**交给 server 的转发路径。
//
// 这条链路上任何一环变化都会被抓到：
//
//	NormalizeErrorBody 不再保留数字 code   → 判据失效（唯一硬依托是 error.code=3007）
//	appendCaptchaNote 产出非法 JSON        → 判据失效 + 用户看到「未知错误」
//	appendCaptchaNote 把 code 弄丢         → 同上
//	求解失败时改成重试上游                 → hits 断言抓到（白耗上游配额）
//
// 端点用 `SetAnthropicBaseForTest` 整体替换；Cred **不带 jwt** ⇒ 走
// coding-plan 通道。start-plan 通道是硬编码的 zcode.z.ai，测试不该碰它。
//
// 求解器不配置任何 dir / 外部服务 ⇒ `UnavailableReason()` 必非空 ⇒
// `solveCaptcha` **确定性失败**（不起 Node、不发网络），这正是要覆盖的分支。

import (
	"context"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"workbuddy2api/internal/pool"
	"workbuddy2api/internal/zcode"
)

// rewriteThroughRealClient 用真实 zcode.Client 打一个假上游，返回它改写后的响应体。
//
// upstreamBody 是**上游原样形状**（ZCode 业务码：顶层 code + msg）。
func rewriteThroughRealClient(t *testing.T, upstreamBody string) []byte {
	t.Helper()

	var hits int
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		hits++
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusBadRequest)
		io.WriteString(w, upstreamBody)
	}))
	defer srv.Close()

	c := zcode.New()
	c.SetAnthropicBaseForTest(srv.URL + "/api/anthropic")
	cr := &zcode.Cred{Provider: zcode.ProviderZAI, Credential: "k.s"}

	rc, status, body, err := c.StreamChat(context.Background(), cr,
		[]byte(`{"model":"glm-5.3","messages":[{"role":"user","content":"hi"}]}`))
	if rc != nil {
		rc.Close()
	}
	if err != nil {
		t.Fatalf("真实 client 不该返回 transport 错误：%v", err)
	}
	if status != http.StatusBadRequest {
		t.Fatalf("真实 client 应保留上游的 400，实际 %d（body=%s）", status, body)
	}
	if rc != nil {
		t.Fatal("求解失败时不该给出可读流 —— server 会把它当成功流去解析")
	}
	if hits != 1 {
		t.Fatalf("求解失败后不该重试上游（重试只会再吃一次 3007），实际请求 %d 次", hits)
	}
	return body
}

// TestRealClientBodyDrivesCaptchaBranch 真实改写产物必须驱动出 captcha_required。
func TestRealClientBodyDrivesCaptchaBranch(t *testing.T) {
	body := rewriteThroughRealClient(t, `{"code":3007,"msg":"captcha verify failed"}`)

	// ① server 的准入判据（forward.go 里那个 if）必须命中真实产物。
	if !zcode.IsCaptchaRequiredBody(body) {
		t.Fatalf("真实 client 的改写产物过不了 server 判据 —— 3007 会退化成"+
			"「账号全部不可用（冷却/禁用）」，唯一真实的信息被埋掉。产物：%s", body)
	}

	// ② 把**真实字节**当上游响应，跑完整转发路径。
	p := pool.New("")
	p.Add(zcodeAuth("zc-real"))
	fz := &captchaUpstream{respBody: string(body)}
	h := NewHandler(Config{Pool: p, APIKey: "", MaxRotate: 3, Zcode: fz})

	req := []byte(`{"model":"glm-5.3","messages":[{"role":"user","content":"hi"}]}`)
	_, status, err := h.forwardChat(req, true, "")
	if err == nil {
		t.Fatal("3007 应当报错，实际成功")
	}

	if fz.calls != 1 {
		t.Errorf("真实产物也必须止住轮转（MaxRotate=3），实际打上游 %d 次", fz.calls)
	}
	if status != http.StatusServiceUnavailable {
		t.Errorf("状态码应为 503，实际 %d", status)
	}
	if code := errorCodeFor(err); code != "captcha_required" {
		t.Errorf("错误码应为 captcha_required，实际 %q（真实产物没被认出来）", code)
	}

	msg := err.Error()
	if !strings.Contains(msg, "人机验证") {
		t.Errorf("文案应点明人机验证，实际：%s", msg)
	}
	// 上游原文必须传到用户面前 —— 这是 captchaUpstreamDetail 存在的理由。
	if !strings.Contains(msg, "captcha verify failed") {
		t.Errorf("文案应带上上游原文，实际：%s", msg)
	}
	if strings.Contains(msg, "all accounts unavailable") {
		t.Errorf("文案不该说「账号全部不可用」—— 账号一个都没问题。实际：%s", msg)
	}

	// ③ 账号必须无辜：不进冷却、不被禁用。
	st, ok := p.Status("zc-real")
	if !ok {
		t.Fatal("账号应当还在池里")
	}
	if st.Cooling {
		t.Errorf("3007 与账号无关，不该罚号；实际已冷却（kind=%q reason=%q）",
			st.CoolKind, st.Reason)
	}
	if st.Disabled {
		t.Error("3007 不该禁用账号")
	}
}

// TestRealClientBodyForOther400NotHijacked 真实产物里的**无关 400** 不该被截走。
//
// 判据是宽匹配（"3007" / "captcha verify failed" / "验证码"），而求解**有成本**
//（起 Node、约 3 秒、会上游限流）—— 误判会白耗配额并把错误引向错误方向。
//
// 两种无关 400 分别覆盖两类处置（都是真实上游形状，且都**经真实 client 改写**，
// 故同时验证「改写没有引入误命中」）：
//
//	11128 渠道未批准 → ErrClient ⇒ 照旧逐号轮转（MaxRotate=3 用满），不罚账号
//	11102 模型不在该区域 → ErrModelNotInRegion ⇒ **请求侧**，一次即返回
//
// ⚠ 11102 那条的 calls 断言是 **1 而不是 3** —— 这不是"被截走"，
// 而是它本来就该早退：同一请求体发给任何账号都会被同一侧拒，
// 轮转只是把请求重传一遍（11102 的注释里记着实测：1.12M token 被重传 3 次）。
//
// 第一版这条测试写死了 calls==3，结果被真实行为打回 —— 它抓到的不是缺陷，
// 而是我把「无关 400」当成了同一种处置。保留这行注释以免又被"简化"回去。
func TestRealClientBodyForOther400NotHijacked(t *testing.T) {
	cases := []struct {
		name       string
		upstream   string
		wantCalls  int
		wantErrNot string // 明确**不该**出现的错误码
	}{
		{
			name:       "11128 渠道未批准（账号侧无关错误，照旧轮转）",
			upstream:   `{"code":11128,"msg":"channel not approved"}`,
			wantCalls:  3,
			wantErrNot: "captcha_required",
		},
		{
			name:       "11102 模型不在该区域（请求侧，一次即返回）",
			upstream:   `{"code":11102,"msg":"model [GLM-5.3] service info not found"}`,
			wantCalls:  1,
			wantErrNot: "captcha_required",
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			body := rewriteThroughRealClient(t, tc.upstream)

			if zcode.IsCaptchaRequiredBody(body) {
				t.Fatalf("无关 400 的真实产物被判成人机验证 —— 会白耗求解配额：%s", body)
			}

			p := pool.New("")
			p.Add(zcodeAuth("zc-other-1"))
			p.Add(zcodeAuth("zc-other-2"))
			p.Add(zcodeAuth("zc-other-3"))
			fz := &captchaUpstream{respBody: string(body)}
			h := NewHandler(Config{Pool: p, APIKey: "", MaxRotate: 3, Zcode: fz})

			req := []byte(`{"model":"GLM-5.3","messages":[{"role":"user","content":"hi"}]}`)
			_, _, err := h.forwardChat(req, true, "")
			if err == nil {
				t.Fatal("应当报错")
			}
			if code := errorCodeFor(err); code == tc.wantErrNot {
				t.Errorf("无关 400 不该拿到 %s —— 该码只给真正的人机验证", tc.wantErrNot)
			}
			if fz.calls != tc.wantCalls {
				t.Errorf("上游应被调用 %d 次，实际 %d 次", tc.wantCalls, fz.calls)
			}
		})
	}
}
