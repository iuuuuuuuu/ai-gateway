package upstream

// model_param_invalid_test.go 锁定 11133（model_param_invalid）的独立归类。
//
// # 守的是所有者 2026-09-28 的真实现场
//
// 他一个 417 轮 / 458M token 的**开发会话**报错：
//
//	HTTP 400 {"code":11133,"msg":"Invalid request parameters",
//	  "extError":{"code":"model_param_invalid","param":"",
//	    "message":"the request parameters were rejected by the model provider",
//	    "type":"invalid_request_error","StatusCode":400}}
//
// 而网关把它当成**账号故障**逐号轮转，最终展示成：
//
//	503 {"code":"no_healthy_account",
//	     "message":"all accounts unavailable (cooling/disabled): ..."}
//
// 用户的反应是去查账号 —— 方向完全错。真实原因是**请求参数被模型提供商拒了**，
// 与账号毫无关系：换一万个账号也是同样的 400。
//
// 这些测试锁住三件事：
//  1. 11133 必须被识别为独立类别（否则会落进 ErrClient 被轮转）
//  2. 它必须与 ErrContextTooLong 分开（后者会让客户端触发自动压缩，
//     而 11133 的 param 是空串、未必与长度有关，误导压缩无济于事）
//  3. 文案要保留 extError.message，让用户看到上游说了什么

import (
	"strings"
	"testing"
)

// real11133Body 所有者现场的**真实**响应体（逐字抄自他贴的报错）。
const real11133Body = `{"code":11133,"msg":"Invalid request parameters",` +
	`"requestId":"2d8ced42-cde3-4415-a292-84c6360501e9",` +
	`"extError":{"code":"model_param_invalid",` +
	`"message":"the request parameters were rejected by the model provider",` +
	`"param":"","type":"invalid_request_error","StatusCode":400,` +
	`"Request":null,"Response":null},` +
	`"displayMsg":{"en":"The request parameters do not meet the current model requirements. Please adjust and retry.",` +
	`"zh":"请求参数不符合当前模型要求，请调整后重试。"},` +
	`"actions":["SUBMIT_FEEDBACK","COPY_ERROR","NEW_CONVERSATION"]}`

// TestClassifyModelParamInvalid 400 + 11133 必须判成 ErrModelParamInvalid。
//
// 这是整条修复链的入口：判错成 ErrClient 就会被逐号轮转。
func TestClassifyModelParamInvalid(t *testing.T) {
	if got := Classify(400, real11133Body); got != ErrModelParamInvalid {
		t.Fatalf("400+11133 应判成 ErrModelParamInvalid（否则会被当成账号问题"+
			"逐号轮转，最后伪装成「账号全部不可用」），实际 %v", got)
	}
}

// TestModelParamInvalidIsNotClient 必须与通用 ErrClient 分开。
//
// 防"图省事合并回去"：ErrClient 会被 applyErrorPolicy 当成账号问题处理。
func TestModelParamInvalidIsNotClient(t *testing.T) {
	if got := Classify(400, real11133Body); got == ErrClient {
		t.Fatal("11133 不能归进 ErrClient —— 那正是本次缺陷的根因")
	}
}

// TestModelParamInvalidDistinctFromContextTooLong ★ 必须与上下文超长分开。
//
// # 为什么这条最重要
//
// 两者都是 400/请求侧错误，看起来"可以合并"，但**客户端行为完全不同**：
//
//	ErrContextTooLong  → 客户端据此触发**自动压缩**并重试
//	ErrModelParamInvalid → 压缩无用（param 是空串，未必与长度有关）
//
// 若把 11133 归到上下文超长，客户端会反复压缩重试，而问题始终存在 ——
// 那比"报错但方向对"更糟：用户看到的是无限重试却永远失败。
func TestModelParamInvalidDistinctFromContextTooLong(t *testing.T) {
	// 11133 不能被判成上下文超长
	if got := Classify(400, real11133Body); got == ErrContextTooLong {
		t.Fatal("11133 不能被判成 ErrContextTooLong —— 那会让客户端误触发" +
			"自动压缩，而压缩不会解决参数问题，表现为无限重试")
	}
	// 反过来：真正的 11115 也不能被判成 11133
	ctxBody := `{"code":11115,"msg":"prompt is too long: 1119655 tokens > 1048576 maximum"}`
	if got := Classify(400, ctxBody); got != ErrContextTooLong {
		t.Fatalf("11115 应保持 ErrContextTooLong，实际 %v", got)
	}
}

