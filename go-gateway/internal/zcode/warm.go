package zcode

// warm.go —— ZCode 账号「激活」（activation / warm）。
//
// # 这是什么，为什么需要
//
// 上游对「刚注册、或刚被我们导入、从未在官方客户端活动过」的账号会额外
// 风控：额度查得到、模型列得出，但领取活动套餐要么拿不到 offer、要么被判
// 异常活动。官方客户端在用户**首次启动**时做两件事，让账号在上游看来
// 是一个"活过的账号"：
//
//	① 上报两个激活事件：app_launch、app_daily_active。
//	   ⚠ **不带 Authorization** —— 这条路径的身份完全由 body 里的
//	   device_mid 表达，上游按 device_mid + 日期对 daily_active 去重。
//	② 把上游**自己列出来的**每一个可领套餐都领一遍，priority 高的先领。
//	   「上游列出来的」就是过滤器本身 —— 不做任何套餐名启发式判断。
//
// 本文件把这两步合成一次激活，供用户在账号页显式触发（或 CLI 子命令）。
//
// # 与参考实现的关系
//
// 规格来自 test-bin/recon/zcode-D3-vin/internal/upstream/warm.go，已逐字核对
// 过 16 个字段、两个 element、以及领取阶段的错误码处置。三处**刻意**的差异，
// 每一处都在下面的注释里写明了理由：
//
//	① 语言/时区用**我们自己** Identity 声明的那一对（默认 zh-CN /
//	   Asia/Shanghai），而不是参考实现硬编码的 en-US / America/New_York；
//	② 验证码**先铸后用**（与参考实现一致），但**铸不出来时不阻断** ——
//	   照发，让上游如实回 3007；
//	③ 事件与领取都走本 Client 的出口（我们没有"按账号分流出口"的概念，
//	   见 egress 的说明）。
//
// # 合规立场（不要放宽这三条）
//
//   - **只由用户显式触发**（账号页按钮 / CLI 子命令）。
//   - **绝不挂进 claim_scheduler 的自动轮询**：定时自动"激活 + 领取"会让
//     账号表现出非人类的活动模式，与 claim.go 文件头记录的是同一条取舍。
//
// ⚠ 本目录是**历史 Go 实现，已不参与构建**（见 .github/workflows/ci.yml）。
//   Rust 版另有「宿主启动后每账号一次」的幂等自动入口，落地在
//   crates/ai-gateway-core/src/modules/multi_product_credit_patrol.rs 的
//   `warm_zcode_accounts`（标记 `warmed_at`）。以那边为准，勿照抄本文件。
//   - 事件上报是**官方客户端真实会发的行为**，不是伪造凭证、不是绕过风控；
//     若上游仍判异常，那是上游的判据，我们不额外伪装、也不做规避。

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"sort"
	"strings"
)

// eventReportPath 激活事件上报端点（拼在 eventHost 之后）。
const eventReportPath = "/api/v1/event/report"

// activationElements 首次启动上报的两个事件。
//
// ⚠ 顺序有意义，且是**顺序发**（任一失败即停），不是并发：官方客户端是
// 单线程依次发的；并发会让上游看到同一 device_mid 的两个请求同时到达。
var activationElements = []string{"app_launch", "app_daily_active"}

// activationFallbackOSVersion 参考实现实测的 Windows 版本号。
//
// 事件 body 必须是官方那 16 个字段的**完整形状**（上游按 device_mid + 日期
// 去重，缺字段本身就是可区分特征），所以这里不像请求头那样"取不到就省略"，
// 而是给一个**真实存在过**的值。Identity 能取到真实版本时优先用真实的。
const activationFallbackOSVersion = "10.0.26200"

// activationScreenResolution 参考实现实测的分辨率。
//
// 网关是独立进程，没有读取屏幕的能力；沿用参考实现那个真实存在过的值，
// 而不是编一个新的。改它没有任何收益，只会让我们与参考实现产生分歧。
const activationScreenResolution = "1920x1080"

