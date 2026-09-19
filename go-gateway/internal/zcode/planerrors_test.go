package zcode

// 「没有套餐资格」与「需要验证码」两类错误的分类。
//
// # 为什么这两类必须有测试
//
// 它们是**实测新发现**的上游码（2026-09-19 逐条试出来的）：
//
//	HTTP 403 {"code":3101,"msg":"coding plan is required"}
//	HTTP 400 {"code":3007,"msg":"captcha verify failed"}
//
// 若不特判，前者会掉进 `ErrProviderDown`（403 不匹配任何状态码分支），
// 后者掉进 default —— 用户看到的都是"上游返回 HTTP xxx"这种空洞信息，
// **完全不知道该怎么办**。
//
// 而它们的处置方向与既有的分类**都不一样**：
//
//	额度耗尽（1113）→ 去充值
//	没有套餐（3101）→ 充值没用，要换账号或去官方开套餐
//	需要验证码（3007）→ 我们**不绕过**，请用户去官方客户端完成
//
// 混在一起说会让用户做错事（比如白充钱）。

import (
	"strings"
	"testing"
)

// TestClassifyPlanRequired 3101 → ErrPlanRequired。
func TestClassifyPlanRequired(t *testing.T) {
	body := `{"code":3101,"msg":"coding plan is required"}`
	got := Classify(403, body)
	if got != ErrPlanRequired {
		t.Errorf("3101 应判为 ErrPlanRequired，实际 %q", got)
	}

	// 错误信息必须说清"不是凭证问题、充值也没用"——
	// 否则用户会去充值（白花钱）或反复重登（白费力）
	msg := (&Error{Kind: got, Status: 403, Code: "3101", Msg: "coding plan is required"}).FriendlyMessage()
	for _, must := range []string{"Coding Plan", "不是凭证问题", "充值"} {
		if !strings.Contains(msg, must) {
			t.Errorf("错误文案应包含 %q，实际：%s", must, msg)
		}
	}
}

// TestClassifyCaptchaRequired 3007 → ErrCaptchaRequired。
//
// ⚠ 这个分类的**重点是不做绕过**：文案要让用户去官方客户端完成验证，
// 而不是暗示"我们会想办法过掉"。
func TestClassifyCaptchaRequired(t *testing.T) {
	body := `{"code":3007,"msg":"captcha verify failed"}`
	got := Classify(400, body)
	if got != ErrCaptchaRequired {
		t.Errorf("3007 应判为 ErrCaptchaRequired，实际 %q", got)
	}

	msg := (&Error{Kind: got, Status: 400, Code: "3007", Msg: "captcha verify failed"}).FriendlyMessage()
	for _, must := range []string{"人机验证", "不会绕过", "官方客户端"} {
		if !strings.Contains(msg, must) {
			t.Errorf("错误文案应包含 %q，实际：%s", must, msg)
		}
	}
}

// TestPlanAndCaptchaNotConfusedWithOtherKinds 三类不能互相混淆。
//
// 这是本组测试的核心：三者的**用户动作完全不同**，判错等于误导。
func TestPlanAndCaptchaNotConfusedWithOtherKinds(t *testing.T) {
	cases := []struct {
		status int
		body   string
		want   ErrKind
		why    string
	}{
		{429, `{"error":{"code":"1113","message":"余额不足或无可用资源包,请充值。"}}`,
			ErrNoResourcePack, "1113 必须判「无可用资源包」（不能被 429 抢走成限流）"},
		{403, `{"code":3101,"msg":"coding plan is required"}`,
			ErrPlanRequired, "3101 不能被 403 吃成 provider_down"},
		{400, `{"code":3007,"msg":"captcha verify failed"}`,
			ErrCaptchaRequired, "3007 不能被 400 吃成 default"},
		{400, `{"code":"11102","message":"model [GLM-5.3] service info not found"}`,
			ErrModelNotFound, "11102 仍判模型不存在"},
	}
	for _, c := range cases {
		if got := Classify(c.status, c.body); got != c.want {
			t.Errorf("HTTP %d %s → 期望 %q，实际 %q（%s）", c.status, c.body, c.want, got, c.why)
		}
	}
}

// TestPlanRequiredMessageDoesNotTellUserToRecharge 反向断言。
//
// 「没有套餐」的文案里提到"充值"时，必须是**否定**语境
//（"充值也解决不了"），不能读起来像在建议充值。
func TestPlanRequiredMessageDoesNotTellUserToRecharge(t *testing.T) {
	msg := (&Error{Kind: ErrPlanRequired, Status: 403, Code: "3101", Msg: "coding plan is required"}).FriendlyMessage()

	// 允许出现"充值"，但必须紧跟着否定
	if strings.Contains(msg, "充值") {
		ok := strings.Contains(msg, "充值也解决不了") || strings.Contains(msg, "充值没用")
		if !ok {
			t.Errorf("提到充值时必须是否定语境（否则用户会去白充值）：%s", msg)
		}
	}
}

// TestClassifyRealResponseShapes 用**真实抓到的响应形状**回归。
//
// 这些 body 是从实际请求里抄下来的（含上游自带的方括号与 request_id），
// 不是手写的理想 JSON —— 分类器要能处理真实形状。
func TestClassifyRealResponseShapes(t *testing.T) {
	// Anthropic 协议下的 1113（形状与 OpenAI 版不同）
	//
	// ⚠ 2026-09-19 修正：1113 归为 `ErrNoResourcePack` 而不是
	// `ErrQuotaExhausted` —— 实测同一账号额度充足（3 亿几乎未用）却回 1113，
	// 说明"额度"与"资源包"在不同通道上。报"额度耗尽"会误导用户去充值。
	anthropicQuota := `{"type":"rate_limit_error","code":"1113","message":"[1113][余额不足或无可用资源包,请充值。][20260919110941daa9ea9cccf64197]"}`
	if got := Classify(429, anthropicQuota); got != ErrNoResourcePack {
		t.Errorf("Anthropic 形状的 1113 应判「无可用资源包」，实际 %q", got)
	}

	// 真实的 3101（补全请求头后才出现的码）
	if got := Classify(403, `{"code":3101,"msg":"coding plan is required"}`); got != ErrPlanRequired {
		t.Errorf("真实 3101 应判套餐缺失，实际 %q", got)
	}

	// 真实的 3007
	if got := Classify(400, `{"code":3007,"msg":"captcha verify failed"}`); got != ErrCaptchaRequired {
		t.Errorf("真实 3007 应判需要验证码，实际 %q", got)
	}

	// 3001（缺参数）**刻意不分类**：那是我们自己请求写错，
	// 归到任何"用户可操作"的分类都会误导。
	if got := Classify(400, `{"code":3001,"msg":"parameter error"}`); got == ErrPlanRequired || got == ErrCaptchaRequired {
		t.Errorf("3001 是请求侧问题，不该归入套餐/验证码类，实际 %q", got)
	}
}
