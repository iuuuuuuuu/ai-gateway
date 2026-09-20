package zcode

// claim.go ZCode 套餐**领取**（`billing/preview` + `billing/claim`）。
//
// # 为什么需要它（额度是这么来的）
//
// 实测确认（2026-09-19）我们的账号额度挂在 **start-plan** 上：
//
//	planId    = "zcode-v3-start-plan-wk-0918"
//	remaining = 299999978 / 300000000
//
// 而这类套餐是**限时活动、先到先得**（服务端 `1005 quota_exhausted` 封顶）。
// 不主动领就没有额度 —— 用户看到的是"明明有账号却报余额不足"。
//
// # 一个我们注释里没有的关键点：`X-Device-Mid`
//
// 参考实现 `TriDefender/zcode-api`（MIT）记录：
//
//	「the gateway again rejects preview with biz 3001 'parameter error'
//	  unless the request carries a UUID-format `X-Device-Mid`」
//
// **我们实测证实了这一点**（`uitest/diag-zcode-claim.cjs`）：
//
//	不带 X-Device-Mid → HTTP 400 code=3001 parameter error
//	带   X-Device-Mid → HTTP 200 {"code":0,"data":{"plans":[]}}
//
// 好消息：我们**已经有**这个头（`Identity.ControlPlaneHeaders`），
// 本模块直接复用，不新增机制。
//
// # 错误码语义（照参考实现 + 我们的实测）
//
//	1001 not_found          套餐不存在（活动未上线）
//	1002 unavailable        当前不可领
//	1003 already_claimed    已经领过了（**不是错误**）
//	1004 ineligible         账号无资格
//	1005 quota_exhausted    名额已满（先到先得，被别人领完了）
//	3001 invalid_request    缺参数（最常见是缺 X-Device-Mid）
//	3007 captcha            需要验证码
//	401  login_required     凭证失效
//
// # ⚠ 领取是**写操作**，默认不自动执行
//
// `ClaimPlan` 只被显式调用（用户在界面上点"领取"，或 CLI 子命令）。
// **不做定时自动抢** —— 那与"用户点一下"不是一回事，且会让账号表现出
// 非人类的活动模式（与 Qoder campaign 同一取舍，见 qoder/campaign.go）。

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"
)

// PlanOffer 一个**可领取**的套餐（preview 的返回项）。
type PlanOffer struct {
	PlanID string `json:"planId"`
	Name   string `json:"name"`
	Status string `json:"status"`
	// EndsAt 该套餐的领取截止（Unix 秒；0 = 未提供）
	EndsAt int64 `json:"endsAt"`
	// Raw 原始对象，便于界面展示未解析字段
	Raw map[string]any `json:"-"`
}

// PlanPreview preview 端点的结果。
type PlanPreview struct {
	// ServerTime 上游服务器时间（Unix 秒）——
	// 用它算"还剩多久截止"，而不是用本机时钟：本机时钟偏差会让倒计时不准。
	ServerTime int64 `json:"serverTime"`
	// Offers 可领的套餐（空 = 当前没有活动）
	Offers []PlanOffer `json:"offers"`
	// Raw 原始 data 对象
	Raw map[string]any `json:"-"`
}

// ClaimResult 领取结果。
type ClaimResult struct {
	// OK 是否成功领到（already_claimed 也算"已达成目标"，故 OK=true）
	OK bool `json:"ok"`
	// Code 上游业务码
	Code string `json:"code"`
	// Msg 上游文案（原样保留，便于排查）
	Msg string `json:"msg"`
	// AlreadyClaimed 之前已领过（**不是错误**）
	AlreadyClaimed bool `json:"alreadyClaimed"`
	// ClaimedAt 领取时间（Unix 秒；0 = 未提供）
	ClaimedAt int64 `json:"claimedAt"`
}

// previewPath 可领套餐列表端点。
//
// ⚠ 与 `FetchQuota` 一样必须走 **QuotaHost**（zcode.z.ai）——
// 两个服务商的 billing 端点都只在那一个 host 上有数据。
const previewPath = "/api/v1/zcode-plan/billing/preview"

// claimPath 领取端点。
const claimPath = "/api/v1/zcode-plan/billing/claim"