// activationEvent 官方 sendReport 的 16 字段 body。
//
// ⚠ 用结构体而不是 map 是刻意的：map 会漏字段（漏一个就成了可区分特征），
// 结构体让「16 个字段全在」成为编译期可见的事实，测试也能逐字段钉住。
//
// 注意两个字段的**类型不一样**，这是官方的真实形状，不要"顺手统一"：
//
//	event_extra_detail  对象  {}
//	marketing_params    字符串 "{}"
type activationEvent struct {
	EventID          string         `json:"event_id"`
	ClientTimezone   string         `json:"client_timezone"`
	ClientLanguage   string         `json:"client_language"`
	ElementName      string         `json:"element_name"`
	EventRegion      string         `json:"event_region"`
	EventType        string         `json:"event_type"`
	EventText        string         `json:"event_text"`
	EventExtraDetail map[string]any `json:"event_extra_detail"`
	UserID           string         `json:"user_id"`
	ScreenResolution string         `json:"screen_resolution"`
	AppVersion       string         `json:"app_version"`
	DeviceOSCategory string         `json:"device_os_category"`
	DeviceOSVersion  string         `json:"device_os_version"`
	DeviceMid        string         `json:"device_mid"`
	MacID            string         `json:"mac_id"`
	MarketingParams  string         `json:"marketing_params"`
}

// WarmPlan 单个套餐的领取结果。
type WarmPlan struct {
	PlanID         string `json:"planId"`
	Name           string `json:"name,omitempty"`
	Priority       int    `json:"priority,omitempty"`
	OK             bool   `json:"ok"`
	AlreadyClaimed bool   `json:"alreadyClaimed,omitempty"`
	Message        string `json:"message,omitempty"`
}

// WarmReport 一次激活的结果。
//
// 口径与参考实现一致：**空串 = 成功**。
//
//	Events  空 = 两个事件都成功发出去了
//	Claim   空 = 没有出问题的套餐
//	Skipped 空 = 领取阶段确实执行了
type WarmReport struct {
	UID string `json:"uid"`
	// Events 事件阶段的问题描述；空串 = 两个事件都成功。
	Events string `json:"events"`
	// Claim 领取阶段的问题（各套餐失败原因的合并）；空串 = 全部成功。
	Claim string `json:"claim"`
	// Skipped 领取阶段被整体跳过的原因；空串 = 执行了。
	Skipped string `json:"skipped,omitempty"`
	// Plans 上游实际给出的套餐与逐个结果（供界面展示）。
	Plans []WarmPlan `json:"plans,omitempty"`
	// Claimed 成功领取的套餐 ID（含"此前已领过"这种幂等命中）。
	Claimed []string `json:"claimed,omitempty"`
}

// buildActivationEvent 构造官方 sendReport 形状的事件 body。
//
// 语言/时区取**我们自己 Identity 声明的那一对**，而不是照抄参考实现硬编码的
// en-US / America/New_York。理由：参考实现自己在注释里写明了它遵循的规则 ——
// 「一个真实的区域对，且与 billing 头声明的是同一对」；它警告的是**同一个
// 账号不能出现两个区域对**。我们的对话路径（Identity.Headers）声明的是
// zh-CN / Asia/Shanghai，遥测若报 en-US 就正好犯了它警告的那件事。
func (c *Client) buildActivationEvent(element, userID, deviceMid string) activationEvent {
	id := c.Identity
	return activationEvent{
		// event_id 每条事件一个新 UUID（官方用的是 UUID v4）。
		EventID:          NewDeviceMid(),
		ClientTimezone:   firstNonEmpty(id.ClientTimezone, "Asia/Shanghai"),
		ClientLanguage:   firstNonEmpty(id.ClientLanguage, "zh-CN"),
		ElementName:      element,
		EventRegion:      "app",
		EventType:        "view",
		EventText:        "",
		EventExtraDetail: map[string]any{},
		UserID:           userID,
		ScreenResolution: activationScreenResolution,
		AppVersion:       id.AppVersion,
		// OS 分类优先用真实值（与 Identity.Headers 的 X-Os-Category 同源）。
		DeviceOSCategory: osCategory(id.Platform),
		DeviceOSVersion:  firstNonEmpty(id.OSVersion, activationFallbackOSVersion),
		DeviceMid:        deviceMid,
		MacID:            "",
		MarketingParams:  "{}",
	}
}

