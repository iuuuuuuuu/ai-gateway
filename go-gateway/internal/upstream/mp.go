// mp.go 小程序（mp）口径的 growth 域写操作：报名 / 领奖。
//
// 背景：小程序限定任务（school_season、Sequential_Tasks_1/_2/_3）的**报名**
// 要求请求头 X-Client-Platform: miniprogram。缺这个头时上游返回
// HTTP 200 + code=0，但逐条结果是 {"status":"error","message":"task not found"} ——
// 200 的壳里装着「任务不存在」，最容易误判成「任务已下线」而放弃。
//
// 实测（2026-09-30，真实国服账号，同一账号同一天）：
//
//	默认口径 accept Sequential_Tasks_1 → results[0] = {status:error, message:"task not found"}
//	加 mp 头 accept  Sequential_Tasks_1 → results[0] = {status:accepted}
//
// ★ 列表回读**不需要** mp 口径，故本文件刻意**不**提供 ListTasksMP：
// 不发 UA 的列表（ListGrowthTasks → growthJSONNoUA）本来就会下发这些任务
//（实测同一账号：no-UA 20 项，含 Sequential_Tasks_1 的 accept_status 与 progress）。
// mp 头会把列表裁剪到小程序视角的 10 项，且**叠加 no-UA 也恢复不到全量**
//（实测 mp+noUA 仍是 10 项）—— 也就是说 mp 头与 no-UA 不是可叠加的两个开关，
// 加 mp 头就是换了一份列表。既然回读用 no-UA 已经能看到全部任务，
// 再引入一份 mp 列表只会多一个口径、多一处不一致。
//
// 因此 mp 口径只用于 accept / claim 这两个**写**操作。
package upstream

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"

	"workbuddy2api/internal/auth"
)

// mpPlatform 小程序口径的 X-Client-Platform 取值。
//
// ⚠ 不要与事件上报的头族混用：report.go 的 ReportMPChatActivity /
// ReportMPExpertUse 走 report 域，那里的 X-Client-Platform 是 "mp-weixin"。
// 两个值分别属于 growth 域任务接口与 report 域事件通道，互换任一方向都会
// 让上游按另一种来源解析（表现为 200 但不计分 / 任务 not found）。
const mpPlatform = "miniprogram"

// growthJSONMP 发一次带小程序口径头的 growth 域请求。
//
// 与 growthJSON 的唯一差别是那一行 X-Client-Platform；请求体与鉴权头族完全一致
//（BillingHeaders），因为小程序调的就是同一个接口。
func (c *Client) growthJSONMP(a *auth.Auth, method, path string, body any) (json.RawMessage, error) {
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
	req.Header.Set("X-Client-Platform", mpPlatform)
	return c.doJSON(a, req)
}

// AcceptTasksMP 以小程序口径报名一批任务，返回上游对每个任务的处理结果。
//
// 与 AcceptGrowthTasks 的关系：同端点、同请求体，唯一差别是 mp 头。
// 刻意**不**让 AcceptGrowthTasks 内部自动补头：批量报名里通常混着普通任务，
// 给整个批次加 mp 头会把普通任务也按小程序口径处理 —— 那是另一种输入，
// 上游按它可能给出不同结果（实测 mp 列表是默认口径的子集，不是超集）。
//
// 返回值与 AcceptGrowthTasks 同形，调用方据此逐条核对 status：
// 对 mp 任务而言「HTTP 200 但 status=error」是真实存在的失败形态，
// 只看 error 会把这种失败当成成功。
func (c *Client) AcceptTasksMP(a *auth.Auth, codes []string) ([]AcceptResult, error) {
	if len(codes) == 0 {
		return nil, nil
	}
	data, err := c.growthJSONMP(a, http.MethodPost, growthTasksAcceptPath,
		map[string]any{"task_codes": codes})
	if err != nil {
		return nil, err
	}
	var resp struct {
		Results []AcceptResult `json:"results"`
	}
	// 与 AcceptGrowthTasks 同策略：结果解析失败不算失败，报名请求本身已成功。
	_ = json.Unmarshal(data, &resp)
	return resp.Results, nil
}

// ClaimRewardMP 领取小程序任务的奖励，mp 域失败时回落 Web 域。
//
// 为什么 mp 优先：小程序任务在 chat 域（与报名同一个基址）领奖，
// Web 域对它们是另一种来源。面板实现（参考实现 autotask.go 的 ClaimRewardMP）
// 同样是 mp 优先、400 回落 Web。
//
// 为什么回落只认 4xx：5xx 是上游故障，换个域重试并不会好；而 4xx
//（含「task not completed」与「task not found」）意味着这个域不接受这次领奖，
// 换 Web 域是有意义的下一步 —— 两个域对同一任务的可见性并不总是相同。
//
// 注意：回落**不**改变「进度未达标」的判定语义 —— ClaimGrowthTask 返回的
// 错误同样可以被 IsGrowthTaskNotCompleted 识别，调用方看到的仍是同一种错误。
func (c *Client) ClaimRewardMP(a *auth.Auth, taskCode string) (credit, energy int64, already bool, err error) {
	if strings.TrimSpace(taskCode) == "" {
		return 0, 0, false, nil
	}
	path := fmt.Sprintf(growthTaskClaimPathFmt, url.PathEscape(taskCode))
	data, err := c.growthJSONMP(a, http.MethodPost, path, nil)
	if err == nil {
		return parseClaimReward(data)
	}
	if ue, ok := err.(*Error); !ok || ue.Status < 400 || ue.Status >= 500 {
		return 0, 0, false, err
	}
	return c.ClaimGrowthTask(a, taskCode)
}

// parseClaimReward 解领奖响应，语义与 ClaimGrowthTask 的解析保持一致。
func parseClaimReward(data json.RawMessage) (credit, energy int64, already bool, err error) {
	var resp struct {
		AlreadyClaimed bool  `json:"already_claimed"`
		Credit         int64 `json:"credit"`
		Energy         int64 `json:"energy"`
	}
	if err := json.Unmarshal(data, &resp); err != nil {
		return 0, 0, false, err
	}
	if resp.AlreadyClaimed {
		return 0, 0, true, nil
	}
	return resp.Credit, resp.Energy, false, nil
}
