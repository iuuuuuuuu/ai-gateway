package qoder

// 权益活动（Qoder 的「每天领 100 Credits」等）。
//
// # 这个功能是怎么找到的（走过的弯路值得记下来）
//
// 我第一轮按客户端 app.asar 里的 `/sash/api/v1/me/campaigns` 去查，
// 得到 `{"campaigns":[]}`，于是下结论说"该账号当前没有活动"。
//
// **那个结论是错的** —— 所有者随后发来截图，上面明明有
// 「每天领 100 Credits [领取] 22:25:46」。
//
// 根因是**探测脚本里三处都和客户端不一致**（对比客户端自己的日志
// `main.log`，逐字）：
//
//	客户端实际发的：              我探测脚本发的：
//	───────────────────────────  ──────────────────────────────
//	host:  openapi.qoder.com.cn  host:  qoder.com.cn（**手抄**的域名）
//	auth:  Authorization: Bearer  auth:  qoder_session_cookie（从 Firefox 挖的）
//	头:    Cosy-ClientType: 10    头:    （无）
//
// ⚠ 其中 host 那一处最值得记：`Region.OpenAPI()` 的**实际值本来就是**
// `https://openapi.qoder.com.cn` —— 代码一直是对的。我错在探测脚本里
// **手写了** host 字符串（抄了 userInfo 的域名 qoder.com.cn），
// 于是引入了一个自己都不知道的偏差，还据此推断"host 错了"。
//
// 真正让清单为空的是**缺 `Cosy-ClientType`**。
//
// 教训：**探测时也要用代码里的取值，不要手抄** —— 手抄出来的偏差
// 会污染整个排查方向（我为此多花了好几轮）。
//
// 补齐后立刻变成 `campaigns=2, claimable=true`。
//
// # 边界（**已更新**：领取可以做）
//
// 我一度判定"领取要人机验证，故不代领"。**那个判断是错的** ——
// 我把两个不同的端点搞混了：
//
//	/api/v1/zcode-plan/billing/claim   ← ZCode 的套餐申领，要
//	                                     X-Aliyun-Captcha-Verify-Param
//	/sash/api/v1/me/campaigns/{id}/claim ← Qoder 的活动领取，**不要验证码**
//
// 所有者实测后者（原话「领取接口,经过我实测,无需人机验证」）：
// 带 Bearer + Cosy-ClientType 直接 POST 就成功，返回
// `{"grantId":…,"status":"CLAIMED","replayed":false}`。
//
// 故本文件**实现领取**。它只是"领取我自己的每日权益"，
// 没有任何风控绕过 —— 用的是用户自己的令牌、打官方接口、
// 与官方客户端的行为完全一致。
//
// ⚠ 已于 2026-09-22 **改为支持自动领取**（所有者要求）。
//
// 此前的注释写着「不做自动化定时领取（那是替用户刷活动）」，理由是
// "会让账号表现出非人类的活动模式"。**那个理由站不住**：
//
//   - ZCode 的套餐自动领取**早就在跑**（`internal/zcode/claim_scheduler.go`），
//     同一个产品里两条相反的口径本身就不自洽；
//   - 领取动作与**用户点那个按钮完全同构**（同 host / 同头 / 同令牌），
//     一次 POST，没有验证码、没有风控绕过。
//
// 见 `ClaimCampaign` 上方的说明。

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"

	"workbuddy2api/internal/qoderclient"
)

// CampaignHost 活动接口的 host。
//
// ⚠ **就是** `cr.Region.OpenAPI()`（`https://openapi.qoder.com.cn`）。
//
// 我一度以为"host 错了"（因为用 `qoder.com.cn` 查得空清单），但那是
// 我在探测脚本里**手抄**域名造成的偏差 —— 代码里的 `Region.OpenAPI()`
// 本来就是对的。真正让清单为空的是缺 `Cosy-ClientType`。
//
// 保留这两个常量而不是直接写 `cr.Region.OpenAPI()`：国际版是
// `openapi.qoder.sh`（不同 TLD），显式写出来更清楚。
const CampaignHostCN = "https://openapi.qoder.com.cn"
const CampaignHostIntl = "https://openapi.qoder.sh"

// campaignClientType 活动接口要求的客户端类型。
//
// ⚠ **必须带上**。实测：同样的 host 与 Bearer，
//
//	不带 Cosy-ClientType → campaigns=0（200，看起来像"没有活动"）
//	带   Cosy-ClientType: 10 → campaigns=2（真实数据）
//
// 这个头的值来自客户端日志的 `clientType:10`。
const campaignClientType = "10"

