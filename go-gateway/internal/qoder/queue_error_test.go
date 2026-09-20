package qoder

// queue_error_test.go 上游「顶层 {code, message}」错误帧的识别（2026-09-20 补）。
//
// # 这条测试守的是什么
//
// 实测（官方 Qoder CN 客户端日志，免费模型排队时）：
//
//	POST gateway.qoder.com.cn/.../agent_chat_generation  → HTTP **200**
//	SSE 第 1 帧：
//	  {"code":"403","message":"{\"code\":\"10605\",\"message\":
//	    \"{\\\"isQueued\\\":true,\\\"modelKey\\\":\\\"qfmodel\\\",
//	       \\\"queueCount\\\":7309,\\\"queueType\\\":\\\"p3\\\",
//	       \\\"retryAfterSeconds\\\":30,\\\"serviceAvailable\\\":true,
//	       \\\"waitTime\\\":206}\"}"}
//
// 这个帧**四个分支全不匹配**（无 error / 无 body / 无 choices / 无 usage），
// 于是被 parseChunk 当"不认识的分片"**静默跳过**；流随即结束且无内容帧
// ⇒ 用户看到**一句空回答**，而真正原因是上游在排队。
//
// 测试要点：
//  ① 这种形状必须被识别成错误（不能静默跳过）
//  ② 三层嵌套要能剥开，渲染成可读文案
//  ③ **正常帧一个都不能被误判**（这是防回归的核心）
import (
	"encoding/json"
	"errors"
	"strings"
	"testing"
)

// officialQueueFrame 官方日志里的**原文**（逐字抄自 qodercli.log）。
//
// ⚠ 这是 Go 字符串字面量，里面的 `\"` 表示 JSON 里的转义引号。
// 也就是说运行时这个字符串里的字节与日志里看到的一致。
const officialQueueFrame = `{"code":"403","message":"{\"code\":\"10605\",\"message\":\"{\\\"isQueued\\\":true,\\\"modelKey\\\":\\\"qfmodel\\\",\\\"queueCount\\\":7309,\\\"queueType\\\":\\\"p3\\\",\\\"retryAfterSeconds\\\":30,\\\"serviceAvailable\\\":true,\\\"waitTime\\\":206}\"}"}`

// TestQueueFrameIsRecognizedAsError 排队帧必须被识别成错误。
//
// 修复前它落到 `&Chunk{Raw: payload}` 被静默跳过 —— 这是缺陷的核心。
func TestQueueFrameIsRecognizedAsError(t *testing.T) {
	ch, err := parseChunk(officialQueueFrame)
	if err == nil {
		t.Fatalf("排队帧必须被识别成错误（修复前它被静默跳过，用户只看到空回答）；"+
			"实际返回 chunk=%+v err=nil", ch)
	}
	var ise *inStreamError
	if !errors.As(err, &ise) {
		t.Fatalf("应是 inStreamError，实际 %T: %v", err, err)
	}
	if ise.Code != "403" {
		t.Errorf("外层 code 应是 403，实际 %q", ise.Code)
	}
}

// TestQueueFrameRendersReadableMessage 三层嵌套要渲染成**人能看懂**的一句话。
//
// 直接拼原文的话用户得自己解两层转义才知道"是在排队"。
func TestQueueFrameRendersReadableMessage(t *testing.T) {
	_, err := parseChunk(officialQueueFrame)
	if err == nil {
		t.Fatal("应报错")
	}
	msg := err.Error()
	t.Logf("渲染结果：%s", msg)

	// 必须点明"排队"与"非故障"
	for _, want := range []string{"排队", "免费"} {
		if !strings.Contains(msg, want) {
			t.Errorf("文案应含 %q（让用户一眼知道是在排队），实际：%s", want, msg)
		}
	}
	// 必须带出关键数字（用户据此判断要不要等）
	for _, want := range []string{"7309", "206"} {
		if !strings.Contains(msg, want) {
			t.Errorf("文案应含数字 %s（来自内层 queueCount/waitTime），实际：%s", want, msg)
		}
	}
	// 应说明服务正常 —— 否则用户会以为账号/服务出问题
	if !strings.Contains(msg, "正常") {
		t.Errorf("应说明上游服务状态正常（serviceAvailable=true），避免用户误判，实际：%s", msg)
	}
	// **不该**把原始的三层转义原文直接丢给用户
	if strings.Contains(msg, `\"`) {
		t.Errorf("渲染后不该残留转义引号（说明没剥开嵌套），实际：%s", msg)
	}
}

