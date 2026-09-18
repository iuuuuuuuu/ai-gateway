// report.go growth 域「对话活跃上报」：POST {billingBase}/v2/report。
//
// 一条上报同时点亮 growth 连登天数、解锁 first_buddy 任务（领养前置）。
// 事件形状照抄客户端 chat_request_send（全字段，勿用最小子集 —— 上游可能随时加严）。
//
// 关键约束：事件必须带 userId（=账号 uid）。缺它时服务端返回 200 但**静默丢弃**，
// 连登天数不动 —— 表现为「上报成功但没点亮」，极难排查。
// 因此调用方上报后要回读 streak 自检（见 scheduler.checkActivityStreak）。
//
// 实测标定（2026-09-15，19 个真实账号）：
//   - 国服：上报 13/13 成功；上报前部分账号 streak.days=0，上报 1 条后全部变 1
//     → 接口确实点亮连登，且**两种区域都接受**（国际版上报同样返回 code=0）
//   - 国际版：/activity/growth/streak 恒 500（该域在国际版不可用）
//     → 故活跃上报默认只跑国服（schedule.checkin_scope=cn），
//       与签到共用同一区域开关；scope=all 时才带上国际版
package upstream

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"time"

	"workbuddy2api/internal/auth"
)

// reportPath 活跃上报通道。
const reportPath = "/v2/report"

// chatRequestEvent 客户端 chat_request_send 事件的完整形状。
//
// 字段与官方客户端一致；conversationId 由调用方生成，无需真实会话
// （服务端不校验会话一致性）。
type chatRequestEvent struct {
	EventCode             string `json:"eventCode"`
	Timestamp             int64  `json:"timestamp"`
	ReportDelay           int    `json:"reportDelay"`
	Mode                  string `json:"mode"`
	ConversationID        string `json:"conversationId"`
	RequestID             string `json:"requestId"`
	InputLength           int    `json:"inputLength"`
	RequestModelID        string `json:"requestModelId"`
	RequestModelName      string `json:"requestModelName"`
	IsPlan                bool   `json:"isPlan"`
	IsAutoExecuteTerminal bool   `json:"isAutoExecuteTerminal"`
	IsAutoModify          bool   `json:"isAutoModify"`
	CodebaseEnable        bool   `json:"codebaseEnable"`
	MaxToken              int    `json:"maxToken"`
	MaxSteps              int    `json:"maxSteps"`
	Temperature           int    `json:"temperature"`
	MaxRetries            int    `json:"maxRetries"`
	MentionContexts       []any  `json:"mentionContexts"`
	KnowledgeID           []any  `json:"knowledgeId"`
	KnowledgeName         []any  `json:"knowledgeName"`
	CodebaseID            string `json:"codebaseId"`
	MentionContextCount   int    `json:"mentionContextCount"`
	Command               string `json:"command"`
	ExpertID              string `json:"expertId"`
	RecommendID           string `json:"recommendId"`
	SkillID               string `json:"skillId"`
	SkillCount            int    `json:"skillCount"`
	TotalCount            int    `json:"totalCount"`
	FileURI               string `json:"fileUri"`
	PresentAt             int64  `json:"presentAt"`
	TraceID               string `json:"traceId"`
	RootRequestID         string `json:"rootRequestId"`
	ParentConversationID  string `json:"parentConversationId"`
	AgentName             string `json:"agentName"`
	AgentType             string `json:"agentType"`
	UserID                string `json:"userId"`
}

// ReportChatActivity 发送一条对话活跃上报（chat_request_send）。
//
// conversationID 由调用方生成（形如 wb2api-<毫秒>）；同一会话的多条上报共用它，
// 但 requestID 必须各不相同（服务端按事件去重）。requestID 为空时回落到 conversationID。
//
// 错误语义与 doJSON 一致：HTTP 非 2xx 或业务 code != 0 → *Error。
func (c *Client) ReportChatActivity(a *auth.Auth, conversationID, requestID string) error {
	if requestID == "" {
		requestID = conversationID
	}
	now := time.Now().UnixMilli()
	ev := chatRequestEvent{
		EventCode:            "chat_request_send",
		Timestamp:            now,
		Mode:                 "craft",
		ConversationID:       conversationID,
		RequestID:            requestID,
		InputLength:          12,
		RequestModelID:       "deepseek-v4-flash",
		RequestModelName:     "DeepSeek V4 Flash",
		MentionContexts:      []any{},
		KnowledgeID:          []any{},
		KnowledgeName:        []any{},
		PresentAt:            now,
		RootRequestID:        conversationID,
		ParentConversationID: conversationID,
		AgentName:            "default",
		AgentType:            "conversation",
		UserID:               a.UID,
	}
	raw, err := json.Marshal([]chatRequestEvent{ev})
	if err != nil {
		return err
	}
	// 走 billingBase（国服 = codebuddy.cn，国际版 = workbuddy.ai），
	// 与 travel 的 growthJSON（chatBase）分属两个域，不可混用。
	_, err = c.billingJSON(a, http.MethodPost, reportPath, json.RawMessage(raw))
	return err
}

