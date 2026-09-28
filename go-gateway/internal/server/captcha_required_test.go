package server

// ZCode 3007「需要人机验证」的处置测试。
//
// ## 要证明的核心命题（所有者 2026-09-28 报告的真实现场）
//
// 他收到的是：
//
//	503 {"code":"no_healthy_account",
//	     "message":"all accounts unavailable (cooling/disabled):
//	                upstream client (http 400):
//	                {\"error\":{\"code\":3007,\"message\":\"captcha verify failed\"}}"}
//
// 这条消息**每一句都在误导**：账号一个都没问题、没有任何账号进入冷却、
// 唯一真实的信息（要人机验证）被埋在最后。
//
// 成因：3007 的 HTTP 状态是 **400** ⇒ `upstream.Classify` 归到 ErrClient
// ⇒ `applyErrorPolicy` 的 default 分支「只换号不罚号」⇒ 继续轮转。
// ZCode 池通常只有一两个账号，轮转瞬间耗尽 ⇒ 回出"账号全挂了"。
//
// 故本测试断言四件事：
//
//	① **不轮转**：上游只被调用 1 次（MaxRotate=3 也不该用满）
//	② **不罚账号**：该账号请求后不进冷却
//	③ 错误码是 `captcha_required`，不是 `no_healthy_account`
//	④ 文案说人机验证，且**不再出现**"all accounts unavailable (cooling/disabled)"

import (
	"context"
	"io"
	"strings"
	"testing"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/pool"
)

// captchaBody 是 server 层实际会看到的响应体形状。
//
// ⚠ 必须用**归一后**的形状（`zcode.NormalizeErrorBody` 的产物）：
// 上游原样是 `{"code":3007,"msg":"captcha verify failed"}`，但 client 层
// 已把它转成 OpenAI 形状且 **code 变成数字**。用原样形状测会掩盖
// 「`zcode.Classify` 认不出数字 code」这个真实约束（见 captchaRequiredMessage）。
const captchaBody = `{"error":{"message":"captcha verify failed",` +
	`"type":"upstream_error","code":3007}}`

// captchaUpstream 一个固定返回 400 + 3007 的假上游。
type captchaUpstream struct {
	calls    int
	lastUID  string
	status   int
	respBody string
}

func (f *captchaUpstream) ChatStream(_ context.Context, a *auth.Auth, _ []byte) (io.ReadCloser, int, []byte, error) {
	f.calls++
	f.lastUID = a.UID
	status := f.status
	if status == 0 {
		status = 400
	}
	return nil, status, []byte(f.respBody), nil
}

func (f *captchaUpstream) Aggregate(io.Reader, string) (map[string]any, error) {
	return nil, nil
}

// TestZcodeCaptchaStopsRotation 3007 必须**立即停止轮转**且**不罚账号**。
func TestZcodeCaptchaStopsRotation(t *testing.T) {
	p := pool.New("")
	acct := zcodeAuth("zc-captcha")
	p.Add(acct)

	// MaxRotate=3：若修复失效，轮转会跑满 3 次并把账号耗尽。
	fz := &captchaUpstream{respBody: captchaBody}
	h := NewHandler(Config{
		Pool:      p,
		APIKey:    "",
		MaxRotate: 3,
		Zcode:     fz,
	})

	body := []byte(`{"model":"glm-5.3-flash","messages":[{"role":"user","content":"hi"}]}`)
	_, status, err := h.forwardChat(body, true, "")
	if err == nil {
		t.Fatal("3007 应当报错，实际成功")
	}

	// ① 不轮转
	if fz.calls != 1 {
		t.Errorf("3007 应当只打上游 1 次（不换号），实际 %d 次 —— 轮转没被止住", fz.calls)
	}

	// ② 不罚账号
	st, ok := p.Status("zc-captcha")
	if !ok {
		t.Fatal("账号应当还在池里")
	}
	if st.Cooling {
		t.Errorf("3007 与账号无关，不该罚号；实际该账号已冷却（kind=%q reason=%q）",
			st.CoolKind, st.Reason)
	}
	if st.Disabled {
		t.Error("3007 不该禁用账号")
	}

	// ③ 错误码与状态码
	if status != 503 {
		t.Errorf("状态码应为 503（上游侧条件、稍后可能恢复），实际 %d", status)
	}
	if code := errorCodeFor(err); code != "captcha_required" {
		t.Errorf("错误码应为 captcha_required，实际 %q", code)
	}
	if code, _ := openAIFailure(err); code != "captcha_required" {
		t.Errorf("openAIFailure 应为 captcha_required，实际 %q", code)
	}

	// ④ 文案：说人机验证，且不再说"账号全挂了"
	msg := err.Error()
	if !strings.Contains(msg, "人机验证") {
		t.Errorf("文案应点明「人机验证」，实际：%s", msg)
	}
	if strings.Contains(msg, "all accounts unavailable") {
		t.Errorf("文案不该再说「账号全部不可用」—— 账号一个都没问题。实际：%s", msg)
	}
	if strings.Contains(msg, "cooling/disabled") {
		t.Errorf("文案不该再说「冷却/禁用」—— 没有任何账号因此冷却。实际：%s", msg)
	}
}

