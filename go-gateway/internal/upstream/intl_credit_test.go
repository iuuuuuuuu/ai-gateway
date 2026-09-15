package upstream

import (
	"encoding/json"
	"net/http"
	"strings"
	"testing"
)

// ---------------------------------------------------------------------------
// 国际版（workbuddy.ai）账号问题回归测试
//
// 三个都来自真实线上观测（所有者用真实国际版账号复现）：
//   1. 额度耗尽的文案是复数 "Credits exhausted"，旧的单数 marker 恒不命中
//   2. 额度耗尽的业务码 14018 嵌在 error.data.code（非顶层 code）
//   3. 国际版要求 messages 首条必须是 system（否则 400 code=11128）
// ---------------------------------------------------------------------------

// creditExhaustedBody 是线上原始响应体（实测抓取，逐字复制）。
const creditExhaustedBody = `{"error":{"data":{"code":14018,"msg":"Credits exhausted. Please visit the link below to purchase add-on packs and get more credits: https://www.codebuddy.ai/profile/usage ","requestId":"b9ab2fbe-92f5-43ad-9ea6-8ea5340ac861"}}}`

// TestClassifyCreditExhaustedByMarker 复数是关键：旧实现只有单数形式，
// 导致 15 个 marker 全不命中 → 被判成 ErrSoftRate（60 秒）→ 无限重试。
func TestClassifyCreditExhaustedByMarker(t *testing.T) {
	// 上线时的真实状态：HTTP 429 + 复数文案
	got := Classify(http.StatusTooManyRequests, creditExhaustedBody)
	if got != ErrHardCredit {
		t.Fatalf("额度耗尽（复数 Credits）应判为 ErrHardCredit，实际 %v —— 会被当成软限流无限重试", got)
	}
}

// TestHardMarkersCoverPluralAndSingular 单复数两种写法都必须命中。
func TestHardMarkersCoverPluralAndSingular(t *testing.T) {
	cases := []string{
		"Credits exhausted.",
		"credit exhausted",
		"insufficient credits",
		"insufficient credit",
		"no credits",
		"no credit",
		"out of credits",
		"credit not enough",
		"credits not enough",
		"not enough credits",
		"积分不足",
		"余额不足",
	}
	for _, c := range cases {
		body := `{"error":{"data":{"msg":"` + c + `"}}}`
		if got := Classify(http.StatusTooManyRequests, body); got != ErrHardCredit {
			t.Errorf("%q 应命中 hardMarkers → ErrHardCredit，实际 %v", c, got)
		}
	}
}

// TestClassifyCreditExhaustedByCode 业务码 14018 必须被识别 ——
// 即便上游把文案改掉，只要有这个码就仍能正确判类。
func TestClassifyCreditExhaustedByCode(t *testing.T) {
	// 文案已改（不含任何 marker），只剩业务码
	body := `{"error":{"data":{"code":14018,"msg":"quota problem"}}}`
	if got := Classify(http.StatusTooManyRequests, body); got != ErrHardCredit {
		t.Fatalf("业务码 14018 应判为 ErrHardCredit，实际 %v", got)
	}
}

// TestCreditCodeNestingMatters 14018 在 error.data.code，不在顶层 ——
// 用 apiEnvelope（只解顶层）是解不到的，必须按嵌套层级解。
func TestCreditCodeNestingMatters(t *testing.T) {
	if !isCreditExhaustedCode(creditExhaustedBody) {
		t.Fatal("应能从 error.data.code 解出 14018")
	}
	// 顶层同名 code 不算（那是另一种信封）
	nested := `{"code":14018,"msg":"top level"}`
	if isCreditExhaustedCode(nested) {
		t.Error("顶层 code 不应被当成 error.data.code（层级不同）")
	}
	// 其它业务码不误判
	if isCreditExhaustedCode(`{"error":{"data":{"code":6004}}}`) {
		t.Error("6004 是模型级限流，不应被当作额度耗尽")
	}
}

// TestModelRateStillTakesItsOwnPath 额度耗尽的修好之后，
// 模型级限流（6004）必须仍然走 ErrModelRate，不能被抢走。
func TestModelRateStillTakesItsOwnPath(t *testing.T) {
	body := `{"code":6004,"msg":"您的使用量已超出频率限制，将在 2026-09-15 13:25:47 UTC+8 重置，您也可以切换其他模型继续使用。"}`
	if got := Classify(http.StatusTooManyRequests, body); got != ErrModelRate {
		t.Fatalf("模型级限流应仍为 ErrModelRate，实际 %v", got)
	}
}

// TestPlain429StaysSoftRate 普通 429（无限额关键词）仍应是软限流 ——
// 别把 hard 判定放得太宽。
func TestPlain429StaysSoftRate(t *testing.T) {
	body := `{"code":429,"msg":"too many requests"}`
	if got := Classify(http.StatusTooManyRequests, body); got != ErrSoftRate {
		t.Fatalf("普通 429 应为 ErrSoftRate，实际 %v", got)
	}
}

// ---------------------------------------------------------------------------
// 国际版 system 首条注入
// ---------------------------------------------------------------------------

