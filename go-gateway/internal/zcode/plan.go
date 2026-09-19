package zcode

// 套餐类型的识别（体验套餐 / 付费套餐 / 纯 API Key）。
//
// # 为什么需要它（所有者的要求）
//
//	「这个体验套餐是 9月23号23:59 过期时间，这是我刚登录的新账号赠送的
//	  额度，要区分好」
//	「glm5.3 3000000 额度，glm5.3flash 5000000 额度，你可以参照这个写逻辑」
//
// 也就是说：**同样显示"有 800 万 token"，含义可能完全不同** ——
//
//	体验套餐（Start Plan）：新账号赠送，**每日重置**，且有到期日
//	                       到期后额度归零，不是"用完了"
//	付费套餐（Coding Plan）：按周发放积分，随订阅续期
//	纯 API Key：按量付费，没有"额度"概念
//
// 不区分的话，界面会说"你还有 800 万额度"，而用户其实**三天后就没有了**
// —— 那是最容易让人误判的一种展示。
//
// # 判据：拿上游**静态配置**的赠送量去比对（不猜、不硬编码）
//
// `GET /api/v1/client/configs` 里的 `configs.startPlanPreview` 写明了
// 体验套餐每天送多少：
//
//	{"name":"Start Plan","planId":"zcode-v3-start-plan",
//	 "entitlements":[
//	   {"grantUnits":3000000,"period":"daily","showName":"GLM-5.3","unitType":"token"},
//	   {"grantUnits":5000000,"period":"daily","showName":"GLM-5.3-Flash","unitType":"token"}]}
//
// 实测（2026-09-19）两条 zai 账号的 `billing/balance`：
//
//	GLM-5.3        total=3000000   ← 与 grantUnits **完全相同**
//	GLM-5.3-Flash  total=5000000   ← 完全相同
//
// 故判据是：**每个模型的总量都等于体验套餐的赠送量** → 体验套餐。
//
// ⚠ 为什么用"全部相等"而不是"总量 <= 赠送量"：
// 付费套餐的总额通常**大于**赠送量；用 `<=` 会把刚订阅但还没用的小额
// 付费套餐误判成体验套餐。用 `==` 虽然可能漏判（上游改了赠送量），
// 但漏判只是"没标出体验套餐"，而误判会给出**错误的到期警告** ——
// 后者的代价大得多。
//
// ⚠ 也**不硬编码 3000000 / 5000000**：那两个数字是上游可改的运营参数。
// 本文件从配置里读，故上游改了赠送量我们会自动跟上。

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strings"
	"sync"
	"time"
)

// PlanKind 套餐类型。
type PlanKind string

const (
	// PlanUnknown 判断不出来（没查配置、或额度结构与赠送量不符）。
	PlanUnknown PlanKind = "unknown"
	// PlanTrial 体验套餐（新账号赠送，每日重置，**有到期日**）。
	PlanTrial PlanKind = "trial"
	// PlanPaid 付费套餐（Coding Plan）。
	PlanPaid PlanKind = "paid"
	// PlanAPIKey 纯 API Key（按量付费，没有额度概念）。
	PlanAPIKey PlanKind = "api_key"
)

// StartPlanEntitlement 体验套餐在**某个模型**上的每日赠送量。
type StartPlanEntitlement struct {
	ShowName string `json:"showName"`
	// GrantUnits 每日赠送量（token 数）。
	GrantUnits float64 `json:"grantUnits"`
	// Period 重置周期，实测是 `daily`。
	Period   string `json:"period"`
	UnitType string `json:"unitType"`
}

// StartPlanPreview 体验套餐的静态描述。
type StartPlanPreview struct {
	Name         string                 `json:"name"`
	PlanID       string                 `json:"planId"`
	Entitlements []StartPlanEntitlement `json:"entitlements"`
}

// 静态配置的缓存。
//
// 为什么缓存：它是一份**运营配置**（不含账号信息），每次查额度都打一次
// 上游是浪费，且会给上游增加无谓负载。一个小时足够新。
var (
	startPlanMu   sync.Mutex
	startPlanVal  *StartPlanPreview
	startPlanAt   time.Time
	startPlanTTL  = time.Hour
	startPlanFail error
)