// TestZcodeCaptchaKeepsSession 3007 不该解绑粘性会话。
//
// 与 3012 分支同因：`fail(uid)` 会在该号正是粘性绑定号时 Unbind，
// 而账号完全无辜（问题在上游对这次请求的验证要求）。解绑会让同一会话
// 下次换到别的号 —— 而验证要求过去后它本可继续用。
//
// 这里用「账号仍在池里且未被禁用」间接断言：真正的 Unbind 需要
// Session 实例，而本分支与 3012 共用同一个 `releaseHeld()` 写法。
func TestZcodeCaptchaKeepsSession(t *testing.T) {
	p := pool.New("")
	p.Add(zcodeAuth("zc-keep"))

	fz := &captchaUpstream{respBody: captchaBody}
	h := NewHandler(Config{Pool: p, APIKey: "", MaxRotate: 1, Zcode: fz})

	body := []byte(`{"model":"glm-5.3","messages":[{"role":"user","content":"hi"}]}`)
	_, _, _ = h.forwardChat(body, true, "")

	// 租约必须已释放（releaseHeld 生效）：否则该号会一直占着在途名额。
	st, _ := p.Status("zc-keep")
	if st.InFlight != 0 {
		t.Errorf("在途租约应当已释放，实际 in_flight=%d —— releaseHeld 没生效", st.InFlight)
	}
}

// TestNonZcodeProductNotHijacked 产品判定必须生效。
//
// `zcode.IsCaptchaRequiredBody` 是**宽匹配**（"3007" / "验证码" /
// "captcha verify failed"）—— 其它产品的响应体里偶然出现同样的数字串
// 或字样时，不该被这条分支截走（那会让它们失去既有的冷却/换号处置）。
//
// 这里用 Qoder 账号 + 含 "3007" 的响应体：应当**照旧轮转**（calls 用满），
// 而不是被 3007 分支截成一次即返回。
//
// ⚠ 必须放**多个**账号：只有一个账号时轮转本来就只跑 1 次
//（`tried` 标记挡住了重复选号），那样测不出这条分支是否真的截走了请求
// —— 第一版就是这么写的，无论修复在不在都会"通过"。
func TestNonZcodeProductNotHijacked(t *testing.T) {
	p := pool.New("")
	p.Add(qoderAuth("qd-captcha-1"))
	p.Add(qoderAuth("qd-captcha-2"))
	p.Add(qoderAuth("qd-captcha-3"))

	fq := &captchaUpstream{respBody: captchaBody}
	h := NewHandler(Config{Pool: p, APIKey: "", MaxRotate: 3, Qoder: fq})

	body := []byte(`{"model":"qwen3-max","messages":[{"role":"user","content":"hi"}]}`)
	_, _, err := h.forwardChat(body, true, "")
	if err == nil {
		t.Fatal("应当报错")
	}
	if fq.calls != 3 {
		t.Errorf("Qoder 账号不该被 3007 分支截走，应照旧跑满 MaxRotate=3，实际 %d 次", fq.calls)
	}
	if code := errorCodeFor(err); code == "captcha_required" {
		t.Error("Qoder 的失败不该拿到 captcha_required —— 该码专属于 ZCode")
	}
}

// TestCaptchaRequiredMessage 文案必须带上上游原文，且不依赖 zcode.Classify。
//
// ⚠ 这条测试守着一个**易踩的坑**：server 层拿到的 code 是**数字** 3007，
// 而 `zcode.Classify` 的 `extractCode` 只认**字符串** code / 顶层 code
// ⇒ 直接调 Classify 会返回 ErrNone、文案退化成默认句。
// 故实现是自己构造 zcode.Error 再取 FriendlyMessage。
func TestCaptchaRequiredMessage(t *testing.T) {
	msg := captchaRequiredMessage(captchaBody)
	if !strings.Contains(msg, "人机验证") {
		t.Errorf("文案应点明人机验证，实际：%s", msg)
	}
	if !strings.Contains(msg, "captcha verify failed") {
		t.Errorf("文案应带上上游原文（否则用户不知道上游说了什么），实际：%s", msg)
	}
	if strings.Contains(msg, "未知错误") {
		t.Errorf("文案退化成了默认句 —— 说明走了 zcode.Classify（认不出数字 code）。实际：%s", msg)
	}

	// 非 JSON / 空体也必须给出可用文案，不能空着。
	for _, in := range []string{"", "   ", "<html>502 Bad Gateway</html>"} {
		if got := captchaRequiredMessage(in); strings.TrimSpace(got) == "" {
			t.Errorf("输入 %q 时文案不该为空", in)
		}
	}
}
