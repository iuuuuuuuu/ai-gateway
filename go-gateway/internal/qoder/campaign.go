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
// # 边界（刻意不做的事）
//
// **不实现"代领"**。领取要 `X-Aliyun-Captcha-Verify-Param`（阿里云验证码），
// 那是服务端的防滥用机制。本文件只做**只读查询** + 给出活动页地址，
// 由用户自己去官方页面点「领取」。
//
// 这不是能力不足，是**边界**：绕过验证码等于帮用户破坏服务端的风控。

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
)

// CampaignHost 活动接口的 host。
//
// ⚠ **不是** `cr.Region.OpenAPI()`（那是 `qoder.com.cn`）。
// 实测两者行为不同：
//
//	openapi.qoder.com.cn + Bearer + Cosy-ClientType → campaigns=2 ✓
//	qoder.com.cn         + Bearer                   → 401 missing cookie header
//
// 客户端日志里 `origin` 字段写的就是 openapi.qoder.com.cn。
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
