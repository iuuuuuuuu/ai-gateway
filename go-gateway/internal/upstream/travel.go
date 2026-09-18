// travel.go growth 域「猫猫旅行」接口：状态查询 / 派出 / 领奖 / 领养 / 协议。
// 全部走 chatBase（copilot.tencent.com，不带 /v2 前缀）+ BillingHeaders，信封同 doJSON。
package upstream

import (
	"bytes"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"strings"

	"workbuddy2api/internal/auth"
)

// growth 域路径（实测）。
const (
	travelStatusPath   = "/activity/growth/buddy/travel/status"
	travelDepartPath   = "/activity/growth/buddy/travel/depart"
	travelClaimPath    = "/activity/growth/buddy/travel/claim"
	buddyInfoPath      = "/activity/growth/buddy/info"
	buddyFirstPath     = "/activity/growth/buddy/first"
	buddyAgreementPath = "/activity/growth/buddy/agreement"
	streakPath         = "/activity/growth/streak"
)

// GrowthStreak 回读连登天数。
//
// 用途是**上报自检**：活跃上报返回 200 不代表真的计分 —— 缺 userId 时服务端
// 200 但静默丢弃，连登天数不动。回读是唯一能发现「静默失败」的手段。
func (c *Client) GrowthStreak(a *auth.Auth) (int, error) {
	data, err := c.growthJSON(a, http.MethodGet, streakPath, nil)
	if err != nil {
		return 0, err
	}
	var resp struct {
		Streak struct {
			Days int `json:"days"`
		} `json:"streak"`
	}
	if err := json.Unmarshal(data, &resp); err != nil {
		return 0, err
	}
	return resp.Streak.Days, nil
}

// buddyTaskIncompleteMarker 领养门槛未达标的业务错误关键词（HTTP 400 时出现）。
const buddyTaskIncompleteMarker = "first_buddy task not completed yet"

// Buddy 账号当前猫档案；nil（data.buddy 为 null）表示无猫。
type Buddy struct {
	ID   int64  `json:"id"`
	Name string `json:"name"`
}

// TravelState 猫猫旅行状态。
type TravelState struct {
	State             string `json:"state"`               // idle / traveling / arrived
	DailyLimitReached bool   `json:"daily_limit_reached"` // 今日已派出过（自然日 00:00 CST 重置）
	RecordID          int64  `json:"record_id"`           // 在途/到站记录 id，claim 必带
	RewardCredit      int64  `json:"reward_credit"`       // 到站可领奖励积分
}

// growthJSON 发 growth 域请求并解信封；body 为 nil 时不带请求体。
// 错误语义与 doJSON 一致：HTTP 非 2xx / 业务 code != 0 → *Error。
func (c *Client) growthJSON(a *auth.Auth, method, path string, body any) (json.RawMessage, error) {
	return c.growthJSONOpt(a, method, path, body, false)
}

// growthJSONNoUA 同 growthJSON，但**不发 User-Agent**。
//
// 为什么需要它（2026-09-18 实测，真实账号 + 逐 UA 对照）：
// 上游按 User-Agent 判定「请求来自哪个客户端平台」，并据此过滤任务清单。
// 我们此前所有 growth 请求都不设 UA，而 Go 的 net/http 会**自动补**
// `Go-http-client/1.1` —— 上游把这个 UA 当作未知平台，于是把「小程序限定」的
// 两个任务（school_season、Sequential_Tasks_1）过滤掉：只返回 18 项。
//
// 「上游按 UA 裁剪任务清单」这条线索最初来自 Sliverkiss/workbuddy2api 的
// e45f39f（MIT）；但下面的逐 UA 对照、以及「不发 UA 才拿到完整 20 项」这一
// 结论，是本项目用真实账号独立实测得出的（该 commit 并未涉及此现象）。
//
// 实测对照（同一账号、同一端点，唯一变量是 UA）：
//
//	Go-http-client/1.1（默认）→ 18 项，无小程序任务
//	WorkBuddy/CLI 客户端 UA   → 18 项，无小程序任务
//	小程序 UA                 → 10 项，含小程序任务（但少了 8 个其它任务）
//	**不发 UA（本函数）**      → 20 项，含小程序任务 **且** 保留全部其它任务
//
// 也就是说「不发 UA」是唯一能拿到完整 20 项的口径。这不是绕过什么限制 ——
// 服务端对未声明平台的请求返回的是**全量**清单，反而是声明了平台才会被按平台裁剪。
//
// 只对任务列表用：其它 growth 端点（签到、旅行、领奖）没有这个行为，
// 全局去掉 UA 会改变它们的指纹，属无谓的改动面扩大。
func (c *Client) growthJSONNoUA(a *auth.Auth, method, path string, body any) (json.RawMessage, error) {
	return c.growthJSONOpt(a, method, path, body, true)
}

