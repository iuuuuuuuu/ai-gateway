package zcode

// 套餐识别的契约测试。
//
// # 为什么这几条值得写
//
// 所有者给的判据很具体：
//
//	「这个体验套餐是 9月23号23:59 过期时间，这是我刚登录的新账号赠送的
//	  额度，要区分好」
//	「glm5.3 3000000 额度，glm5.3flash 5000000 额度」
//
// 而**体验套餐与付费套餐在额度上可以长得一样**（都是"若干 token"）。
// 界面上把体验套餐显示成普通额度，用户会以为自己还有几个月可用，
// 而实际三天后归零 —— 那是最容易让人误判的一种展示。
//
// 故这里钉住：识别判据、不硬编码上游数字、拿不到配置时**不猜**。

import "testing"

// trialFixture 上游真实的 startPlanPreview（字段名逐字抄）。
func trialFixture() *StartPlanPreview {
	return &StartPlanPreview{
		Name:   "Start Plan",
		PlanID: "zcode-v3-start-plan",
		Entitlements: []StartPlanEntitlement{
			{ShowName: "GLM-5.3", GrantUnits: 3000000, Period: "daily", UnitType: "token"},
			{ShowName: "GLM-5.3-Flash", GrantUnits: 5000000, Period: "daily", UnitType: "token"},
		},
	}
}

// TestClassifyTrialPlan 实测的两条 zai 账号就是体验套餐。
//
// 数据来自 2026-09-19 真实响应：
//
//	entries: GLM-5.3        total=3000000  expiresAt=1789833599
//	         GLM-5.3-Flash  total=5000000  expiresAt=1789833599
//
// 两者与 startPlanPreview 的 grantUnits **完全相同**。
func TestClassifyTrialPlan(t *testing.T) {
	q := &Quota{Entries: []QuotaEntry{
		{ShowName: "GLM-5.3", Total: 3000000, Remaining: 3000000, ExpiresAt: 1789833599},
		{ShowName: "GLM-5.3-Flash", Total: 5000000, Remaining: 5000000, ExpiresAt: 1789833599},
	}}
	if got := ClassifyPlan(q, trialFixture()); got != PlanTrial {
		t.Errorf("应识别为体验套餐，实际 %q", got)
	}
	if got := PlanLabel(PlanTrial); got != "体验套餐" {
		t.Errorf("展示名应为「体验套餐」，实际 %q", got)
	}
}

// TestClassifyPaidPlan 付费套餐：总量与赠送量不同。
//
// 实测那条 bigmodel 账号 total=300000000（3 亿），远大于赠送量。
func TestClassifyPaidPlan(t *testing.T) {
	q := &Quota{Entries: []QuotaEntry{
		{ShowName: "GLM-5.3-Flash", Total: 300000000, Remaining: 299999978, ExpiresAt: 1789866000},
	}}
	if got := ClassifyPlan(q, trialFixture()); got != PlanPaid {
		t.Errorf("总量 3 亿远大于赠送量，应识别为付费套餐，实际 %q", got)
	}
}

// TestClassifyPartialMatchIsPaid 只要**有一个**模型对不上，就不算体验套餐。
//
// 为什么用"全部相等"而不是"任一相等"：
// 付费套餐的模型清单里**可能正好包含**体验套餐送的那几个模型
//（上游的模型池是重叠的）。用"任一相等"会把付费套餐误判成体验套餐，
// 于是界面显示"三天后到期"—— 而用户其实是长期订阅。
//
// 误判的代价（吓用户一跳、可能让他白白续费）大于漏判的代价
//（只是没标出"体验"）。
func TestClassifyPartialMatchIsPaid(t *testing.T) {
	q := &Quota{Entries: []QuotaEntry{
		// 这个命中赠送量
		{ShowName: "GLM-5.3", Total: 3000000},
		// 这个不命中（付费套餐额外的大额度）
		{ShowName: "GLM-5.3-Flash", Total: 300000000},
	}}
	if got := ClassifyPlan(q, trialFixture()); got != PlanPaid {
		t.Errorf("有一个模型对不上就不该判为体验套餐，实际 %q", got)
	}
}

// TestClassifyNoConfigDoesNotGuess 拿不到上游配置时**不猜**。
//
// 返回 unknown，界面据此**不显示**到期警告。猜成"体验套餐"会给
// 长期订阅的用户一个假的到期日；猜成"付费套餐"则让体验用户
// 错过"快到期了"的提醒。两种猜错都比"不知道"更糟。
func TestClassifyNoConfigDoesNotGuess(t *testing.T) {
	q := &Quota{Entries: []QuotaEntry{
		{ShowName: "GLM-5.3", Total: 3000000},
	}}
	if got := ClassifyPlan(q, nil); got != PlanUnknown {
		t.Errorf("没有配置时应返回 unknown（不猜），实际 %q", got)
	}
	// 空赠送清单同理
	if got := ClassifyPlan(q, &StartPlanPreview{}); got != PlanUnknown {
		t.Errorf("空赠送清单时应返回 unknown，实际 %q", got)
	}
}