// FetchPlanPreview 查**可领取**的套餐。
//
// # 为什么与 FetchQuota 是两个端点
//
//	FetchQuota      → billing/balance  **已生效**的套餐与余额
//	FetchPlanPreview→ billing/preview  **可领取**的套餐（活动）
//
// 前者回答"我现在有多少"，后者回答"我还能领什么"。混用会得到
// "有额度但领不了"或"能领但看不到额度"这类误导性结论。
func (c *Client) FetchPlanPreview(ctx context.Context, cr *Cred) (*PlanPreview, error) {
	if cr == nil {
		return nil, fmt.Errorf("账号为空")
	}
	// ⚠ 必须带 query 参数：实测不带 app_version/platform 会回 3001
	q := fmt.Sprintf("%s?app_version=%s&platform=%s",
		previewPath, url.QueryEscape(c.Identity.AppVersion), url.QueryEscape(c.Identity.PlatformArch()))

	req, err := http.NewRequestWithContext(ctx, http.MethodGet,
		cr.Provider.QuotaHost()+q, nil)
	if err != nil {
		return nil, err
	}
	// ⚠ **极简头**（见 Identity.PreviewHeaders 的说明）：参考实现的 preview
	// 只发 `Authorization`（+ 活动期的 `X-Device-Mid`），**不发**身份头 bundle。
	//
	// 我此前给 preview 发的是 `ControlPlaneHeaders` 整套（含 User-Agent /
	// X-Title / HTTP-Referer / X-Os-* / X-ZCode-Agent）—— 而上游风控按
	// "是否与真机一致"判断，多发反而是可区分特征。
	for k, v := range c.Identity.PreviewHeaders(cr.DeviceMid) {
		if v != "" {
			req.Header.Set(k, v)
		}
	}
	// Authorization：有 JWT 才发。缺 JWT 时**一个头都不发** ——
	// 匿名 preview 在参考实现里是合法的（会拿到空清单而不是 401）。
	if jwt := strings.TrimSpace(cr.JWT); jwt != "" {
		req.Header.Set("Authorization", "Bearer "+jwt)
	}

	resp, err := c.http().Do(req)
	if err != nil {
		return nil, fmt.Errorf("查询可领套餐失败: %w", err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))

	if resp.StatusCode >= 400 {
		return nil, &Error{
			Kind:   Classify(resp.StatusCode, string(body)),
			Status: resp.StatusCode,
			Code:   codeOf(body),
			Msg:    msgOf(body),
		}
	}

	var doc struct {
		Code int    `json:"code"`
		Msg  string `json:"msg"`
		Data struct {
			ServerTime int64            `json:"server_time"`
			Plans      []map[string]any `json:"plans"`
		} `json:"data"`
	}
	if err := json.Unmarshal(body, &doc); err != nil {
		return nil, fmt.Errorf("解析可领套餐响应失败: %w（原始：%.200s）", err, body)
	}
	if doc.Code != 0 {
		return nil, &Error{
			Kind:   Classify(resp.StatusCode, string(body)),
			Status: resp.StatusCode,
			Code:   fmt.Sprintf("%d", doc.Code),
			Msg:    doc.Msg,
		}
	}

	out := &PlanPreview{ServerTime: doc.Data.ServerTime}
	for _, p := range doc.Data.Plans {
		out.Offers = append(out.Offers, PlanOffer{
			PlanID: firstNonEmpty(strOf(p, "plan_id"), strOf(p, "planId")),
			Name:   strOf(p, "name"),
			// ⚠ 三种拼写都要认（snake / camel / 带 claim 前缀）——
			// 只认两种会漏掉 camelCase 的 claimStatus，而那是**实际
			// 响应里出现过**的一种（我的单测正是抓到这个才补上的）。
			Status: firstNonEmpty(
				strOf(p, "status"),
				strOf(p, "claim_status"),
				strOf(p, "claimStatus"),
			),
			EndsAt: toInt64(firstAny(p["ends_at"], p["endsAt"])),
			Raw:    p,
		})
	}
	// 保留原始 data 供界面展示未解析字段
	var raw map[string]any
	if err := json.Unmarshal(body, &struct {
		Data *map[string]any `json:"data"`
	}{Data: &raw}); err == nil {
		out.Raw = raw
	}
	return out, nil
}