// postEvent 上报一个激活事件。
//
// ⚠ **只设 Content-Type，不设 Authorization**：官方客户端这条路径就是匿名的，
// 身份由 body 里的 device_mid 表达。多送一个认证头反而是"与真机不一致"的
// 可区分特征（与 Identity.ClaimHeaders 的取舍完全同源）。
func (c *Client) postEvent(ctx context.Context, cr *Cred, element, userID, deviceMid string) error {
	body, err := json.Marshal(c.buildActivationEvent(element, userID, deviceMid))
	if err != nil {
		return fmt.Errorf("构造 %s 事件失败: %w", element, err)
	}

	ctx, cancel := context.WithTimeout(ctx, c.timeout())
	defer cancel()

	req, err := http.NewRequestWithContext(ctx, http.MethodPost,
		c.eventHost(cr)+eventReportPath, bytes.NewReader(body))
	if err != nil {
		return err
	}
	req.Header.Set("Content-Type", "application/json")

	resp, err := c.http().Do(req)
	if err != nil {
		return fmt.Errorf("事件 %s 上报失败: %w", element, err)
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))

	if resp.StatusCode >= 400 {
		return fmt.Errorf("事件 %s HTTP %d %.200s",
			element, resp.StatusCode, strings.TrimSpace(string(raw)))
	}
	return nil
}

// warmDeviceMid 取该账号的 device_mid；缺失时现场生成并固化。
//
// device_mid 是这条路径的**唯一身份**（事件不带认证头），所以它必须稳定 ——
// 每次激活换一个，上游看到的就是"同一个账号被大量不同设备激活"，比不发还糟。
// 取到后固化写回凭证文件，此后与外部文件无关（与 cred.go 的 device_mid 注释
// 同一取舍）。
func (c *Client) warmDeviceMid(cr *Cred) string {
	if IsUUID(cr.DeviceMid) {
		return cr.DeviceMid
	}
	cr.DeviceMid = NewDeviceMid()
	if cr.FilePath != "" {
		persistDeviceMidOnce(cr)
	}
	return cr.DeviceMid
}

// WarmAccount 对一个账号执行一次激活：先报事件，再领上游给出的所有套餐。
//
// # 为什么事件不成功就不领
//
// 与参考实现一致：激活事件是"让账号看起来活过"的前提，套餐领取是它的收益。
// 事件没发出去就领，等于在一个上游看来仍是"从未活动过"的账号上做写操作 ——
// 那正是最容易被判异常的时序。宁可只报事件、把领取留给下次。
//
// 返回的 report 在**上游业务错误**时也不是 nil（错误写进 report 的字段），
// 这样界面能逐阶段展示到底哪一步出了问题；只有"连账号都没有"才返回 error。
func (c *Client) WarmAccount(ctx context.Context, cr *Cred) (*WarmReport, error) {
	if cr == nil {
		return nil, fmt.Errorf("账号为空")
	}
	rep := &WarmReport{UID: cr.UID}

	// 事件 body 里的 user_id：优先从 JWT 现解（与 AccountIDFromJWT 的口径
	// 一致：user_id 优先、其次 sub），JWT 解不出时退回加载时已解好的值。
	userID := firstNonEmpty(AccountIDFromJWT(cr.JWT), cr.AccountID)
	if userID == "" {
		// 参考实现同款处置：没有 user_id 就发不了事件（body 里那个字段是
		// 必需的身份），但**不报错** —— 如实说明跳过原因即可。
		rep.Events = "JWT 里没有 user_id/sub —— 无法上报激活事件"
		rep.Skipped = "激活事件未发出，未执行领取"
		return rep, nil
	}

	mid := c.warmDeviceMid(cr)

	for _, element := range activationElements {
		if err := c.postEvent(ctx, cr, element, userID, mid); err != nil {
			rep.Events = err.Error()
			break
		}
	}
	if rep.Events != "" {
		rep.Skipped = "激活事件未全部成功，未执行领取"
		return rep, nil
	}

	c.claimOfferedPlans(ctx, cr, rep)
	return rep, nil
}