// Campaign 一条权益活动。
type Campaign struct {
	CampaignID  string `json:"campaignId"`
	CampaignKey string `json:"campaignKey"`
	// ActionType "CLAIM_BENEFIT"（可领取）/ "VIEW_DETAILS"（只看详情）。
	ActionType string `json:"actionType"`
	// ClaimStatus "CLAIMABLE" / "CLAIMED" / ""（未知）。
	ClaimStatus string `json:"claimStatus"`
	StartAt     int64  `json:"startAt"`
	EndAt       int64  `json:"endAt"`
	Benefit     *struct {
		Kind     string  `json:"kind"`   // "CREDITS"
		Amount   float64 `json:"amount"` // 100
		Validity *struct {
			Mode string `json:"mode"` // "RELATIVE_DAYS"
			Days int    `json:"days"` // 30
		} `json:"validity"`
	} `json:"benefit"`
	Placements []struct {
		Type        string `json:"type"` // "POPUP"
		CampaignURL string `json:"campaignUrl"`
		Content     map[string]struct {
			ButtonText  string `json:"buttonText"`
			Description string `json:"description"`
			DetailURL   string `json:"detailUrl"`
		} `json:"content"`
	} `json:"placements"`
}

// CampaignStatus 活动总览。
type CampaignStatus struct {
	// ShowCampaign 上游是否要展示活动入口。
	ShowCampaign bool `json:"showCampaign"`
	// Claimable 是否有**可领取**的活动。
	Claimable bool `json:"claimable"`
	// CampaignURL 活动页（服务端渲染的 iframe 页面）。
	CampaignURL string `json:"campaignUrl"`
	// Campaigns 活动明细。
	Campaigns []Campaign `json:"campaigns"`
}

// campaignHostOf 该账号的活动接口 host。
//
// ⚠ **不是** `cr.Region.OpenAPI()`（`qoder.com.cn`）。实测两者行为不同，
// 见 CampaignHostCN 的说明。抽成函数是为了让测试能直接钉住这一点。
func campaignHostOf(cr *Cred) string {
	if cr != nil && cr.Region == RegionIntl {
		return CampaignHostIntl
	}
	return CampaignHostCN
}

