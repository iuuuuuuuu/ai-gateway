package qoder

// 权益活动查询的**形状契约**。
//
// # 为什么这几条测试重要
//
// 我对这个功能下过一次**错误的结论**：按 `qoder.com.cn` + cookie 查询
// 得到 `campaigns:[]`，于是告诉所有者"该账号没有活动" ——
// 而他的截图里明明有「每天领 100 Credits」。
//
// 根因是三处请求形状全错（host / 鉴权 / 缺 Cosy-ClientType），
// 而错的时候服务端**仍回 200**，只是数据为空。
//
// 所以这里要钉住的是**请求形状**，而不是"能解析 JSON"：
// 形状错了不会报错，只会静默返回空 —— 那正是最难发现的失败模式。

import (
	"context"
	"encoding/json"
	"strings"
	"testing"
)

// parseCampaignsForTest 解析活动响应（避免为纯解析测试起 HTTP 服务）。
func parseCampaignsForTest(t *testing.T, raw string) *CampaignStatus {
	t.Helper()
	var st CampaignStatus
	if err := json.Unmarshal([]byte(raw), &st); err != nil {
		t.Fatalf("解析失败: %v", err)
	}
	return &st
}

// TestCampaignsSendsAllThreeRequiredParts 三个必需项一个都不能少。
//
// 实测对照（uitest/requery-qoder-campaigns-correct.cjs）：
//
//	openapi.qoder.com.cn + Bearer                    → campaigns=0
//	openapi.qoder.com.cn + Bearer + Cosy-ClientType  → campaigns=2
//
// 少任何一个都**不报错**，只是空清单。
func TestCampaignsSendsAllThreeRequiredParts(t *testing.T) {
	c := New()
	cr := &Cred{UID: "u1", DT: "dt-token", Region: RegionCN}

	req, err := c.buildCampaignRequest(context.Background(), cr, "")
	if err != nil {
		t.Fatal(err)
	}

	// 1) 鉴权：Bearer（**不是** cookie）
	if got := req.Header.Get("Authorization"); got != "Bearer dt-token" {
		t.Errorf("Authorization 应为 Bearer <token>，实际 %q", got)
	}
	// 2) Cosy-ClientType：缺它上游回空清单且**不报错**（最隐蔽的一处）
	if got := req.Header.Get("Cosy-ClientType"); got != campaignClientType {
		t.Errorf("Cosy-ClientType 必须是 %q（缺它上游回空清单且不报错），实际 %q",
			campaignClientType, got)
	}
	// 3) host 必须走 openapi（不是 OpenAPI() 那个）
	if !strings.Contains(req.URL.Host, "openapi.") {
		t.Errorf("活动请求应打到 openapi.* host，实际 %q", req.URL.Host)
	}
	if got := req.Header.Get("Accept"); got != "application/json" {
		t.Errorf("Accept 应为 application/json，实际 %q", got)
	}
}

// TestCampaignsParsesRealShape 用**真实抓到的响应**验证解析。
//
// 形状取自 2026-09-19 的实际响应（字段名逐字抄，没有简化）：
// 两个活动分别是"可领取"与"已领取"，覆盖两种 claimStatus。
func TestCampaignsParsesRealShape(t *testing.T) {
	raw := `{
	  "uid":"019f1772-4976-7393-b4d4-b93ab04b7fe7",
	  "showCampaign":true,
	  "claimable":true,
	  "campaignUrl":"https://openapi.qoder.com.cn/growth-page/activity-iframe",
	  "campaigns":[
	    {"campaignId":"01a0b4aa-fbd4-720f-87bc-7eb1c6c9ddb6",
	     "campaignKey":"act-20260918-628",
	     "actionType":"CLAIM_BENEFIT",
	     "startAt":1789783200,"endAt":1789869540,
	     "claimStatus":"CLAIMABLE",
	     "benefit":{"kind":"CREDITS","amount":100,"validity":{"mode":"RELATIVE_DAYS","days":30}}},
	    {"campaignId":"01a05bbf-5668-7031-83d6-91545f97ec05",
	     "campaignKey":"act-20260901-922",
	     "actionType":"VIEW_DETAILS",
	     "startAt":1788243600,"endAt":1790783940,
	     "claimStatus":"CLAIMED"}
	  ]}`

	st := parseCampaignsForTest(t, raw)

	if !st.ShowCampaign || !st.Claimable {
		t.Errorf("应解析出 showCampaign/claimable 为 true，实际 %+v", st)
	}
	if len(st.Campaigns) != 2 {
		t.Fatalf("应解析出 2 个活动，实际 %d", len(st.Campaigns))
	}

	first := st.Campaigns[0]
	if first.CampaignKey != "act-20260918-628" {
		t.Errorf("campaignKey 解析错：%q", first.CampaignKey)
	}
	if first.ClaimStatus != "CLAIMABLE" {
		t.Errorf("claimStatus 应为 CLAIMABLE，实际 %q", first.ClaimStatus)
	}
	if first.Benefit == nil {
		t.Fatal("benefit 不应为空（那是展示「每天领 100 Credits」的数据源）")
	}
	if first.Benefit.Kind != "CREDITS" || first.Benefit.Amount != 100 {
		t.Errorf("奖励应解析为 CREDITS×100，实际 %s×%v", first.Benefit.Kind, first.Benefit.Amount)
	}
	if first.Benefit.Validity == nil || first.Benefit.Validity.Days != 30 {
		t.Errorf("有效期应解析为 30 天，实际 %+v", first.Benefit.Validity)
	}
	// endAt 是 Unix **秒**（不是毫秒）—— 当毫秒用会得到 1970 年
	if first.EndAt < 1e9 || first.EndAt > 1e10 {
		t.Errorf("endAt 应是 Unix 秒，实际 %d", first.EndAt)
	}

	// 第二个活动：只看详情、已领取 —— 界面要能区分，不能都显示"可领取"
	second := st.Campaigns[1]
	if second.ActionType != "VIEW_DETAILS" {
		t.Errorf("第二个活动的 actionType 应为 VIEW_DETAILS，实际 %q", second.ActionType)
	}
	if second.ClaimStatus != "CLAIMED" {
		t.Errorf("第二个活动应已领取，实际 %q", second.ClaimStatus)
	}
}