// growthJSONOpt growthJSON 的实现，omitUA 控制是否抑制 User-Agent。
func (c *Client) growthJSONOpt(a *auth.Auth, method, path string, body any, omitUA bool) (json.RawMessage, error) {
	var rdr io.Reader
	if body != nil {
		raw, err := json.Marshal(body)
		if err != nil {
			return nil, err
		}
		rdr = bytes.NewReader(raw)
	}
	req, err := http.NewRequest(method, c.chatBase(a)+path, rdr)
	if err != nil {
		return nil, err
	}
	BillingHeaders(req, a)
	if omitUA {
		// **必须用 Set("") 而不是留空**：留空时 Go 的 net/http 会自动补
		// `Go-http-client/1.1`，正是要避开的那个值。
		// Set("") 之后 wire 上仍会出现 `User-Agent: `（空值），实测上游对
		// 空值 UA 与「无 UA」的处理一致（都是 20 项）。
		req.Header.Set("User-Agent", "")
	}
	return c.doJSON(a, req)
}

// TravelStatus 查询猫猫旅行状态。
func (c *Client) TravelStatus(a *auth.Auth) (*TravelState, error) {
	data, err := c.growthJSON(a, http.MethodGet, travelStatusPath, nil)
	if err != nil {
		return nil, err
	}
	var st TravelState
	if err := json.Unmarshal(data, &st); err != nil {
		return nil, err
	}
	return &st, nil
}

// TravelDepart 派出猫旅行；locationID 实测 1~4（收益/时长区间相同）。
func (c *Client) TravelDepart(a *auth.Auth, locationID int) error {
	_, err := c.growthJSON(a, http.MethodPost, travelDepartPath, map[string]any{"location_id": locationID})
	return err
}

// TravelClaim 领取到站奖励，返回 reward_credit。
func (c *Client) TravelClaim(a *auth.Auth, recordID int64) (int64, error) {
	data, err := c.growthJSON(a, http.MethodPost, travelClaimPath, map[string]any{"record_id": recordID})
	if err != nil {
		return 0, err
	}
	var resp struct {
		RewardCredit int64 `json:"reward_credit"`
	}
	if len(data) > 0 {
		// 奖励字段缺失不视为失败：调用方按 0 记日志即可。
		_ = json.Unmarshal(data, &resp)
	}
	return resp.RewardCredit, nil
}

// BuddyInfo 查询当前猫档案；返回 (nil, nil) 表示无猫（data.buddy 为 null）。
func (c *Client) BuddyInfo(a *auth.Auth) (*Buddy, error) {
	data, err := c.growthJSON(a, http.MethodGet, buddyInfoPath, nil)
	if err != nil {
		return nil, err
	}
	var resp struct {
		Buddy json.RawMessage `json:"buddy"`
	}
	if err := json.Unmarshal(data, &resp); err != nil {
		return nil, err
	}
	// null / 缺字段 / 空对象都按无猫处理。
	trimmed := strings.TrimSpace(string(resp.Buddy))
	if trimmed == "" || trimmed == "null" {
		return nil, nil
	}
	var b Buddy
	if err := json.Unmarshal(resp.Buddy, &b); err != nil {
		return nil, err
	}
	return &b, nil
}

// BuddyFirst 领养第一只猫。无猫且已过 conversation 门槛时送 300 分。
// 门槛未达标返回 HTTP 400（见 IsBuddyTaskIncomplete），属预期行为，调用方静默跳过。
func (c *Client) BuddyFirst(a *auth.Auth) error {
	_, err := c.growthJSON(a, http.MethodPost, buddyFirstPath, map[string]any{})
	return err
}

// BuddyAgreement 同意协议（幂等，重复调用无副作用）。
func (c *Client) BuddyAgreement(a *auth.Auth) error {
	_, err := c.growthJSON(a, http.MethodPost, buddyAgreementPath, map[string]any{"agree": true})
	return err
}

// IsBuddyTaskIncomplete 判定「领养门槛未达标」：HTTP 400 + first_buddy 关键词。
// 该错误当日不应重试（避免对上游重试轰炸）。
func IsBuddyTaskIncomplete(err error) bool {
	if err == nil {
		return false
	}
	var ue *Error
	if !errors.As(err, &ue) || ue.Status != http.StatusBadRequest {
		return false
	}
	return strings.Contains(strings.ToLower(ue.Msg), buddyTaskIncompleteMarker)
}