// claimOfferedPlans 领取上游给出的每一个套餐，priority 高的先领。
//
// 「上游列出来的就是过滤器」—— 不做任何套餐名启发式判断。上游会在不同时间
// 给出不同的 offer 集合，用名字去猜哪些该领会随着活动变化而失效。
func (c *Client) claimOfferedPlans(ctx context.Context, cr *Cred, rep *WarmReport) {
	pv, err := c.FetchPlanPreview(ctx, cr)
	if err != nil {
		rep.Claim = fmt.Sprintf("查询可领套餐失败: %v", err)
		return
	}
	if pv == nil || len(pv.Offers) == 0 {
		// 没有可领套餐 = 正常状态（活动未上线 / 已领完），不是错误。
		rep.Skipped = "上游没有给出可领套餐"
		return
	}

	var problems []string
	for _, o := range sortOffersByPriority(pv.Offers) {
		if strings.TrimSpace(o.PlanID) == "" {
			// 没有 plan_id 就发不出去（ClaimPlan 也会本地拦下，
			// 但在这里先记一笔能让用户看到是**哪个** offer 有问题）。
			problems = append(problems, "上游给出的某个套餐没有 plan_id")
			continue
		}

		row := WarmPlan{PlanID: o.PlanID, Name: o.Name, Priority: priorityOf(o)}
		res, err := c.claimWithCaptcha(ctx, cr, o.PlanID)
		switch {
		case err != nil:
			row.Message = err.Error()
			problems = append(problems, fmt.Sprintf("%s: %v", o.PlanID, err))
		case res != nil && res.AlreadyClaimed:
			row.OK = true
			row.AlreadyClaimed = true
			row.Message = "此前已领取过（不是错误）"
			rep.Claimed = append(rep.Claimed, o.PlanID)
		default:
			row.OK = true
			row.Message = "已领取"
			rep.Claimed = append(rep.Claimed, o.PlanID)
		}
		rep.Plans = append(rep.Plans, row)
	}

	if len(problems) > 0 {
		rep.Claim = strings.Join(problems, "; ")
	}
}