// TestCampaignsEmptyIsNotAnError 空清单**不是错误**。
//
// 上游会因多种原因返回空（活动过期 / 已领完 / 该账号不符合条件），
// 那些都是正常状态。把它当错误会让界面显示一个吓人的红框，
// 而实际只是"暂时没得领"。
func TestCampaignsEmptyIsNotAnError(t *testing.T) {
	raw := `{"uid":"u","showCampaign":false,"claimable":false,"campaignUrl":"","campaigns":[]}`
	st := parseCampaignsForTest(t, raw)
	if len(st.Campaigns) != 0 {
		t.Errorf("应为空，实际 %d 条", len(st.Campaigns))
	}
	if st.ShowCampaign || st.Claimable {
		t.Error("空清单时 showCampaign/claimable 应为 false")
	}
}

// TestCampaignHostIsOpenAPIHost 活动 host **就是** OpenAPI host。
//
// ## 我一度以为是"host 错了"，实测证明不是
//
// 我前几轮用 `qoder.com.cn` 打活动接口，拿到空清单，于是判断 host 错了。
// 但实测 `Region.OpenAPI()` 的值**本来就是** `https://openapi.qoder.com.cn`
// —— 也就是说我一直打的是对的 host。
//
// 真正让清单为空的是**缺 `Cosy-ClientType` 头**（见
// TestCampaignsSendsAllThreeRequiredParts）。
//
// 那为什么我看到的错误信息里有 `qoder.com.cn`？因为我在探测脚本里
// **手写了 host 字符串**（抄了 `userInfo` 的域名），而没有走
// `Region.OpenAPI()`。教训：探测时也要用**代码里的取值**，
// 不要手抄 —— 手抄会引入一个自己都不知道的偏差。
//
// 这条测试钉住"两者相同"，防止有人反过来"优化"成另一个 host。
func TestCampaignHostIsOpenAPIHost(t *testing.T) {
	cn := &Cred{Region: RegionCN}
	intl := &Cred{Region: RegionIntl}

	if got := campaignHostOf(cn); got != cn.Region.OpenAPI() {
		t.Errorf("国服活动 host 应与 OpenAPI host 一致，实际 %q vs %q", got, cn.Region.OpenAPI())
	}
	if got := campaignHostOf(cn); !strings.Contains(got, "openapi.") {
		t.Errorf("国服活动 host 应含 openapi.，实际 %q", got)
	}
	if got := campaignHostOf(intl); !strings.Contains(got, "openapi.") {
		t.Errorf("国际版活动 host 应含 openapi.，实际 %q", got)
	}
	// 即便 host 相同，也**必须**带上 Cosy-ClientType —— 这是本功能的
	// 真正要害：少它不报错，只是静默返回空清单。
	c := New()
	req, err := c.buildCampaignRequest(context.Background(), cn, "")
	if err != nil {
		t.Fatal(err)
	}
	if req.Header.Get("Cosy-ClientType") == "" {
		t.Error("少 Cosy-ClientType 会静默返回空清单 —— 界面上看起来像「没有活动」")
	}
}