// TestNormalFramesNotMistakenAsError **正常帧一个都不能被误判成错误**。
//
// 这是防回归的核心：我改的是 parseChunk 的**兜底分支**，
// 若判据写宽了，正常内容帧会被当错误打断 —— 那比原缺陷更糟。
func TestNormalFramesNotMistakenAsError(t *testing.T) {
	normal := []struct {
		name    string
		payload string
	}{
		{"嵌套内容帧", `{"body":"{\"choices\":[{\"index\":0,\"delta\":{\"content\":\"你好\"}}]}"}`},
		{"直接 OpenAI 内容帧", `{"choices":[{"index":0,"delta":{"content":"hi"}}]}`},
		{"结束帧", `{"body":"{\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}"}`},
		{"独立 usage 帧", `{"usage":{"prompt_tokens":10,"completion_tokens":5}}`},
		{"带 model 的帧", `{"id":"x","model":"m","choices":[{"delta":{"content":"a"}}]}`},
		{"不认识但有 choices", `{"choices":[],"something_new":1}`},
	}
	for _, c := range normal {
		ch, err := parseChunk(c.payload)
		if err != nil {
			t.Errorf("%s **不该**被当错误（会打断正常回答）：payload=%s err=%v",
				c.name, c.payload, err)
			continue
		}
		if ch == nil {
			t.Errorf("%s 应返回一个 chunk", c.name)
		}
	}
}

// TestSuccessCodeNotError 明确表示成功的 code 不算错误。
//
// 防御性：上游某版本可能给正常帧加个 `code:"0"` 或 `"200"`。
// 那样的话我们不该把它当错误（否则所有回答都会被打断）。
func TestSuccessCodeNotError(t *testing.T) {
	for _, p := range []string{
		`{"code":"0","message":"ok"}`,
		`{"code":"200","message":"ok"}`,
		`{"code":0,"message":"ok"}`,
		`{"code":200,"message":"ok"}`,
	} {
		if _, err := parseChunk(p); err != nil {
			t.Errorf("成功码不该判成错误：%s → %v", p, err)
		}
	}
}

// TestUnknownShapeWithoutCodeStillSkipped 没有 code 的不认识分片**仍应跳过**。
//
// 上游加字段是常事，为此中断整个流会让回答被截断 —— 这个既有行为要保住。
func TestUnknownShapeWithoutCodeStillSkipped(t *testing.T) {
	p := `{"something_brand_new":{"a":1}}`
	ch, err := parseChunk(p)
	if err != nil {
		t.Errorf("没有 code 的未知分片该跳过（不中断流），实际报错：%v", err)
	}
	if ch == nil {
		t.Error("应返回 chunk（保留原文供排障）")
	}
}

// TestTopLevelErrorWithMsg 顶层 {code, msg, message} 的其它变体。
func TestTopLevelErrorWithMsg(t *testing.T) {
	cases := []struct {
		name    string
		payload string
		code    string
	}{
		{"message 字段", `{"code":"403","message":"forbidden"}`, "403"},
		{"msg 字段", `{"code":"105","msg":"Login expired"}`, "105"},
		{"数字 code", `{"code":403,"message":"forbidden"}`, "403"},
	}
	for _, c := range cases {
		_, err := parseChunk(c.payload)
		if err == nil {
			t.Errorf("%s 应被识别成错误：%s", c.name, c.payload)
			continue
		}
		var ise *inStreamError
		if errors.As(err, &ise) && ise.Code != c.code {
			t.Errorf("%s：code 应是 %q，实际 %q", c.name, c.code, ise.Code)
		}
	}
}

// TestExistingErrorEnvelopeStillWorks 既有的 `{"error":{…}}` 形状**不能被破坏**。
func TestExistingErrorEnvelopeStillWorks(t *testing.T) {
	_, err := parseChunk(`{"error":{"code":"500","msg":"内部错误"}}`)
	if err == nil {
		t.Fatal("error 包装形状仍应被识别")
	}
	var ise *inStreamError
	if !errors.As(err, &ise) {
		t.Fatalf("应是 inStreamError，实际 %T", err)
	}
	if ise.Code != "500" || !strings.Contains(ise.Msg, "内部错误") {
		t.Errorf("code/msg 应保留，实际 code=%q msg=%q", ise.Code, ise.Msg)
	}
}

// TestPeelNestedIsBounded 剥嵌套必须**有界**（防异常串挂住）。
func TestPeelNestedIsBounded(t *testing.T) {
	// 构造一个自引用的深层嵌套（远超 4 层）
	inner := `{"isQueued":true,"queueCount":1}`
	for i := 0; i < 12; i++ {
		b, _ := json.Marshal(map[string]any{"code": "1", "message": inner})
		inner = string(b)
	}
	e := &inStreamError{Code: "403", Msg: inner}
	// 不该 panic / 不该死循环；剥不到就返回 nil（调用方退回原文）
	got := e.peelNested()
	_ = got
	if s := e.Error(); s == "" {
		t.Error("无论如何都要有非空文案（退回原文）")
	}
}
