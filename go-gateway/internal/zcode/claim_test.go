package zcode

// claim.go 的单测。
//
// # 为什么重点测错误码解析
//
// 上游响应有**两种形状**（都实测见过）：
//
//	{"code":3001,"msg":"parameter error"}          ← 控制面
//	{"error":{"code":"1113","message":"余额不足"}}  ← 对话面
//
// 而且业务码**有时是数字、有时是字符串**（`3001` vs `"1113"`）。
// 只认一种会漏判，进而用 HTTP 状态码兜底分类 —— 那正是把 1113
//（无资源包）误判成 429（限流）的根源，会让用户去"稍后重试"
// 而不是"换账号/换通道"。

import (
	"testing"
)

// TestCodeOfHandlesBothShapesAndTypes 两种形状 × 两种类型都要认。
func TestCodeOfHandlesBothShapesAndTypes(t *testing.T) {
	cases := []struct {
		name string
		body string
		want string
	}{
		{"控制面形状-数字", `{"code":3001,"msg":"parameter error"}`, "3001"},
		{"控制面形状-字符串", `{"code":"3001","msg":"parameter error"}`, "3001"},
		{"对话面形状-字符串", `{"error":{"code":"1113","message":"余额不足"}}`, "1113"},
		{"对话面形状-数字", `{"error":{"code":1113,"message":"余额不足"}}`, "1113"},
		{"成功码 0 不算错误", `{"code":0,"msg":""}`, ""},
		{"字符串 0 不算错误", `{"code":"0"}`, ""},
		{"无码", `{"data":{}}`, ""},
		{"非 JSON", `not json`, ""},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			if got := codeOf([]byte(c.body)); got != c.want {
				t.Errorf("codeOf(%s) = %q，期望 %q", c.body, got, c.want)
			}
		})
	}
}

// TestAnyToCodeDoesNotProduceFloatSuffix 数字码不能输出成 "3001.0"。
//
// JSON 数字经 `interface{}` 解出来是 float64。若用 `%v` 格式化，
// 3001 会变成 "3001.0" —— 而所有业务码比较都是字符串相等，
// 于是 `code == "3001"` 永远不成立，错误分类静默失效。
func TestAnyToCodeDoesNotProduceFloatSuffix(t *testing.T) {
	if got := anyToCode(float64(3001)); got != "3001" {
		t.Errorf("数字码应输出 \"3001\"（不能是 \"3001.0\"，否则所有码比较都失效），实际 %q", got)
	}
	if got := anyToCode(float64(1003)); got != "1003" {
		t.Errorf("实际 %q", got)
	}
}

// TestMsgOfHandlesBothShapes 文案字段两种形状都要认。
func TestMsgOfHandlesBothShapes(t *testing.T) {
	if got := msgOf([]byte(`{"code":3001,"msg":"parameter error"}`)); got != "parameter error" {
		t.Errorf("控制面形状：期望 parameter error，实际 %q", got)
	}
	if got := msgOf([]byte(`{"error":{"code":"1113","message":"余额不足"}}`)); got != "余额不足" {
		t.Errorf("对话面形状：期望 余额不足，实际 %q", got)
	}
	if got := msgOf([]byte(`not json`)); got != "" {
		t.Errorf("非 JSON 应返回空串，实际 %q", got)
	}
}

// TestClaimPlanRejectsEmptyPlanID 空 planID 必须**本地拦下**。
//
// 不拦的话会发一个 `{"plan_id":""}` 上去，上游回 3001「parameter error」——
// 那个文案完全看不出是"我们没传 ID"，排查方向会被引向"是不是缺 X-Device-Mid"。
func TestClaimPlanRejectsEmptyPlanID(t *testing.T) {
	c := New()
	cr := &Cred{UID: "u", Credential: "x"}
	if _, err := c.ClaimPlan(t.Context(), cr, "   "); err == nil {
		t.Error("空 planID 应本地报错，不该发请求上去")
	}
	if _, err := c.ClaimPlan(t.Context(), nil, "p1"); err == nil {
		t.Error("空账号应报错")
	}
}

// TestPlanOfferParsingSnakeAndCamel preview 的字段两种拼写都要认。
//
// 实测上游同一份响应里两种拼写都出现过（与 balances 的
// `expires_at` / `expiresAt` 同一情况）。
func TestPlanOfferParsingSnakeAndCamel(t *testing.T) {
	p := map[string]any{
		"plan_id": "plan-a", "name": "周末活动", "status": "CLAIMABLE", "ends_at": float64(1789866000),
	}
	got := PlanOffer{
		PlanID: firstNonEmpty(strOf(p, "plan_id"), strOf(p, "planId")),
		Name:   strOf(p, "name"),
		Status: firstNonEmpty(strOf(p, "status"), strOf(p, "claim_status"), strOf(p, "claimStatus")),
		EndsAt: toInt64(firstAny(p["ends_at"], p["endsAt"])),
	}
	if got.PlanID != "plan-a" || got.Name != "周末活动" || got.Status != "CLAIMABLE" {
		t.Errorf("解析结果不对：%+v", got)
	}
	if got.EndsAt != 1789866000 {
		t.Errorf("ends_at 应解析成 1789866000，实际 %d", got.EndsAt)
	}

	// camelCase 变体
	//
	// ⚠ 这条最初**红了** —— 我第一版只认 `status` / `claim_status`，
	// 漏了 `claimStatus`。那是真实响应里出现过的拼写，漏掉会让
	// 界面把"可领取"显示成空白状态。测试的价值就在这里。
	q := map[string]any{"planId": "plan-b", "claimStatus": "CLAIMED", "endsAt": float64(1)}
	got2 := PlanOffer{
		PlanID: firstNonEmpty(strOf(q, "plan_id"), strOf(q, "planId")),
		Status: firstNonEmpty(strOf(q, "status"), strOf(q, "claim_status"), strOf(q, "claimStatus")),
		EndsAt: toInt64(firstAny(q["ends_at"], q["endsAt"])),
	}
	if got2.PlanID != "plan-b" || got2.Status != "CLAIMED" || got2.EndsAt != 1 {
		t.Errorf("camelCase 变体解析不对：%+v", got2)
	}
}
