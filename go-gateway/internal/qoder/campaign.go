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
// ⚠ 仍然**不做**的事：不做自动化定时领取（那是替用户刷活动，
// 与"用户点一下领取"不是一回事）。

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"
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
		Kind   string  `json:"kind"`   // "CREDITS"
		Amount float64 `json:"amount"` // 100
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
// 抽出来是为了让测试能**直接断言三个头都带上了** —— 那正是本功能
// 最容易静默出错的地方（少一个头 → 上游回空清单且不报错）。
func (c *Client) buildCampaignRequest(ctx context.Context, cr *Cred, host string) (*http.Request, error) {
	if host == "" {
		host = campaignHostOf(cr)
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, host+"/sash/api/v1/me/campaigns", nil)
	if err != nil {
		return nil, err
	}
	// 三件套缺一不可，见文件头的说明。
	req.Header.Set("Authorization", "Bearer "+cr.DT)
	req.Header.Set("Accept", "application/json")
	req.Header.Set("User-Agent", "Qoder/1.0")
	req.Header.Set("Cosy-ClientType", campaignClientType)
	return req, nil
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
	GrantID  string `json:"grantId"`
	Status   string `json:"status"` // "CLAIMED"
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
// # 仍然不做自动化
//
// 不写定时任务替用户每天自动领。那与"用户点一下"不是一回事，
// 且会让账号表现出非人类的活动模式。领取**只能由用户显式触发**。
func (c *Client) ClaimCampaign(ctx context.Context, cr *Cred, campaignID string) (*ClaimResult, error) {
	if cr == nil || cr.DT == "" {
		return nil, fmt.Errorf("账号没有可用令牌")
	}
	campaignID = strings.TrimSpace(campaignID)
	if campaignID == "" {
		return nil, fmt.Errorf("活动 ID 不能为空")
	}

	ctx, cancel := context.WithTimeout(ctx, c.timeout())
	defer cancel()

	url := campaignHostOf(cr) + "/sash/api/v1/me/campaigns/" + url.PathEscape(campaignID) + "/claim"
	// 官方请求是 POST + 空 body（Content-Length: 0）。
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, url, nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Authorization", "Bearer "+cr.DT)
	req.Header.Set("Accept", "application/json")
	req.Header.Set("User-Agent", "Qoder/1.0")
	req.Header.Set("Cosy-ClientType", campaignClientType)
	req.Header.Set("Origin", campaignHostOf(cr))
	// 官方带 referer 指向活动页；服务端未必校验，但照抄更安全。
	req.Header.Set("Referer", campaignHostOf(cr)+"/growth-page/activity-iframe")

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