// buildCampaignRequest 构造活动查询请求。
//
// 抽出来是为了让测试能**直接断言这些头都带上了** —— 那正是本功能
// 最容易静默出错的地方（少一个头 → 上游回**看似正常**的残缺清单）。
//
// # ⚠⚠ 2026-09-23 实测修正：还缺两个头，且它们缺一不可
//
// 原实现只发 `Authorization` / `Accept` / `User-Agent` / `Cosy-ClientType`，
// 并注释说"三件套缺一不可"。**那个判断不完整** —— 所有者报
// 「国内的有这个领取任务，国际版怎么只剩下一个活动了，这是bug吧?」，
// 而他用**官方国际版客户端**打开时明明有两个活动。
//
// 逐个头部做二分实测（`openapi.qoder.sh`，同一个国际版令牌）：
//
//	只 Cosy-ClientType                 → 1 条 [VIEW_DETAILS]      ← 我们原来的做法
//	+ Cosy-MachineOs                  → 1 条
//	+ Cosy-MachineToken               → 1 条
//	+ Cosy-MachineType                → 1 条
//	+ Cosy-MachineCode/Hostname/Id    → 1 条
//	Cosy-MachineToken + Cosy-MachineType → **2 条 [CLAIM_BENEFIT(CLAIMABLE), VIEW_DETAILS]**
//
// 即 **`Cosy-MachineToken` 与 `Cosy-MachineType` 必须同时在场**，
// 少任何一个都退回残缺清单。而且上游对残缺请求回的是 **HTTP 200 +
// 结构合法的 JSON**，只是活动列表少了一条 —— 不报错、不告警，
// 从响应本身完全看不出异常（这正是它藏了这么久的原因）。
//
// 对照官方客户端抓包（`Qoder CN.exe`，Reqable id=6）确认它发的头里有：
//
//	cosy-clienttype / cosy-machinecode / cosy-machinehostname /
//	cosy-machineid / cosy-machineos / cosy-machinetoken / cosy-machinetype /
//	cosy-version
//
// 我们只补**实测证明必需的那两个**：其余几个是客户端身份元数据，
// 实测加了不改变结果，补它们只会引入需要维护的假值。
//
// 数据来源：`Cred.MachineToken` / `Cred.MachineType`（登录时上游下发，
// 已持久化在凭证文件的 `machine.token` / `machine.type`）。
// 见 cred.go 里那三个字段的注释 —— 它们本来就是为 COSY 签名存的。
//
// ⚠ 空值时不发该头（而不是发空串）：上游对 `Cosy-MachineType: `（空值）
// 的判定未实测，发空串可能被当成"提供了但非法"，比不提供更糟。
func (c *Client) buildCampaignRequest(ctx context.Context, cr *Cred, host string) (*http.Request, error) {
	if host == "" {
		host = campaignHostOf(cr)
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, host+"/sash/api/v1/me/campaigns", nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Authorization", "Bearer "+cr.DT)
	req.Header.Set("Accept", "application/json")
	req.Header.Set("User-Agent", "Qoder/1.0")
	// ① 缺了它 → 上游回空清单（见文件头第一段教训）
	req.Header.Set("Cosy-ClientType", campaignClientType)
	// ②③ 缺任一 → 上游回**残缺清单**（少掉可领取的那条），200 且不报错。
	setMachineHeaders(ctx, req, cr)
	return req, nil
}

// setMachineHeaders 补上活动接口必需的**机器指纹**头。
//
// # 实测结论（2026-09-23，二分定位）
//
// `Cosy-MachineToken` 与 `Cosy-MachineType` **必须同时在场且值真实**：
//
//	只 Cosy-ClientType                          → 1 条活动
//	+ 伪造的 Cosy-MachineToken/Cosy-MachineType  → 1 条活动
//	+ **真实的** 那两个头                        → 2 条活动（含「每天领 100 Credits」）
//
// ⚠ 上游对残缺/伪造的请求回的是 **HTTP 200 + 结构合法的 JSON**，
// 只是少一条活动 —— 不报错、不告警。这是本功能最隐蔽的失败模式。
//
// # 真实值从哪来
//
// 由客户端自带的阿里云风控 SDK（`runtime-info.exe`）产出，见
// `qoderclient.MachineIdentityOf`。**不再用 `Cred.MachineToken`** ——
// 那个字段是 `EnsureFingerprint` 本地编造的（`hexShort(32)` + 固定 `"5"`），
// 国服 host 不校验所以一直没暴露，而国际版 host 严格校验三元组。
//
// 抽成函数是为了让查询与领取两条路径**共用同一组头** ——
// 分别手写必然会漂移（本文件历史上已经因为"三件套"写漏过头）。
func setMachineHeaders(ctx context.Context, req *http.Request, cr *Cred) {
	if cr == nil {
		return
	}
	id := qoderclient.MachineIdentityOf(ctx)
	if !id.Usable() {
		// 拿不到就**不发**（而不是发空串或假值）：
		// 空串可能被上游当成"提供了但非法"，假值则确定无效。
		// 退回"降级但可用"的老行为，至少不会更坏。
		return
	}
	req.Header.Set("Cosy-MachineToken", id.MachineToken)
	req.Header.Set("Cosy-MachineType", id.MachineType)
	// Code 实测不是必需（二分时只补 Token+Type 就够），
	// 但官方客户端发了它，且它是同一组身份的一部分 —— 一并带上。
	req.Header.Set("Cosy-MachineCode", id.MachineCode)
}

// FetchCampaigns 查询该账号的权益活动（**只读**）。
func (c *Client) FetchCampaigns(ctx context.Context, cr *Cred) (*CampaignStatus, error) {
	if cr == nil || cr.DT == "" {
		return nil, fmt.Errorf("账号没有可用令牌")
	}

	ctx, cancel := context.WithTimeout(ctx, c.timeout())
	defer cancel()

	req, err := c.buildCampaignRequest(ctx, cr, campaignHostOf(cr))
	if err != nil {
		return nil, err
	}

	resp, err := c.http().Do(req)
	if err != nil {
		return nil, fmt.Errorf("查询权益活动失败: %w", err)
	}
	defer resp.Body.Close()

	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if resp.StatusCode >= 400 {
		return nil, fmt.Errorf("查询权益活动失败（HTTP %d）：%s", resp.StatusCode, truncate(string(raw), 200))
	}

	var st CampaignStatus
	if err := json.Unmarshal(raw, &st); err != nil {
		return nil, fmt.Errorf("活动响应解析失败: %w（原始内容：%s）", err, truncate(string(raw), 160))
	}
	return &st, nil
}

// ClaimResult 领取结果。
type ClaimResult struct {
	GrantID string `json:"grantId"`
	Status  string `json:"status"` // "CLAIMED"
	// Replayed true = 这次是**重放**（之前已领过），不是新领到。
	//
	// ⚠ 必须把它透出给用户：否则重复点"领取"会让人以为又领了一份。
	Replayed    bool   `json:"replayed"`
	CampaignID  string `json:"campaignId"`
	CampaignKey string `json:"campaignKey"`
	ClaimedAt   string `json:"claimedAt"`
	GrantedAt   string `json:"grantedAt"`
}

// ClaimCampaign 领取一个权益活动（**用自己的令牌打官方接口**）。
//
// # 为什么可以做（我此前误判为"不能做"）
//
// 我把两个端点搞混了：
//
//	/api/v1/zcode-plan/billing/claim      ← ZCode 的套餐申领，要验证码
//	/sash/api/v1/me/campaigns/{id}/claim  ← 本函数，**不要验证码**
//
// 所有者实测后者直接返回 `{"status":"CLAIMED"}`。它只是"领取我自己的
// 每日权益"，与官方客户端点那个「领取」按钮**完全同构** ——
// 同样的 host、同样的头、同样的令牌。没有任何风控绕过。
//
// # 自动领取（2026-09-22 所有者要求后改为支持）
//
// 此前的注释写着「不写定时任务替用户每天自动领……领取**只能由用户显式触发**」。
// **该结论已撤销**，理由：
//
//   - 实测该端点**不需要任何验证码**（见文件头），与用户点按钮同构；
//   - 活动**每天 10:00 (UTC+8) 重置**、时限约 22 小时，
//     不自动领就**直接过期作废** —— 这对用户是净损失；
//   - ZCode 的自动领取早已在跑，两条产品线口径必须一致。
//
// 批量入口：`claim-all-campaigns`（`internal/qoder/logincli.go`）。
func (c *Client) ClaimCampaign(ctx context.Context, cr *Cred, campaignID string) (*ClaimResult, error) {
	if cr == nil || cr.DT == "" {
		return nil, fmt.Errorf("账号没有可用令牌")
	}
	return c.claimCampaignAt(ctx, cr, campaignHostOf(cr), campaignID)
}

// claimCampaignAt 是 ClaimCampaign 的可注入 host 版本（供测试用 httptest 拦截）。
//
// 抽出来的唯一目的是**让测试能断言请求头真的发出去了** —— 本功能的历史缺陷
// 全是"头写漏了但响应仍 200"（见 buildCampaignRequest 的注释），
// 而那种缺陷只有把请求拦下来看头才能测到。
func (c *Client) claimCampaignAt(ctx context.Context, cr *Cred, host, campaignID string) (*ClaimResult, error) {
	if cr == nil || cr.DT == "" {
		return nil, fmt.Errorf("账号没有可用令牌")
	}
	campaignID = strings.TrimSpace(campaignID)
	if campaignID == "" {
		return nil, fmt.Errorf("活动 ID 不能为空")
	}

	ctx, cancel := context.WithTimeout(ctx, c.timeout())
	defer cancel()

	if host == "" {
		host = campaignHostOf(cr)
	}
	url := host + "/sash/api/v1/me/campaigns/" + url.PathEscape(campaignID) + "/claim"
	// 官方请求是 POST + 空 body（Content-Length: 0）。
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, url, nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Authorization", "Bearer "+cr.DT)
	req.Header.Set("Accept", "application/json")
	req.Header.Set("User-Agent", "Qoder/1.0")
	req.Header.Set("Cosy-ClientType", campaignClientType)
	req.Header.Set("Origin", host)
	// 官方带 referer 指向活动页；服务端未必校验，但照抄更安全。
	req.Header.Set("Referer", host+"/growth-page/activity-iframe")
	// ⚠ 领取请求同样要补这两个头（与查询同源，见 buildCampaignRequest 的注释）：
	// 查询少了它们会拿到**残缺清单**（看不到那条可领的活动），
	// 而领取少了它们同样有被上游降级的风险 —— 两条路径用同一组头才不会分叉。
	setMachineHeaders(ctx, req, cr)

	resp, err := c.http().Do(req)
	if err != nil {
		return nil, fmt.Errorf("领取权益失败: %w", err)
	}
	defer resp.Body.Close()

	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if resp.StatusCode >= 400 {
		return nil, fmt.Errorf("领取权益失败（HTTP %d）：%s", resp.StatusCode, truncate(string(raw), 200))
	}

	var r ClaimResult
	if err := json.Unmarshal(raw, &r); err != nil {
		return nil, fmt.Errorf("领取响应解析失败: %w（原始内容：%s）", err, truncate(string(raw), 160))
	}
	// `status` 不是 CLAIMED 时**不当作成功** —— 上游可能用 200 表达
	// "这次没领到"（如活动已结束但详情还在）。把那种情况当成功会让
	// 界面显示"领取成功"而实际没有。
	if r.Status != "CLAIMED" {
		return nil, fmt.Errorf("上游返回的状态不是已领取：%s", truncate(string(raw), 160))
	}
	return &r, nil
}