// TestIntlInjectsSystemFirst 国际版：首条是 user 时必须补一条 system。
func TestIntlInjectsSystemFirst(t *testing.T) {
	in := `{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"hi"}]}`
	out := PrepareBodyForRegion([]byte(in), false, nil, true)
	var obj map[string]any
	mustUnmarshal(t, out, &obj)
	msgs := obj["messages"].([]any)
	if len(msgs) != 2 {
		t.Fatalf("应补一条 system，消息数应为 2，实际 %d", len(msgs))
	}
	first := msgs[0].(map[string]any)
	if first["role"] != "system" {
		t.Fatalf("首条应为 system，实际 %v", first["role"])
	}
	// 原有消息必须原样保留在末尾
	last := msgs[1].(map[string]any)
	if last["role"] != "user" || last["content"] != "hi" {
		t.Fatalf("原有 user 消息应原样保留，实际 %v", last)
	}
}

// TestIntlKeepsExistingSystem 首条已是 system 时不应重复插入。
func TestIntlKeepsExistingSystem(t *testing.T) {
	in := `{"model":"m","messages":[{"role":"system","content":"be nice"},{"role":"user","content":"hi"}]}`
	out := PrepareBodyForRegion([]byte(in), false, nil, true)
	var obj map[string]any
	mustUnmarshal(t, out, &obj)
	msgs := obj["messages"].([]any)
	if len(msgs) != 2 {
		t.Fatalf("首条已是 system，不应再补，消息数应为 2，实际 %d", len(msgs))
	}
	if msgs[0].(map[string]any)["content"] != "be nice" {
		t.Error("原有 system 内容不应被替换")
	}
}

// TestDomesticNotInjected 国服绝不能注入 system ——
// 国服的 11128 是「渠道未批准」（见 sanitize.go），与「首条必须 system」无关；
// 而且国服没这个要求，乱插 system 会改变模型看到的上下文。
func TestDomesticNotInjected(t *testing.T) {
	in := `{"model":"m","messages":[{"role":"user","content":"hi"}]}`
	out := PrepareBodyForRegion([]byte(in), false, nil, false) // intl=false
	var obj map[string]any
	mustUnmarshal(t, out, &obj)
	if n := len(obj["messages"].([]any)); n != 1 {
		t.Fatalf("国服不应注入 system，消息数应为 1，实际 %d", n)
	}
}

// TestIntlDeveloperFirstBecomesSystem developer 归一化后若已成 system，不再补。
func TestIntlDeveloperFirstBecomesSystem(t *testing.T) {
	in := `{"model":"m","messages":[{"role":"developer","content":"rules"},{"role":"user","content":"hi"}]}`
	out := PrepareBodyForRegion([]byte(in), false, nil, true)
	var obj map[string]any
	mustUnmarshal(t, out, &obj)
	msgs := obj["messages"].([]any)
	if len(msgs) != 2 {
		t.Fatalf("developer 已归一为 system，不应再补，实际 %d 条", len(msgs))
	}
	if msgs[0].(map[string]any)["role"] != "system" {
		t.Error("首条应是归一后的 system")
	}
}

// TestIntlEmptyMessagesUntouched 空 messages 不补 ——
// 那种请求本就缺上下文，交给上游报错更诚实。
func TestIntlEmptyMessagesUntouched(t *testing.T) {
	for _, in := range []string{
		`{"model":"m","messages":[]}`,
		`{"model":"m"}`,
	} {
		out := PrepareBodyForRegion([]byte(in), false, nil, true)
		var obj map[string]any
		mustUnmarshal(t, out, &obj)
		if msgs, ok := obj["messages"].([]any); ok && len(msgs) != 0 {
			t.Errorf("输入 %s：空/缺失 messages 不应被填充，实际 %d 条", in, len(msgs))
		}
	}
}

// ---------------------------------------------------------------------------
// 客户端可读的错误信息
// ---------------------------------------------------------------------------

// TestFriendlyMessageForCreditExhausted 客户端不该再看到整段原始 JSON。
func TestFriendlyMessageForCreditExhausted(t *testing.T) {
	msg := FriendlyMessage(ErrHardCredit, http.StatusTooManyRequests, creditExhaustedBody)
	if msg == "" {
		t.Fatal("额度耗尽应给出可读文案")
	}
	if strings.Contains(msg, "requestId") || strings.Contains(msg, "{") {
		t.Fatalf("可读文案不应包含原始 JSON，实际: %s", msg)
	}
	if !strings.Contains(msg, "额度") {
		t.Fatalf("文案应说明是额度问题，实际: %s", msg)
	}
}

// TestFriendlyMessageFallsBackEmpty 无更优表述时返回空串，让调用方回退原始文案。
func TestFriendlyMessageFallsBackEmpty(t *testing.T) {
	if msg := FriendlyMessage(ErrNone, http.StatusOK, "{}"); msg != "" {
		t.Fatalf("成功分类不应产生错误文案，实际: %s", msg)
	}
	if msg := FriendlyMessage(ErrClient, http.StatusBadRequest, `{"code":11101}`); msg != "" {
		t.Fatalf("未知 4xx 应回退空串，实际: %s", msg)
	}
}

func mustUnmarshal(t *testing.T, b []byte, v any) {
	t.Helper()
	if err := json.Unmarshal(b, v); err != nil {
		t.Fatalf("解析失败: %v (body=%s)", err, string(b))
	}
}
