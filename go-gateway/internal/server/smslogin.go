package server

// smslogin.go —— 手机号 + 短信验证码登录的 HTTP 入口（2026-09-22 新增）。
//
// # 两条路由
//
//	POST /login/sms/send    {"phone":"...", "region":"cn|intl"}
//	     → {"ok":true,"expiresIn":300}
//	POST /login/sms/verify  {"phone":"...", "smsCode":"...", "region":"cn|intl"}
//	     → {"ok":true,"account":{...}}
//
// # 为什么要分成两步而不是一次调用
//
// 验证码是**用户从手机上看来的**，天然有两次人机交互。
// 合成一个接口就必须把"等用户输入"做成服务端会话状态 ——
// 而网关是无状态转发的，那会引入一个不必要的内存表。
//
// # ⚠ 落库为什么走宿主而不是这里
//
// 登录成功后要**把账号写进宿主的账号库**。而网关进程只持有
// 「凭证目录的只读视图」（`auth_dir`），账号库（含备注、禁用状态、
// 区域标记）是**宿主的**数据结构（见 pool 包注释：授权状态由宿主管理）。
//
// 网关直接写账号库会绕过宿主的所有校验与缓存，表现为"加了账号但界面看不到"。
// 故这里**只负责换 token 并落一份凭证文件**，账号库登记由宿主在
// 拿到结果后自己完成（与 qoder/zcode 的登录同款分工）。

import (
	"encoding/json"
	"io"
	"net/http"
	"strings"
)

// smsBaseFor 把区域名解析成登录用的 base URL。
//
// ⚠ 与 `upstream.chatBase` 分开：那个需要一个 `*auth.Auth` 才能判区域，
// 而登录时**还没有账号对象**（那正是本流程的产物）。
func smsBaseFor(region string) string {
	switch strings.ToLower(strings.TrimSpace(region)) {
	case "intl", "ai", "international":
		return "https://www.workbuddy.ai"
	default:
		return "https://www.workbuddy.cn"
	}
}

// smsSend 发送登录验证码。
func (h *Handler) smsSend(w http.ResponseWriter, r *http.Request) {
	if h.cfg.Upstream == nil {
		writeOpenAIError(w, http.StatusServiceUnavailable, "upstream_unavailable",
			"upstream client not configured on this gateway instance")
		return
	}

	body, _ := io.ReadAll(io.LimitReader(r.Body, 1<<16))
	var req struct {
		Phone  string `json:"phone"`
		Region string `json:"region"`
	}
	if err := json.Unmarshal(body, &req); err != nil {
		writeOpenAIError(w, http.StatusBadRequest, "invalid_request", "invalid JSON body")
		return
	}
	if strings.TrimSpace(req.Phone) == "" {
		writeOpenAIError(w, http.StatusBadRequest, "invalid_request", "手机号不能为空")
		return
	}

	res, err := h.cfg.Upstream.SendSMS(smsBaseFor(req.Region), req.Phone)
	if err != nil {
		// ⚠ 用 400 而不是 502：这里的失败**绝大多数是用户输入问题**
		//（号码错 / 发得太频繁 / 验证码过期），把它报成"上游异常"
		// 会让用户去查网络，而正确动作是"检查号码、等一分钟再试"。
		writeOpenAIError(w, http.StatusBadRequest, "sms_send_failed", err.Error())
		return
	}

	writeJSON(w, http.StatusOK, map[string]any{
		"ok":        true,
		"expiresIn": res.ExpiresIn,
	})
}

// smsVerify 用验证码换 token，并把凭证交给宿主登记。
func (h *Handler) smsVerify(w http.ResponseWriter, r *http.Request) {
	if h.cfg.Upstream == nil {
		writeOpenAIError(w, http.StatusServiceUnavailable, "upstream_unavailable",
			"upstream client not configured on this gateway instance")
		return
	}
	// 登记回调由宿主注入（见 Config.SMSLogin）。
	// 未注入时**明确报错**而不是"换了 token 但没人存" ——
	// 后者会让用户以为登录成功了，重启后发现账号不见了。
	if h.cfg.SMSLogin == nil {
		writeOpenAIError(w, http.StatusServiceUnavailable, "sms_login_unavailable",
			"this gateway instance cannot register accounts (SMSLogin not configured)")
		return
	}

	body, _ := io.ReadAll(io.LimitReader(r.Body, 1<<16))
	var req struct {
		Phone   string `json:"phone"`
		SMSCode string `json:"smsCode"`
		Region  string `json:"region"`
	}
	if err := json.Unmarshal(body, &req); err != nil {
		writeOpenAIError(w, http.StatusBadRequest, "invalid_request", "invalid JSON body")
		return
	}
	if strings.TrimSpace(req.Phone) == "" || strings.TrimSpace(req.SMSCode) == "" {
		writeOpenAIError(w, http.StatusBadRequest, "invalid_request", "手机号与验证码都不能为空")
		return
	}

	region := normalizeSMSRegion(req.Region)
	res, err := h.cfg.Upstream.LoginWithSMS(smsBaseFor(region), req.Phone, req.SMSCode)
	if err != nil {
		writeOpenAIError(w, http.StatusBadRequest, "sms_login_failed", err.Error())
		return
	}

	// 交给宿主登记（落账号库 + 写凭证文件都在宿主侧）。
	acct, err := h.cfg.SMSLogin(SMSLoginInput{
		Phone:        strings.TrimSpace(req.Phone),
		Region:       region,
		AccessToken:  res.AccessToken,
		RefreshToken: res.RefreshToken,
	})
	if err != nil {
		writeOpenAIError(w, http.StatusInternalServerError, "sms_register_failed", err.Error())
		return
	}

	writeJSON(w, http.StatusOK, map[string]any{
		"ok":      true,
		"account": acct,
	})
}

// normalizeSMSRegion 归一区域名；空 = 国服。
//
// ⚠ 空默认成**国服**而不是报错：绝大多数用户是国服，
// 而界面上默认选中也是国服。要求显式传会让不关心区域的调用方白失败一次。
func normalizeSMSRegion(region string) string {
	switch strings.ToLower(strings.TrimSpace(region)) {
	case "intl", "ai", "international":
		return "intl"
	default:
		return "cn"
	}
}

// SMSLoginInput 宿主登记账号所需的全部输入。
type SMSLoginInput struct {
	Phone        string
	Region       string
	AccessToken  string
	RefreshToken string
}

// SMSLoginFunc 宿主提供的登记回调：落账号库 + 写凭证文件，返回账号摘要。
//
// 返回值形状由宿主决定（界面直接展示），网关不解释它。
type SMSLoginFunc func(SMSLoginInput) (any, error)