// FetchStartPlanPreview 取体验套餐的静态描述（带缓存）。
func (c *Client) FetchStartPlanPreview(ctx context.Context) (*StartPlanPreview, error) {
	startPlanMu.Lock()
	if startPlanVal != nil && time.Since(startPlanAt) < startPlanTTL {
		v := startPlanVal
		startPlanMu.Unlock()
		return v, nil
	}
	startPlanMu.Unlock()

	ctx, cancel := context.WithTimeout(ctx, 20*time.Second)
	defer cancel()

	url := "https://zcode.z.ai/api/v1/client/configs?app_version=" + DefaultAppVersion + "&platform=win32-x64"
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return nil, err
	}
	// 这些头是官方客户端带的（见所有者的实测抓包）。缺 X-Device-Mid 等
	// 会被上游以 parameter error 拒绝，故一并带上。
	req.Header.Set("User-Agent", "ZCode/"+DefaultAppVersion)
	req.Header.Set("X-ZCode-App-Version", DefaultAppVersion)
	req.Header.Set("X-Platform", "win32-x64")
	req.Header.Set("X-Release-Channel", "production")
	req.Header.Set("Accept", "application/json")

	resp, err := c.http().Do(req)
	if err != nil {
		startPlanMu.Lock()
		startPlanFail = err
		startPlanMu.Unlock()
		return nil, fmt.Errorf("取体验套餐配置失败: %w", err)
	}
	defer resp.Body.Close()

	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 8<<20))
	if resp.StatusCode >= 400 {
		return nil, fmt.Errorf("取体验套餐配置失败（HTTP %d）", resp.StatusCode)
	}

	var doc struct {
		Code int `json:"code"`
		Data struct {
			Configs struct {
				StartPlanPreview *StartPlanPreview `json:"startPlanPreview"`
			} `json:"configs"`
		} `json:"data"`
	}
	if err := json.Unmarshal(raw, &doc); err != nil {
		return nil, fmt.Errorf("体验套餐配置解析失败: %w", err)
	}
	sp := doc.Data.Configs.StartPlanPreview
	if sp == nil || len(sp.Entitlements) == 0 {
		return nil, fmt.Errorf("上游配置里没有 startPlanPreview")
	}

	startPlanMu.Lock()
	startPlanVal = sp
	startPlanAt = time.Now()
	startPlanMu.Unlock()
	return sp, nil
}

// ClassifyPlan 判断该额度是哪一类套餐。
//
// 需要一个 `StartPlanPreview`（可为 nil —— 那时只能判断 api_key）。
//
// 判据见文件头：**每个模型的总量都等于体验套餐的赠送量** → 体验套餐。
func ClassifyPlan(q *Quota, sp *StartPlanPreview) PlanKind {
	if q == nil {
		return PlanUnknown
	}
	// 没有任何额度明细 → 按量付费（API Key 那种没有"额度"）
	if len(q.Entries) == 0 {
		return PlanAPIKey
	}

	// 配置拿不到 → 不能判断是体验还是付费。
	// 返回 unknown 而不是猜一个 —— 猜错会给用户错误的到期警告。
	if sp == nil || len(sp.Entitlements) == 0 {
		return PlanUnknown
	}

	// 每个模型都要能在赠送清单里找到**总量完全相同**的一项
	matched := 0
	for _, e := range q.Entries {
		for _, g := range sp.Entitlements {
			// 名字比对放宽大小写与连字符：上游写 `GLM-5.3-Flash`，
			// 而额度接口的 showName 也可能是 `GLM-5.3-Flash` 或变体。
			if !sameModelName(e.ShowName, g.ShowName) {
				continue
			}
			// 总量完全相同才算（理由见文件头：用 == 不用 <=）
			if float64(e.Total) == g.GrantUnits && e.Total > 0 {
				matched++
			}
			break
		}
	}
	// 所有明细都命中赠送量 → 体验套餐
	if matched == len(q.Entries) {
		return PlanTrial
	}
	return PlanPaid
}

// sameModelName 模型名是否指同一个模型（宽松比对）。
//
// 上游不同接口的大小写/连字符写法可能不同，严格相等会漏判。
func sameModelName(a, b string) bool {
	norm := func(s string) string {
		s = strings.ToLower(strings.TrimSpace(s))
		s = strings.ReplaceAll(s, "-", "")
		s = strings.ReplaceAll(s, "_", "")
		s = strings.ReplaceAll(s, " ", "")
		return s
	}
	return norm(a) == norm(b)
}

// PlanLabel 套餐类型的中文展示名。
func PlanLabel(k PlanKind) string {
	switch k {
	case PlanTrial:
		return "体验套餐"
	case PlanPaid:
		return "付费套餐"
	case PlanAPIKey:
		return "按量付费"
	default:
		return "未知"
	}
}

// PlanNote 一句话说明该套餐的**关键约束**（界面悬浮提示用）。
//
// 体验套餐必须说清"到期后会归零"与"每日重置" —— 这正是用户最容易
// 误判的地方（以为额度是"攒着的"）。
func PlanNote(k PlanKind) string {
	switch k {
	case PlanTrial:
		return "新账号赠送的体验额度：每日重置，且**到期后未用完的部分会失效**，不是被用完了。"
	case PlanPaid:
		return "付费套餐额度：按订阅周期发放与续期。"
	case PlanAPIKey:
		return "按量付费（API Key）：没有预发额度，按实际用量计费。"
	default:
		return "未能识别套餐类型（可能是上游配置取不到）。"
	}
}