// TestClassifyEmptyQuotaIsAPIKey 没有额度明细 = 按量付费。
//
// 纯 API Key 的账号没有"预发额度"这个概念，`balances` 是空的。
// 这与"查询失败"不同（失败会返回 error，不是空 Quota）。
func TestClassifyEmptyQuotaIsAPIKey(t *testing.T) {
	if got := ClassifyPlan(&Quota{}, trialFixture()); got != PlanAPIKey {
		t.Errorf("空额度应是按量付费，实际 %q", got)
	}
	if got := ClassifyPlan(nil, trialFixture()); got != PlanUnknown {
		t.Errorf("nil 额度应是 unknown，实际 %q", got)
	}
}

// TestClassifyDoesNotHardcodeGrantUnits 判据来自配置，**不硬编码**上游数字。
//
// 3000000 / 5000000 是**运营参数**，上游随时可改。如果把这两个数写进
// 代码，上游一改赠送量，我们就会把所有体验套餐误判成付费套餐
//（并因此不再提醒用户"快到期了"）。
//
// 这条测试把赠送量换成一组完全不同的数字，验证仍能正确识别。
func TestClassifyDoesNotHardcodeGrantUnits(t *testing.T) {
	// 假设上游把赠送量改成 7M / 9M
	other := &StartPlanPreview{
		Name: "Start Plan", PlanID: "zcode-v3-start-plan",
		Entitlements: []StartPlanEntitlement{
			{ShowName: "GLM-5.3", GrantUnits: 7000000},
			{ShowName: "GLM-5.3-Flash", GrantUnits: 9000000},
		},
	}
	q := &Quota{Entries: []QuotaEntry{
		{ShowName: "GLM-5.3", Total: 7000000},
		{ShowName: "GLM-5.3-Flash", Total: 9000000},
	}}
	if got := ClassifyPlan(q, other); got != PlanTrial {
		t.Errorf("赠送量改了也要认得出来（说明判据来自配置而非硬编码），实际 %q", got)
	}

	// 反过来：旧的 3M/5M 在**新配置**下不该再被认成体验套餐
	old := &Quota{Entries: []QuotaEntry{
		{ShowName: "GLM-5.3", Total: 3000000},
		{ShowName: "GLM-5.3-Flash", Total: 5000000},
	}}
	if got := ClassifyPlan(old, other); got != PlanPaid {
		t.Errorf("旧赠送量在新配置下不该判为体验套餐，实际 %q", got)
	}
}

// TestSameModelName 模型名比对要容忍写法差异。
//
// 上游不同接口的大小写与连字符写法可能不同（`GLM-5.3-Flash` vs
// `glm-5.3-flash` vs `GLM 5.3 Flash`）。严格相等会漏判，
// 于是体验套餐被当成付费套餐。
func TestSameModelName(t *testing.T) {
	same := [][2]string{
		{"GLM-5.3", "glm-5.3"},
		{"GLM-5.3-Flash", "GLM-5.3-Flash"},
		{"GLM-5.3-Flash", "glm 5.3 flash"},
		{"GLM-5.3-Flash", "GLM_5.3_Flash"},
	}
	for _, p := range same {
		if !sameModelName(p[0], p[1]) {
			t.Errorf("%q 与 %q 应视为同一模型", p[0], p[1])
		}
	}
	diff := [][2]string{
		{"GLM-5.3", "GLM-5.3-Flash"},
		{"GLM-5.3", "GLM-5.2"},
		{"GLM-5-Turbo", "GLM-5.3"},
	}
	for _, p := range diff {
		if sameModelName(p[0], p[1]) {
			t.Errorf("%q 与 %q 不该视为同一模型", p[0], p[1])
		}
	}
}

// TestPlanNoteMentionsExpiryLoss 体验套餐的说明必须点出"到期会失效"。
//
// 这是用户最容易误判的地方：他看到"还有 800 万"，会以为那是攒着的余额。
// 而体验额度是**每日重置**且**到期作废**的。
func TestPlanNoteMentionsExpiryLoss(t *testing.T) {
	note := PlanNote(PlanTrial)
	for _, want := range []string{"每日重置", "失效"} {
		if !contains(note, want) {
			t.Errorf("体验套餐说明应提到 %q，实际：%s", want, note)
		}
	}
}

func contains(s, sub string) bool {
	return len(sub) == 0 || (len(s) >= len(sub) && indexOf(s, sub) >= 0)
}

func indexOf(s, sub string) int {
	for i := 0; i+len(sub) <= len(s); i++ {
		if s[i:i+len(sub)] == sub {
			return i
		}
	}
	return -1
}