// ClaimPlan 领取一个套餐。
//
// # ⚠ 这是**写操作**
//
// 只应由用户在界面上显式点击（或 CLI 子命令）触发。
// **不做定时自动抢** —— 理由见文件头。
//
// # 验证码
//
// 该端点可能要求 `X-Aliyun-Captcha-Verify-Param`（上游回 3007）。
// 若 `cr.CaptchaParam` 为空而上游要求验证码，会如实返回 `ErrCaptchaRequired`——
// 由调用方决定要不要先求解一个（求解器见 captcha.go）。
func (c *Client) ClaimPlan(ctx context.Context, cr *Cred, planID string) (*ClaimResult, error) {
	if cr == nil {
		return nil, fmt.Errorf("账号为空")
	}
	planID = strings.TrimSpace(planID)
	if planID == "" {
		return nil, fmt.Errorf("套餐 ID 不能为空")
	}

	payload, err := json.Marshal(map[string]string{"plan_id": planID})
	if err != nil {
		return nil, err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost,
		cr.Provider.QuotaHost()+claimPath, strings.NewReader(string(payload)))
	if err != nil {
		return nil, err
	}
	for k, v := range c.Identity.ClaimHeaders(cr.DeviceMid) {
		if v != "" {
			req.Header.Set(k, v)
		}
	}
	req.Header.Set("Content-Type", "application/json")
	// Authorization：有 JWT 才发（与参考实现一致）。
	//
	// ⚠ 这条路径用 **JWT**，不是 `x-api-key` —— 参考实现的 claim 客户端
	// 不发 `x-api-key`（那是对话面的双头做法，两条路径不同）。
	if jwt := strings.TrimSpace(cr.JWT); jwt != "" {
		req.Header.Set("Authorization", "Bearer "+jwt)
	}
	// 验证码头（仅在已求到时）—— 与对话路径同一取舍：没有就不发，
	// 让上游如实回 3007，而不是我们猜。
	if cr.CaptchaParam != "" {
		req.Header.Set("X-Aliyun-Captcha-Verify-Param", cr.CaptchaParam)
		if cr.CaptchaRegion != "" {
			req.Header.Set("X-Aliyun-Captcha-Verify-Region", cr.CaptchaRegion)
		}
	}

	resp, err := c.http().Do(req)
	if err != nil {
		return nil, fmt.Errorf("领取套餐失败: %w", err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))

	code := codeOf(body)
	msg := msgOf(body)

	// `1003 already_claimed` 不是错误 —— 目标（拿到该套餐）已经达成。
	// 若当错误报，用户重复点会以为每次都失败。
	if code == "1003" {
		return &ClaimResult{OK: true, Code: code, Msg: msg, AlreadyClaimed: true}, nil
	}
	if resp.StatusCode >= 400 || (code != "" && code != "0") {
		return nil, &Error{
			Kind:   Classify(resp.StatusCode, string(body)),
			Status: resp.StatusCode,
			Code:   code,
			Msg:    msg,
		}
	}

	var doc struct {
		Data struct {
			ClaimedAt int64 `json:"claimed_at"`
		} `json:"data"`
	}
	_ = json.Unmarshal(body, &doc)
	return &ClaimResult{OK: true, Code: code, Msg: msg, ClaimedAt: doc.Data.ClaimedAt}, nil
}

// strOf 取字符串字段（缺失/类型不对时返回空串）。
func strOf(m map[string]any, key string) string {
	if v, ok := m[key].(string); ok {
		return v
	}
	return ""
}

// codeOf / msgOf 从上游响应体里挖业务码与文案。
//
// 上游形状有两种（都实测见过）：
//
//	{"code":3001,"msg":"parameter error"}          ← 控制面
//	{"error":{"code":"1113","message":"余额不足"}}  ← 对话面
//
// 故两个都要认，否则会把"有业务码的错误"当成"没有码"，进而
// 用 HTTP 状态码兜底分类 —— 那正是把 1113 误判成"限流"的根源。
func codeOf(body []byte) string {
	var a struct {
		Code  any `json:"code"`
		Error struct {
			Code any `json:"code"`
		} `json:"error"`
	}
	if json.Unmarshal(body, &a) != nil {
		return ""
	}
	if s := anyToCode(a.Code); s != "" && s != "0" {
		return s
	}
	return anyToCode(a.Error.Code)
}

func msgOf(body []byte) string {
	var a struct {
		Msg   string `json:"msg"`
		Error struct {
			Message string `json:"message"`
		} `json:"error"`
	}
	if json.Unmarshal(body, &a) != nil {
		return ""
	}
	return firstNonEmpty(a.Msg, a.Error.Message)
}

// anyToCode 把业务码统一成字符串。
//
// ⚠ 上游有时发数字（`3001`）、有时发字符串（`"1113"`）——
// 只认一种会漏判（实测两种都存在）。
func anyToCode(v any) string {
	switch t := v.(type) {
	case string:
		return strings.TrimSpace(t)
	case float64:
		// JSON 数字经 interface{} 解出来是 float64；
		// 用 %d 而不是 %v —— 后者会输出 "3001.0"
		return fmt.Sprintf("%d", int64(t))
	case int64:
		return fmt.Sprintf("%d", t)
	default:
		return ""
	}
}
