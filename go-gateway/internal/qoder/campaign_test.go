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
	"net/http"
	"net/http/httptest"
	"os"
	"strings"
	"testing"

	"workbuddy2api/internal/qoderclient"
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

// TestCampaignsSendsMachineFingerprintHeaders 活动查询必须带**机器指纹**头。
//
// # 这是一个真实缺陷的回归测试（2026-09-23）
//
// 所有者报：「国内的有这个领取任务，国际版怎么只剩下一个活动了，这是bug吧?」
// 而他用官方国际版客户端打开时**明明有两个活动**（含「每天领 100 Credits」）。
//
// 根因：`buildCampaignRequest` 只发了 Authorization/Accept/User-Agent/
// Cosy-ClientType，**缺 `Cosy-MachineToken` / `Cosy-MachineType` / `Cosy-MachineCode`**。
//
// 二分实测（openapi.qoder.sh，同一个国际版令牌）：
//
//	只 Cosy-ClientType                     → 1 条 [VIEW_DETAILS]
//	真实的 MachineToken + MachineType      → 2 条 [CLAIM_BENEFIT(CLAIMABLE), VIEW_DETAILS]
//
// ⚠ 上游对残缺请求回的是 **HTTP 200 + 结构合法的 JSON**，只是少一条活动 ——
// 不报错、不告警。所以**只有断言"头发出去了"才能拦住它**：
// 断言"解析没报错"或"拿到了活动"都拦不住（残缺响应同样满足）。
//
// # ⚠⚠ 本测试依赖本机装了 Qoder 客户端
//
// 指纹来自客户端自带的 `runtime-info.exe`（见 qoderclient.MachineIdentityOf）。
// 没装客户端时拿不到值 ⇒ 本测试**跳过**（而不是失败）：
// 那是一个**已知且可接受**的降级，不是回归。
func TestCampaignsSendsMachineFingerprintHeaders(t *testing.T) {
	id := qoderclient.MachineIdentityOf(context.Background())
	if !id.Usable() {
		t.Skip("本机没有 Qoder 客户端的 runtime-info.exe ⇒ 拿不到真实指纹（已知降级，非回归）")
	}

	c := New()
	cr := &Cred{UID: "u1", DT: "dt-token", Region: RegionIntl}

	req, err := c.buildCampaignRequest(context.Background(), cr, "")
	if err != nil {
		t.Fatal(err)
	}

	if got := req.Header.Get("Cosy-MachineToken"); got != id.MachineToken {
		t.Errorf("缺/错了 Cosy-MachineToken：期望 %q，实际 %q\n"+
			"缺它时上游回**残缺清单**（少掉可领取的那条），HTTP 200 且不报错",
			id.MachineToken, got)
	}
	if got := req.Header.Get("Cosy-MachineType"); got != id.MachineType {
		t.Errorf("缺/错了 Cosy-MachineType：期望 %q，实际 %q\n"+
			"它与 Cosy-MachineToken **必须同时在场**，少任一个都会退回残缺清单",
			id.MachineType, got)
	}
	if got := req.Header.Get("Cosy-MachineCode"); got != id.MachineCode {
		t.Errorf("缺/错了 Cosy-MachineCode：期望 %q，实际 %q", id.MachineCode, got)
	}
}

// TestClaimCampaignAlsoSendsMachineHeaders 领取请求与查询请求**共用同一组头**。
//
// 两条路径分别手写头必然会漂移 —— 本文件历史上就因为"三件套"写漏过一次。
// 这里用 httptest 把请求拦下来，钉住领取路径也走 `setMachineHeaders`。
func TestClaimCampaignAlsoSendsMachineHeaders(t *testing.T) {
	id := qoderclient.MachineIdentityOf(context.Background())
	if !id.Usable() {
		t.Skip("本机没有 Qoder 客户端的 runtime-info.exe ⇒ 拿不到真实指纹（已知降级，非回归）")
	}

	c := New()
	cr := &Cred{UID: "u1", DT: "dt-token", Region: RegionIntl}

	var got *http.Request
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		got = r
		_, _ = w.Write([]byte(`{"grantId":"g1","status":"CLAIMED","replayed":false}`))
	}))
	defer srv.Close()

	if _, err := c.claimCampaignAt(context.Background(), cr, srv.URL, "camp-1"); err != nil {
		t.Fatalf("领取失败：%v", err)
	}
	if got == nil {
		t.Fatal("请求没发出去")
	}
	if v := got.Header.Get("Cosy-MachineToken"); v != id.MachineToken {
		t.Errorf("领取请求缺 Cosy-MachineToken：期望 %q，实际 %q", id.MachineToken, v)
	}
	if v := got.Header.Get("Cosy-MachineType"); v != id.MachineType {
		t.Errorf("领取请求缺 Cosy-MachineType：期望 %q，实际 %q", id.MachineType, v)
	}
	if v := got.Header.Get("Cosy-ClientType"); v != campaignClientType {
		t.Errorf("领取请求缺 Cosy-ClientType：期望 %q，实际 %q", campaignClientType, v)
	}
}

// TestMachineHeadersAreNeverFabricated 拿不到真实指纹时**不发**（而不是编一个）。
//
// # 这是本缺陷的核心教训
//
// 原实现用 `Cred.MachineToken` —— 那是 `EnsureFingerprint` 本地编造的
// `hexShort(32)` + 固定 `"5"`。国服 host 不校验所以一直没暴露，
// 而国际版 host 严格校验 ⇒ 编造的值**确定无效**，且会让排查方向跑偏
// （看起来"头发了"，实际发的是垃圾）。
//
// 本测试钉住：`setMachineHeaders` 的值只能来自 `MachineIdentityOf`，
// **不得**读 `Cred.MachineToken/MachineType`。
//
// 用源码级断言：行为断言需要伪造"拿不到指纹"的环境，
// 而进程级缓存（sync.Once）在测试里不好重置。
func TestMachineHeadersAreNeverFabricated(t *testing.T) {
	src, err := os.ReadFile("campaign.go")
	if err != nil {
		t.Fatal(err)
	}
	s := string(src)

	if strings.Contains(s, "cr.MachineToken") || strings.Contains(s, "cr.MachineType") {
		t.Error("setMachineHeaders 不得读 Cred.MachineToken/MachineType ——\n" +
			"那两个字段是 EnsureFingerprint 本地编造的（hexShort(32) + 固定 \"5\"），\n" +
			"对严格校验的 host（国际版）确定无效。真实值只能来自\n" +
			"qoderclient.MachineIdentityOf（客户端自带的 runtime-info.exe）")
	}
	if !strings.Contains(s, "qoderclient.MachineIdentityOf") {
		t.Error("setMachineHeaders 应调用 qoderclient.MachineIdentityOf 取真实指纹")
	}
}

// TestCampaignsParsesRealShape 用**真实抓到的响应**验证解析。
//
// 形状取自 2026-09-19 的实际响应（字段名逐字抄，没有简化）：
// 两个活动分别是"可领取"与"已领取"，覆盖两种 claimStatus。
func TestCampaignsParsesRealShape(t *testing.T) {
	raw := `{
	  "uid":"{uuid}",
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
