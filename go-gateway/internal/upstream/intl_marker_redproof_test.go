package upstream

import (
	"net/http"
	"strings"
	"testing"
)

// TestRedProofPluralMarkerIsLoadBearing 隔离验证「复数 marker」这一处修复是**必需的**。
//
// 为什么单独写：Classify 里还有一层 isCreditExhaustedCode 的业务码判定，
// 它会先命中、把「文案不匹配」这个缺陷掩盖掉（实测：只还原 marker 表时
// TestClassifyCreditExhaustedByMarker 仍然 PASS）。因此必须在**排除业务码**的
// 前提下验证 marker 表本身 —— 否则无法证明补齐复数写法是必要修复。
//
// 做法：直接查 hardMarkers 是否含复数形态，并验证「无业务码的复数文案」能命中。
func TestRedProofPluralMarkerIsLoadBearing(t *testing.T) {
	// 1) 复数形态必须在表里（旧表只有单数 "credit exhausted"）
	foundPlural := false
	for _, m := range hardMarkers {
		if strings.EqualFold(m, "credits exhausted") {
			foundPlural = true
			break
		}
	}
	if !foundPlural {
		t.Fatal("hardMarkers 缺少复数形态 \"credits exhausted\" —— 上游国际版返回的正是复数，" +
			"缺了它会导致 429 被误判为软限流（60 秒）而无限重试")
	}

	// 2) 没有业务码、只有复数文案时也必须命中（模拟上游只给文案的场景）
	body := `{"error":{"msg":"Credits exhausted. Please purchase add-on packs."}}`
	if got := Classify(http.StatusTooManyRequests, body); got != ErrHardCredit {
		t.Fatalf("无业务码的复数文案应命中 hardMarkers → ErrHardCredit，实际 %v", got)
	}

	// 3) 反向：去掉 s 的单数写法同样命中（两种都要支持）
	body2 := `{"error":{"msg":"credit exhausted"}}`
	if got := Classify(http.StatusTooManyRequests, body2); got != ErrHardCredit {
		t.Fatalf("单数写法也应命中，实际 %v", got)
	}
}