// claimWithCaptcha 领取一个套餐，最多两次；第二次是吃到 3007 之后
// **重新铸一个** param 的重试（与参考实现的 `for attempt := 1; attempt <= 2`
// 逐字一致）。
//
// # 为什么重试必须换一个新 param
//
// `Cred.CaptchaParam` 是**一次性**的（见 cred.go 的注释）：同一个 param 用
// 第二次上游必回 3007。重发同一个毫无意义，只会白烧一次请求。
//
// # 为什么铸不出来不阻断
//
// 参考实现每次领取前都新铸一个 param，我们照做；但本项目的求解器是**可选
// 组件**（`zcode_captcha_enabled` 恒为 true，可目录可能为空、node 可能不存在、
// 也可能正被上游限流）。铸不出来就照发（不带 param），让上游如实回 3007，
// 我们把它报成 `captcha_required` —— 那比"因为没配求解器所以整个激活失败"
// 诚实，也让用户能分清"是求解器没配好"还是"上游拒绝了这次领取"。
func (c *Client) claimWithCaptcha(ctx context.Context, cr *Cred, planID string) (*ClaimResult, error) {
	var lastErr error
	var lastMintErr error

	for attempt := 1; attempt <= 2; attempt++ {
		// 用副本：param 是一次性的，不该留在调用方的 Cred 上（那会让
		// 后续别的调用误以为它还能用）。ClaimPlan 只读它需要的字段，
		// 浅拷贝足够。
		probe := *cr

		param, region, mintErr := c.mintCaptcha(ctx)
		if mintErr != nil {
			lastMintErr = mintErr
			probe.CaptchaParam = ""
			probe.CaptchaRegion = ""
		} else {
			probe.CaptchaParam = param
			probe.CaptchaRegion = region
		}

		res, err := c.ClaimPlan(ctx, &probe, planID)
		if err == nil {
			return res, nil
		}
		lastErr = err

		if !isCaptchaRequired(err) || attempt == 2 {
			break
		}
		// 第一轮吃了 3007 ⇒ 换一个新 param 再试一次。
	}

	if isCaptchaRequired(lastErr) {
		hint := ""
		if lastMintErr != nil {
			hint = fmt.Sprintf("；求解器未能铸出 param：%v", lastMintErr)
		}
		return nil, fmt.Errorf("验证码连续两次被拒%s（原错误：%v）", hint, lastErr)
	}
	return nil, lastErr
}

// mintCaptcha 铸一个一次性的验证码 param（连同它所属的 region）。
func (c *Client) mintCaptcha(ctx context.Context) (param, region string, err error) {
	// 测试注入点（见 client.go 的 SetCaptchaMinterForTest）：
	// 真求解会起 Node 进程并**真的打上游**，测试绝不能走那条路。
	if c.mintCaptchaFn != nil {
		param, region, err = c.mintCaptchaFn(ctx)
		if err != nil {
			return "", "", err
		}
		if strings.TrimSpace(param) == "" {
			return "", "", fmt.Errorf("求解器返回了空 param")
		}
		return param, region, nil
	}

	param, err = c.solveCaptcha(ctx)
	if err != nil {
		return "", "", err
	}
	if strings.TrimSpace(param) == "" {
		return "", "", fmt.Errorf("求解器返回了空 param")
	}
	return param, c.captchaRegion(), nil
}

// isCaptchaRequired 判断一个错误是不是"上游要验证码"（业务码 3007）。
//
// 两个判据都看：Kind 是分类结果，Code 是上游原始业务码。任一中即算 ——
// 上游的码有时是数字、有时是字符串（见 codeOf 的注释），分类与原始码
// 只要有一处命中就不该漏判。
func isCaptchaRequired(err error) bool {
	if err == nil {
		return false
	}
	var ze *Error
	if errors.As(err, &ze) {
		return ze.Kind == ErrCaptchaRequired || ze.Code == CodeCaptchaRequired
	}
	return false
}

// priorityOf 读 offer 的 priority（缺失 = 0）。
//
// priority 在 PlanOffer 里没有专门字段，但原始对象被完整保留在 Raw 上，
// 所以这里直接读 —— 不去改 claim.go 的解析结构，避免为了排序动到领取
// 主路径（那是已经实测过的代码）。
func priorityOf(o PlanOffer) int {
	if o.Raw == nil {
		return 0
	}
	return int(toInt64(firstAny(o.Raw["priority"], o.Raw["Priority"])))
}

// sortOffersByPriority 按 priority **降序**稳定排序（参考实现同款）。
//
// 用稳定排序：priority 相同的 offer 保持上游给出的相对顺序，
// 那通常也是上游的意图顺序。
func sortOffersByPriority(offers []PlanOffer) []PlanOffer {
	out := make([]PlanOffer, len(offers))
	copy(out, offers)
	sort.SliceStable(out, func(i, j int) bool {
		return priorityOf(out[i]) > priorityOf(out[j])
	})
	return out
}