// SchoolSeasonActivityID 开学季/校园日活动的关联 id。
//
// 事件带上它才会被归到该活动；不带则**不计分**（上游实测）。
const SchoolSeasonActivityID = "school_open_day_2026"

// 校园日任务的集成本源于 Sliverkiss/workbuddy2api 的 e45f39f（MIT）：
// 该 commit 首次指出「school_season 是小程序专属下发的任务、判据为
// mini chat_request_send + activityId」，本仓库据此在 Go 侧重新实现并实测校准。
// 本文件的事件形状、activityId、以及下面的 UA 结论均由本项目独立实测确认。

// miniChatRequestEvent 小程序（微信容器）指纹的 chat_request_send 事件。
//
// 与桌面版 chatRequestEvent 的差异（都来自实测的上游小程序客户端形状）：
//
//	source     = "mini_program"     ← 服务端据此判定来自小程序
//	ideName    = "wx_app_cloud"
//	ideType    = "WorkBuddy_MP"
//	extName    = "workbuddy-mp"     ← 关键指纹，缺了不计分
//	extVersion = "SaaS"
//	mode       = "chat"
//	activityId = school_open_day_2026
//
// 桌面版那套字段（requestModelId / agentName / traceId 等）小程序**不发**，
// 因此这里刻意用独立结构体而不是给 chatRequestEvent 加字段 —— 后者会让桌面版
// 也带上小程序字段，把两种指纹混成一个（上游会看出异常）。
type miniChatRequestEvent struct {
	EventCode           string `json:"eventCode"`
	Timestamp           int64  `json:"timestamp"`
	ReportDelay         int    `json:"reportDelay"`
	Source              string `json:"source"`
	IDEName             string `json:"ideName"`
	IDEType             string `json:"ideType"`
	ExtName             string `json:"extName"`
	ExtVersion          string `json:"extVersion"`
	Mode                string `json:"mode"`
	ConversationID      string `json:"conversationId"`
	RequestID           string `json:"requestId"`
	InputLength         int    `json:"inputLength"`
	ActivityID          string `json:"activityId"`
	MentionContexts     []any  `json:"mentionContexts"`
	MentionContextCount int    `json:"mentionContextCount"`
	UserID              string `json:"userId"`
}

// ReportSchoolSeasonActivity 发一条小程序指纹的对话事件，点亮「校园日」任务。
//
// 为什么必须用小程序指纹而不是复用桌面版上报：实测（2026-09-18，真实账号）
// 桌面版形状的事件发出去 HTTP 200 但 growth 域进度**恒为 0**；
// 换成这套小程序指纹 + activityId 后 current 从 0 变 1、accept_status 变 completed、
// claim 成功到账 100 积分 + 5 能量。
//
// 域的选择：report 走 billingBase（codebuddy.cn），与 account_records 一致 ——
// 上游小程序的 /v2/report 也在这个域。
//
// 一个重要陷阱（本次踩过）：**回读时必须带身份头**（X-User-Id / X-Domain，
// 由 BillingHeaders 提供）。缺这两个头时查询接口照样返回 200，但返回的是
// 「不带身份」的口径 —— 进度永远显示 0，让人误以为事件没生效。
// 本函数只负责上报；调用方回读请走 GrowthTask 查询（那里已带身份头）。
func (c *Client) ReportSchoolSeasonActivity(a *auth.Auth, conversationID string) error {
	if conversationID == "" {
		conversationID = fmt.Sprintf("wb-run-%d", time.Now().UnixMilli())
	}
	now := time.Now().UnixMilli()
	ev := miniChatRequestEvent{
		EventCode:           "chat_request_send",
		Timestamp:           now,
		ReportDelay:         0,
		Source:              "mini_program",
		IDEName:             "wx_app_cloud",
		IDEType:             "WorkBuddy_MP",
		ExtName:             "workbuddy-mp",
		ExtVersion:          "SaaS",
		Mode:                "chat",
		ConversationID:      conversationID,
		RequestID:           conversationID,
		InputLength:         12,
		ActivityID:          SchoolSeasonActivityID,
		MentionContexts:     []any{},
		MentionContextCount: 0,
		UserID:              a.UID,
	}
	raw, err := json.Marshal([]miniChatRequestEvent{ev})
	if err != nil {
		return err
	}
	_, err = c.billingJSON(a, http.MethodPost, reportPath, json.RawMessage(raw))
	return err
}

// billingJSON 发 billing 域请求并解信封。
//
// 与 growthJSON 的区别仅在基址：billing 域走 billingBase（国服与 chat 不同域），
// growth 域走 chatBase。report 等 billing 端点共用本函数。
func (c *Client) billingJSON(a *auth.Auth, method, path string, body any) (json.RawMessage, error) {
	var rdr io.Reader
	if body != nil {
		raw, err := json.Marshal(body)
		if err != nil {
			return nil, err
		}
		rdr = bytes.NewReader(raw)
	}
	req, err := http.NewRequest(method, c.billingBase(a)+path, rdr)
	if err != nil {
		return nil, err
	}
	BillingHeaders(req, a)
	return c.doJSON(a, req)
}