// TestIsModelParamInvalidMatches 判据要能认出两种写法。
func TestIsModelParamInvalidMatches(t *testing.T) {
	yes := []string{
		real11133Body,
		`{"code":11133}`,
		`{"extError":{"code":"model_param_invalid"}}`,
		`{"extError":{"code":"MODEL_PARAM_INVALID"}}`, // 大小写不敏感
	}
	for _, b := range yes {
		if !IsModelParamInvalid(b) {
			t.Errorf("应识别为 11133: %s", b[:min(60, len(b))])
		}
	}
	no := []string{
		`{"code":11115,"msg":"prompt is too long"}`,
		`{"code":11102,"msg":"model service info not found"}`,
		`{"code":11140,"msg":"request illegal"}`,
		`{"code":0,"msg":"ok"}`,
		``,
	}
	for _, b := range no {
		if IsModelParamInvalid(b) {
			t.Errorf("不该识别为 11133（误伤会让请求侧错误被当账号问题轮转）: %s", b)
		}
	}
}

// TestModelParamInvalidMessageKeepsUpstreamText 文案要带回上游的说明。
//
// 用户要据此判断"是参数问题"而不是"账号挂了"。上游的
// "the request parameters were rejected by the model provider" 是最直接的证据。
func TestModelParamInvalidMessageKeepsUpstreamText(t *testing.T) {
	msg := ModelParamInvalidMessage(real11133Body)
	if !strings.Contains(msg, "rejected by the model provider") {
		t.Errorf("文案应保留上游 extError.message（那是「参数问题」的直接证据），实际: %s", msg)
	}
	if !strings.Contains(msg, "11133") {
		t.Errorf("文案应含业务码 11133 便于排查，实际: %s", msg)
	}
}

// TestModelParamInvalidMessageNoUpstreamText 上游没给 message 时不能 panic、不能空。
func TestModelParamInvalidMessageNoUpstreamText(t *testing.T) {
	for _, b := range []string{`{"code":11133}`, `not json`, ``} {
		msg := ModelParamInvalidMessage(b)
		if msg == "" {
			t.Errorf("输入 %q 时文案不该为空", b)
		}
	}
}

// TestClassifyUnaffectedByModelParamInvalid 新增 kind 不得改动既有分类（零漂移）。
func TestClassifyUnaffectedByModelParamInvalid(t *testing.T) {
	cases := []struct {
		status int
		body   string
		want   ErrKind
	}{
		{402, `{}`, ErrHardCredit},
		{429, `{}`, ErrSoftRate},
		{404, `{}`, ErrNotFound},
		{500, `{}`, ErrServer},
		{503, `{}`, ErrServer},
		{403, `<html>403 Forbidden</html>`, ErrClient},
		{403, `{"code":11140,"msg":"request illegal"}`, ErrAccountUnusable},
		{400, `{"code":11102}`, ErrModelNotInRegion},
		{400, `{"code":11115,"msg":"prompt is too long"}`, ErrContextTooLong},
		{200, `{"code":0}`, ErrNone},
	}
	for _, c := range cases {
		if got := Classify(c.status, c.body); got != c.want {
			t.Errorf("Classify(%d, %q) = %v，期望 %v（新增 kind 不该改动既有分类）",
				c.status, c.body, got, c.want)
		}
	}
}
