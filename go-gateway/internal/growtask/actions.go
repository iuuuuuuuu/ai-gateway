// actions.go 「一键完成」的动作表：任务码 → 具体动作实现。
//
// 分类依据是**依赖强度**，而不是代码量。它同时决定执行顺序与风险：
//
//	A 类 纯行为事件上报 —— 无外部依赖、不消耗对话额度，最稳，放在最前；
//	B 类 真实对话 —— 消耗 token、在上游留下真实会话记录（Model_chat_GLM5.2）；
//	C 类 专家市场链 —— 除真实对话外还要求专家 id 真实存在、
//	     且判据事件的 requestId 必须是服务端分配的真实 id，依赖最多；
//	D 类 不可自动 —— 需真实人工动作（Expert_Philanthropy 需真实捐款）。
//
// 执行顺序 = 表顺序：先解锁依赖项、先做稳的。单项失败不影响后续项，
// 因此顺序不会造成"前面失败导致后面全废"。
package growtask

import (
	"context"
	"errors"
	"fmt"
	"math/rand/v2"
	"time"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/upstream"
)

// action 一个可自动化的任务动作。
type action struct {
	// Code 目标任务码（与上游 task_code 精确对应）。
	Code string
	// Desc 展示用的动作说明。
	Desc string
	// NeedsChat 是否消耗真实对话（由 cate 推导，供界面提示资源消耗）。
	NeedsChat bool
	// run 执行动作。before 是动作前的任务状态（可能为 nil）。
	//
	// 返回值语义遵循「尽力而为」：能推进进度就返回说明 + nil；
	// 只有**确知无法推进**（如前置条件不满足、上游拒绝）才返回 error。
	// 上报部分成功、部分失败时返回说明 + nil 并把失败写进说明 ——
	// 因为进度可能是部分推进的，而 error 会让调用方跳过回读。
	run func(r *Runner, ctx context.Context, a *auth.Auth, before *upstream.GrowthTask) (string, error)
}

// 企鹅教师助手（Buddy_App_QQ 的判据应用）。同一组 buddyapp 事件同时满足
// Buddy_App（进入任一应用）与 Buddy_App_QQ，故两个任务共用一次上报。
const (
	qqTeacherBuddyID   = "cb_y5Dy46tPQGGWtueMxXbe"
	qqTeacherBuddyName = "企鹅教师助手"

	// hpThemeKey Hp_Appearance 的判据主题（和平精英激战金秋）。
	hpThemeKey = "theme-tkmw7j"

	// libraryDocURL 资料库介绍页（Library_read 判据事件的 pageURL）。
	libraryDocURL = "https://www.workbuddy.cn/space/d/o0KWYeynteVv06UnAZqIFm"

	// lighthouseExpertID 腾讯轻量云专家（Expert_lighthouse 判据专家）。
	lighthouseExpertID = "ex_2cvvUZQhDyeJ"

	// freeModel 用于真实对话的免费/低成本模型。
	// 用 fast-model 而非昂贵模型：这些动作的目的是产生「真实对话」这一事实，
	// 与模型能力无关，没必要为一次 1+1 消耗主力模型额度。
	freeModel = "fast-model"
)

// 国服「成长任务集」的三个任务码（Sequential_Tasks_1/_2/_3）。
//
// ★ 实测纠正（2026-09-30，真实国服账号）：它们**不是**单纯的「顺序解锁」，
// 而是两道不同的门槛叠加：
//
//	_1 完成前 → _2/_3 在两种列表口径里都不下发（连影子都没有）
//	_1 完成后 → _2 立刻出现，但带 locked=true + unlock_at=2026-10-01T00:00:00+08:00，
//	            progress 仍是 null —— 日期锁：**露面 ≠ 能动手**
//
// 即「完成前一个」只负责让下一个**露面**，能不能执行另由 unlock_at 决定。
// 把两者混为一谈会写出「列表里有了就去做」的代码，而那种代码在锁定任务上
// 会发出注定被静默丢弃的事件（见 runner.go runAction 的锁定守卫）。
//
// 已验证范围：_1 全程跑通（报名 → 上报 → 领奖 +100 积分 +5 能量）；
// _2 已验证「能出现在列表里」且「事件发得出去」，但**未验证能计分**
//（锁着时事件被丢弃）；_3 至今未在两种口径下出现过。
const (
	sequentialChatCode   = "Sequential_Tasks_1"
	sequentialExpertCode = "Sequential_Tasks_2"
	sequentialChat5Code  = "Sequential_Tasks_3"
)

// mpOnlyCodes 必须走**小程序口径**（X-Client-Platform: miniprogram）的任务码。
//
// 口径差别是实测出来的，不是猜的（2026-09-30，同一账号同一天）：
//
//	默认口径 accept Sequential_Tasks_1 → HTTP 200 + code=0，
//	                                     但逐条结果 {"status":"error","message":"task not found"}
//	加 mp 头 accept  Sequential_Tasks_1 → {"status":"accepted"}
//
// 也就是说「HTTP 200 + code=0」并不代表报名成功 —— 200 的壳里可以装着
// 「任务不存在」。这个集合同时决定报名与领奖走哪个口径（见 runner.go 的
// acceptOne / claim）。
//
// school_season 与它们同属「小程序限定任务」，参考实现也把它放在同一组；
// 但校园日活动已结束（上游不再下发），无法实测它是否同样强制 mp 头，
// 故这里**只收已验证的三个码**，不动 school_season 的既有报名路径。
var mpOnlyCodes = map[string]bool{
	sequentialChatCode:   true,
	sequentialExpertCode: true,
	sequentialChat5Code:  true,
}

// actions 动作表（顺序即执行顺序，见包注释的分类说明）。
var actions = []action{
	// ---- A 类：纯行为事件上报（零对话消耗） ----

	// chat_5 是累计型任务：按差额补报 chat_request_send。
	// 放在最前是因为它复用既有的活跃上报通道（billing 域），依赖最少。
	{Code: "chat_5", Desc: "补报对话活跃事件至达标（按差额）", run: runChat5},
	// first_buddy 依赖活跃上报解锁：先报一条 → 同意协议 → 领养。
	{Code: "first_buddy", Desc: "上报解锁 → 同意协议 → 领养第一只 Buddy", run: runFirstBuddy},
	{Code: "RichMeow_Chat", Desc: "上报桌面端完整对话事件链（含成功回执）", run: runRichMeow},
	// Buddy_App 与 Buddy_App_QQ 共用同一组事件（见 qqTeacherBuddyID 注释），
	// 幂等由任务状态跳过兜底：第一个跑完点亮后，第二个会在前置检查里跳过。
	{Code: "Buddy_App", Desc: "上报进入 Buddy 应用事件链", run: runBuddyApp},
	{Code: "Buddy_App_QQ", Desc: "上报进入企鹅教师助手事件链", run: runBuddyApp},
	{Code: "automation_1", Desc: "上报定时任务创建成功事件", run: runAutomationCreate},
	{Code: "Library_read", Desc: "以 Web 指纹上报资料库阅读事件", run: runLibraryRead},
	{Code: "template_5", Desc: "上报模板使用事件组 ×5", run: runTemplateUse},
	{Code: "playbook_prompt", Desc: "上报灵感案例「做同款」发送事件组", run: runPlaybookPrompt},
	{Code: "create_canvas", Desc: "上报设计创意画布创建事件组", run: runCreateCanvas},
	{Code: "Hp_Appearance", Desc: "设置主题并上报皮肤生效事件", run: runAppearance},
	// school_season（校园日）：growth 域任务，但判据是 **school 域 activityId 关联** ——
	// 必须发小程序指纹（mini_program / workbuddy-mp）+ activityId 才计分。
	// 挂在 C 类之前是因为它零对话消耗（纯事件上报）。
	{Code: "school_season", Desc: "上报校园日活动对话事件（小程序指纹 + activityId）", run: runSchoolSeason},

	// 国服「成长任务集」：与 school_season 同为小程序指纹的纯事件上报
	//（零对话消耗），差别是**不带 activityId** 且报名要走 mp 口径 —— 见 runMPMiniChat
	// 的注释。三条是「露面靠前一个完成、能动手靠 unlock_at」的双重门槛，
	// 见 sequentialChatCode 的说明；锁定项由 runAction 的守卫挡下。
	{Code: sequentialChatCode, Desc: "上报小程序对话事件（成长任务集，无 activityId）", run: runSequentialChat},
	{Code: sequentialExpertCode, Desc: "上报小程序专家使用事件（expert_actual_use）", run: runMiniExpert},
	// _3 的判据与 _1 同形状、只是 target=5，但**有反作弊节奏要求**（见 runMPMiniChat
	// 的 mpChatGap 说明）：秒级连发会被整体回滚，必须按真人节奏逐条上报。
	{Code: sequentialChat5Code, Desc: "按真人节奏补报 5 次小程序对话（反作弊：需间隔）", run: runSequentialChat5},

	// ---- B 类：真实对话（消耗 token） ----

	{Code: "Model_chat_GLM5.2", Desc: "glm-5.2 真实对话一次并对齐模型上报", NeedsChat: true, run: runModelChat},

	// ---- C 类：专家市场链（真实专家 id + 真实对话回执） ----

	{Code: "expert_5", Desc: "真实专家召唤 + 使用链 ×5", NeedsChat: true, run: runExpert5},
	{Code: "Expert_team_use_3", Desc: "真实专家团召唤 + 使用链 ×3", NeedsChat: true, run: runExpertTeam3},
	{Code: "Expert_lighthouse", Desc: "轻量云专家召唤 + 使用链（真实对话回执）", NeedsChat: true, run: runExpertLighthouse},
	{Code: "skill_1", Desc: "真实对话 + 技能加载（skill_info）事件", NeedsChat: true, run: runSkill1},

	// ---- D 类：时段敏感（窗口外跳过，不算失败） ----

	// black_cat 判据是夜猫时段内的**真实对话**（实测目标 3 次、只发上报不计分），
	// 故它也消耗对话额度 —— NeedsChat 必须置 true，界面才能如实提示。
	{Code: "black_cat", Desc: "夜猫时段内补足真实对话（23:00–08:00 北京时间）", NeedsChat: true, run: runBlackCat},
}

// actionFor 查任务对应的动作；无则返回 nil（不可自动化）。
func actionFor(code string) *action {
	for i := range actions {
		if actions[i].Code == code {
			return &actions[i]
		}
	}
	return nil
}

// SupportedCodes 返回全部可自动化的任务码（供宿主展示覆盖面）。
func SupportedCodes() []string {
	out := make([]string, 0, len(actions))
	for _, a := range actions {
		out = append(out, a.Code)
	}
	return out
}

// 唯一不可自动的任务：需真实捐款，服务端在领奖时校验捐赠回执。
// 刻意**不**尝试伪造 —— 伪造捐赠回执属于欺诈，且必然失败（回执由真实支付链路产生）。
const philanthropyeCode = "Expert_Philanthropy"

// workstationCode 工作台搭建师：需**桌面端换血对话**，不适合自动化。
//
// # 为什么不做（2026-09-20 对照参考脚本 workbuddyv3 后确认）
//
// 参考脚本的 `t_workstation` 实现方式是：
//
//	1. 把 WorkBuddy 桌面端的 `.info` 认证文件**换成目标账号**（改本机登录态）
//	2. `taskkill /F /IM WorkBuddy.exe` **杀掉用户的桌面端**
//	3. 重启它，等 8 秒
//	4. 通过 CDP 发一条带 `expert://WorkspaceBuilder` 资源引用的对话
//	5. 再杀一次、重启一次，恢复原登录态
//
// **这四步都在动用户正在用的桌面端**：改登录态、杀进程、重启两次。
// 网关跑在后台（可能正被别的客户端使用），做这些会：
//
//	· 打断用户正在进行的桌面端会话（数据可能未保存）
//	· 与「网关只做 HTTP 层自动化」的定位冲突 —— 它是服务，不该操作 GUI
//	· 失败时可能把用户桌面端留在"换了登录态但没恢复"的状态
//
// 故**如实标为需人工**，而不是硬做。收益（一个任务的积分）远小于风险。
const workstationCode = "workstation_expert"

// UnsupportedHint 返回不可自动任务的操作指引（供界面展示）。
func UnsupportedHint(code string) string {
	if code == philanthropyeCode {
		return "该任务需真实捐款：服务端在领奖时校验捐赠回执，无法通过接口绕过；请按任务说明在客户端内完成"
	}
	if code == workstationCode {
		return "该任务需在 WorkBuddy 桌面端内与「工作台搭建师」专家对话（网关不操作桌面端：那会打断你正在用的会话）；请在客户端内完成"
	}
	return "该任务需要客户端内的人工操作，无法自动完成；请按任务说明在官方客户端操作"
}

// ---------------------------------------------------------------------------
// A 类实现
// ---------------------------------------------------------------------------

// runChat5 按差额补报 chat_request_send 至 chat_5 达标。
func runChat5(r *Runner, ctx context.Context, a *auth.Auth, before *upstream.GrowthTask) (string, error) {
	if before == nil {
		return "", errors.New("任务不存在")
	}
	target := before.Target
	if target <= 0 {
		// 未报名时上游不下发 progress；RunAll 已先报名，这里兜底用上游的真实目标 5。
		target = 5
	}
	need := target - before.Current
	if need <= 0 {
		return "进度已达标，无需上报", nil
	}
	for i := int64(0); i < need; i++ {
		cid := fmt.Sprintf("wb2api-chat5-%d-%d", time.Now().UnixMilli(), i)
		if err := r.up.ReportChatActivity(a, cid, ""); err != nil {
			// 部分成功也要如实汇报：已发的那些条可能已经计分，
			// 返回 error 会让调用方跳过回读，连已达标的进度都发现不了。
			return fmt.Sprintf("已补报 %d/%d 条后中断: %v", i, need, err), nil
		}
		if i < need-1 {
			if err := sleepCtx(ctx, r.reportGap); err != nil {
				return fmt.Sprintf("已补报 %d/%d 条（本轮被取消）", i+1, need), nil
			}
		}
	}
	return fmt.Sprintf("已补报 %d 条对话活跃事件", need), nil
}

// runSchoolSeason 「校园日」活动任务（school_season）。
//
// 这是 growth 域下发的任务，但**判据在 school 域**：发一条带
// activityId=school_open_day_2026 的**小程序指纹**对话事件即点亮（target=1）。
//
// 为什么不能复用 runChat5 的 ReportChatActivity（桌面版指纹）：
// 实测（2026-09-18，真实账号）桌面版形状的事件发出去 HTTP 200，但
// growth 域进度**恒为 0**；换小程序指纹后 current 从 0 变 1、
// accept_status 变 completed、claim 到账 100 积分 + 5 能量。
// 两者差别就在 source/ideName/ideType/extName/mode 这几个指纹字段上。
//
// 与其它动作一样，**只发一次**（target=1，按差额补足），不循环刷。
func runSchoolSeason(r *Runner, ctx context.Context, a *auth.Auth, before *upstream.GrowthTask) (string, error) {
	if before == nil {
		return "", errors.New("任务不存在")
	}
	target := before.Target
	if target <= 0 {
		// 未报名时上游不下发 progress；RunOne 已先报名，这里兜底为 1。
		target = 1
	}
	need := target - before.Current
	if need <= 0 {
		return "进度已达标，无需上报", nil
	}

	sent := int64(0)
	for i := int64(0); i < need; i++ {
		cid := fmt.Sprintf("wb-run-%d-%d", time.Now().UnixMilli(), i)
		if err := r.up.ReportSchoolSeasonActivity(a, cid); err != nil {
			// 部分成功如实汇报：已发的那些条可能已经计分，
			// 返回 error 会让调用方跳过回读，连已达标的进度都发现不了。
			return fmt.Sprintf("已上报 %d/%d 条后中断: %v", sent, need, err), nil
		}
		sent++
		if i < need-1 {
			if err := sleepCtx(ctx, r.reportGap); err != nil {
				return fmt.Sprintf("已上报 %d/%d 条（本轮被取消）", sent, need), nil
			}
		}
	}

	// 服务端归账需要一点时间：实测上报后**立即回读仍可能是 0/1**，
	// 等约 2 秒才稳定变成 1/1。睡在这里是为了让调用方的回读能拿到新进度 ——
	// 否则会读出旧值，表现为「报了但没生效」，而实际上只是读早了。
	if err := sleepCtx(ctx, 2*time.Second); err != nil {
		return fmt.Sprintf("已上报 %d 条（等待归账时被取消）", sent), nil
	}
	return fmt.Sprintf("已上报 %d 条校园日活动事件", sent), nil
}

// ---------------------------------------------------------------------------
// 国服「成长任务集」：Sequential_Tasks_1 / _2 / _3
// ---------------------------------------------------------------------------

// mpChatJitter 在 mpChatGap 之上叠加的抖动上限。
//
// 为什么需要抖动：固定周期的上报本身就是一种机器指纹 —— 「每 45.000 秒一条、
// 连续 5 条」比「间隔 45~55 秒」更像脚本。参考实现用的是固定 0~10 秒随机；
// 这里改成**跟随 gap 缩放**（gap/5，45s → 9s，同量级），好处是测试把 gap
// 压到毫秒级时抖动自动一起消失，不需要额外开一个「关掉抖动」的开关
//（那种开关本身就会被误用成「测试里把反作弊节奏也关掉」）。
func mpChatJitter(gap time.Duration) time.Duration {
	span := gap / 5
	if span <= 0 {
		return 0
	}
	return time.Duration(rand.Int64N(int64(span)))
}

// runMPChatTask 是 Sequential_Tasks_1/_3 的公共实现：按差额补报小程序对话事件。
//
// # 与 school_season 的唯一差别：**不带 activityId**
//
// 两个任务的判据事件 eventCode 相同（chat_request_send）、指纹相同
//（mini_program / wx_app_cloud / WorkBuddy_MP / workbuddy-mp），服务端
// 靠**有没有 activityId** 区分它们：school_season 必带
// activityId=school_open_day_2026，成长任务集必须不带（见 report.go 里
// miniChatRequestEvent.ActivityID 的 omitempty 说明）。搞反的表现是
// HTTP 200 + 进度恒为 0 —— 最难排查的一类失败。
//
// # 反作弊：为什么每条上报前都要先睡
//
// 上游对「有效对话」有节奏校验（参考实现 2026-09-26 实测，与本次移植前的
// 真实账号观察一致）：数秒级连发的事件会**先被计入进度**（回读 5/5、
// accept_status 甚至短暂变 completed），随后被判定无效**整体回滚** ——
// 进度回落、claim 返回 400 task not completed。2 秒连发 4 条全灭；
// 45 秒间隔逐条上报全存活且能领到奖励。
//
// 所以这里**每条上报前都睡**（首条也睡）。首条也等看着多余（单条上报不存在
// 「连发」），但实测存在另一种失败：上一轮被回滚后立即重报同样无效 ——
// 宁可多等 45 秒，也不要发出一条注定被静默丢弃的事件（后者要赔上整个重试周期）。
//
// # 为什么要重新读一次任务
//
// accept 之后上游才下发真实 progress。RunAll 的批量报名路径里，动作拿到的
// before 是**报名前**的快照（target=0、progress=null），照它算差额会少报：
// 参考实现实测 Sequential_Tasks_6 报名后真实 target=10，只补 1 条就被误判
// 达标去领奖 → claim 400。故这里先 refetch 一次，拿不到才回落用 before。
func runMPChatTask(r *Runner, ctx context.Context, a *auth.Auth, before *upstream.GrowthTask, code string, withActivityID bool) (string, error) {
	if before == nil {
		return "", errors.New("任务不存在")
	}
	t := before
	if fresh := r.refetch(a, code); fresh != nil {
		t = fresh
	}
	if t.Claimed() {
		return "该任务奖励已领取", nil
	}
	target := t.Target
	if target <= 0 {
		// 未报名时上游不下发 progress，target 兜底 1（报名后会被上面的
		// refetch 纠正成真实值）。
		target = 1
	}
	if t.Current >= target {
		return "进度已达标，无需上报", nil
	}

	need := target - t.Current
	sent := int64(0)
	for i := int64(0); i < need; i++ {
		if err := sleepCtx(ctx, r.mpChatGap+mpChatJitter(r.mpChatGap)); err != nil {
			if sent == 0 {
				return "本轮在首次上报前被取消", nil
			}
			return fmt.Sprintf("已上报 %d/%d 条（本轮被取消）", sent, need), nil
		}
		cid := fmt.Sprintf("wb2api-mp-%d-%d", time.Now().UnixMilli(), i)
		var err error
		if withActivityID {
			err = r.up.ReportSchoolSeasonActivity(a, cid)
		} else {
			err = r.up.ReportMPChatActivity(a, cid)
		}
		if err != nil {
			// 部分成功如实汇报：已发的那些条可能已经计分，
			// 返回 error 会让调用方跳过回读，连已达标的进度都发现不了。
			return fmt.Sprintf("已上报 %d/%d 条后中断: %v", sent, need, err), nil
		}
		sent++
	}
	return fmt.Sprintf("已上报 %d 条小程序对话事件", sent), nil
}

// runSequentialChat 完成 Sequential_Tasks_1「在小程序内完成一次有效对话」。
//
// 判据 = 一条**不带 activityId** 的小程序 chat_request_send（target=1）。
// 参考实现实测该任务值 100 积分 + 5 能量。
func runSequentialChat(r *Runner, ctx context.Context, a *auth.Auth, before *upstream.GrowthTask) (string, error) {
	return runMPChatTask(r, ctx, a, before, sequentialChatCode, false)
}

// runSequentialChat5 完成 Sequential_Tasks_3「完成 5 次有效对话」。
//
// 判据与 _1 同形状、只是 target=5，服务端按上报条数累加 —— 但**有节奏要求**，
// 见 runMPChatTask 的反作弊说明。45 秒间隔补满 5/5 后领奖 +300 积分 +5 能量，
// 领后 accept_status=claimed 稳定不回滚。
//
// 代价：5 条 × (45s + 0~9s 抖动) ≈ 4~5 分钟，是全部动作里最慢的一项。
// 仍在宿主 growTaskTimeout（10 分钟）预算内，但同账号把它和 _1 排在一起时
// 剩余预算要留够。
//
// ⚠ 截至 2026-09-30，本任务**未在真实实例上验证过**：_3 连露面都没露面
//（_1 完成后复读列表，只多出 _2，_3 仍不在）。所以它的实现是按参考实现的
// 形状移植的，未经验证 —— 若上游对它的判据与 _1 有出入，这里的 45 秒节奏
// 与事件形状都可能是错的。首次真实执行时请留意进度是否真的推进。
func runSequentialChat5(r *Runner, ctx context.Context, a *auth.Auth, before *upstream.GrowthTask) (string, error) {
	return runMPChatTask(r, ctx, a, before, sequentialChat5Code, false)
}

// runMiniExpert 完成 Sequential_Tasks_2「在小程序内选中专家并完成有效对话」。
//
// 判据 = mp 指纹 expert_actual_use（**不带** activityId/conversationId、
// extVersion=2.2.8、type=send_message）—— 与 school 域的专家事件是两套口径，
// 别混用；见 report.go 的 mpExpertUseEvent。参考实现实测上报即 completed，
// 领奖 +200 积分 +5 能量。
//
// # 为什么先解析专家市场
//
// 判据事件的专家 id 必须是市场里**真实存在的 ex_ id**：空 id 或编造的 id
// 服务端照样返回 200，但不计分。所以拉不到市场列表就整个任务不动作 ——
// 宁可不做，也不要发一条注定不计分的事件（用户会以为已经完成）。
//
// ⚠ 与参考实现的一处**顺序差异**：它把市场检查放在 accept **之前**，
// 而本项目里 accept 由 Runner 统一负责（RunOne / acceptPending 在任何
// 动作之前完成），动作函数拿到的已经是报名后的状态。差异的后果仅是
// 可能多报名一次 —— 报名是幂等的（重复返回 already_accepted），
// 任务停在「已报名未上报」也不影响下一轮重试，故不为此改动 Runner 的
// 统一报名时序。
//
// ⚠ 截至 2026-09-30，本任务**只验证到「能出现、能发出事件」**，未验证能计分：
// _1 完成后 _2 确实出现在列表里（默认口径 total 20 → 21），但带
// locked=true + unlock_at=2026-10-01T00:00:00+08:00，此时上报事件被服务端
// **静默丢弃**（HTTP 200 但进度 before/after 都是 not_accepted）。
// 实现按参考实现的形状移植；锁定态由 runAction 的守卫挡下，解锁后才可能计分。
func runMiniExpert(r *Runner, ctx context.Context, a *auth.Auth, before *upstream.GrowthTask) (string, error) {
	if before == nil {
		return "", errors.New("任务不存在")
	}
	t := before
	if fresh := r.refetch(a, sequentialExpertCode); fresh != nil {
		t = fresh
	}
	if t.Claimed() {
		return "该任务奖励已领取", nil
	}
	target := t.Target
	if target <= 0 {
		target = 1
	}
	if t.Current >= target {
		return "进度已达标，无需上报", nil
	}

	experts, err := r.up.MarketExpertList(a, "")
	if err != nil {
		return fmt.Sprintf("专家市场不可用（%v），跳过以防半程态", err), nil
	}
	if len(experts) == 0 {
		return "专家市场未返回可用专家，跳过以防半程态", nil
	}
	e := experts[0]
	name := e.DisplayNameZH
	if name == "" {
		name = e.ProfessionZH
	}
	if err := r.up.ReportMPExpertUse(a, e.ExpertID, name, e.ExpertType); err != nil {
		return fmt.Sprintf("上报 expert_actual_use 失败: %v", err), nil
	}
	return fmt.Sprintf("已上报小程序专家使用事件（%s）", name), nil
}

// runFirstBuddy 领养第一只 Buddy：上报（解锁前置）→ 同意协议 → 领养。
//
// 顺序不可换：领养接口要求 first_buddy 任务的前置条件（当日有对话活跃）已满足，
// 未满足时返回 HTTP 400 "first_buddy task not completed yet"。
// 该错误当日重试无意义（门槛按自然日重置），因此识别出来就返回说明而非 error。
func runFirstBuddy(r *Runner, ctx context.Context, a *auth.Auth, before *upstream.GrowthTask) (string, error) {
	if err := r.up.ReportChatActivity(a, fmt.Sprintf("wb2api-adopt-%d", time.Now().UnixMilli()), ""); err != nil {
		return "", fmt.Errorf("前置上报失败: %w", err)
	}
	if err := sleepCtx(ctx, r.reportGap); err != nil {
		return "前置上报已完成，本轮被取消", nil
	}
	if err := r.up.BuddyAgreement(a); err != nil {
		return "", fmt.Errorf("同意协议失败: %w", err)
	}
	if err := r.up.BuddyFirst(a); err != nil {
		if upstream.IsBuddyTaskIncomplete(err) {
			// 门槛未过是**预期状态**（上游按当日活跃度判定），不是故障。
			return "前置已上报，但领养门槛未过（上游要求当日活跃），请稍后重试", nil
		}
		return "", fmt.Errorf("领养失败: %w", err)
	}
	return "已领养第一只 Buddy", nil
}

// runRichMeow 上报桌面端完整对话事件链（RichMeow_Chat）。
//
// 为什么不发真实对话：该任务的判据是「桌面端完成一次对话」，由**事件链**
// 判定，其中 chat_message_response 的 isSuccessful=true 是成功回执。
// 因此纯事件即可点亮，无需消耗真实对话额度。
func runRichMeow(r *Runner, ctx context.Context, a *auth.Auth, _ *upstream.GrowthTask) (string, error) {
	ms := time.Now().UnixMilli()
	conv := fmt.Sprintf("wb2api-rm-%d", ms)
	req := fmt.Sprintf("wb2api-rm-req-%d", ms)
	events := upstream.ChatSequence(conv, req, fmt.Sprintf("req-%d-user", ms), freeModel, freeModel)
	if err := r.up.ReportDesktopEvents(a, events...); err != nil {
		return "", err
	}
	return "已上报桌面端对话事件链（含成功回执）", nil
}

// runBuddyApp 上报进入 Buddy 应用事件链（同时覆盖 Buddy_App 与 Buddy_App_QQ）。
func runBuddyApp(r *Runner, ctx context.Context, a *auth.Auth, _ *upstream.GrowthTask) (string, error) {
	if err := r.up.ReportDesktopEvents(a, upstream.BuddyAppSequence(qqTeacherBuddyID, qqTeacherBuddyName)...); err != nil {
		return "", err
	}
	return "已上报 buddyapp 进入五连事件（同时覆盖 Buddy_App 与 Buddy_App_QQ）", nil
}

// runAutomationCreate 上报定时任务创建成功事件（automation_1）。
func runAutomationCreate(r *Runner, ctx context.Context, a *auth.Auth, _ *upstream.GrowthTask) (string, error) {
	if err := r.up.ReportDesktopEvents(a, upstream.AutomationCreateEvent("wb2api 自动化")); err != nil {
		return "", err
	}
	return "已上报定时任务创建成功事件", nil
}

// runLibraryRead 以 Web 指纹上报资料库阅读事件（Library_read）。
//
// 必须用 Web 指纹：该任务的判据是网页端的元素点击，
// 用桌面指纹上报同类事件不计分（实测）。
func runLibraryRead(r *Runner, ctx context.Context, a *auth.Auth, _ *upstream.GrowthTask) (string, error) {
	if err := r.up.ReportWebEvent(a, "web_element_click", libraryDocURL,
		"library_doc_intro_click", "WorkBuddy资料库介绍"); err != nil {
		return "", err
	}
	return "已上报资料库介绍阅读事件（Web 指纹）", nil
}

// runTemplateUse 上报模板使用事件组 ×5（template_5）。
//
// 每组都带一条完整对话链：判据事件 JOIN 到对话上，
// 只发 template_used 而没有对话链不会被计入。
func runTemplateUse(r *Runner, ctx context.Context, a *auth.Auth, _ *upstream.GrowthTask) (string, error) {
	templates := [][2]string{
		{"1", "深度研究"}, {"2", "周报生成"}, {"3", "竞品分析"},
		{"4", "活动策划"}, {"5", "代码评审"},
	}
	for i, tp := range templates {
		ms := time.Now().UnixMilli()
		conv := fmt.Sprintf("wb2api-tpl-%d-%d", ms, i)
		req := fmt.Sprintf("wb2api-tpl-req-%d-%d", ms, i)
		if err := r.up.ReportDesktopEvents(a, upstream.TemplateUseSequence(conv, req, tp[0], tp[1])...); err != nil {
			return fmt.Sprintf("已上报 %d/%d 组模板事件后中断: %v", i, len(templates), err), nil
		}
		if err := sleepCtx(ctx, 300*time.Millisecond); err != nil {
			return fmt.Sprintf("已上报 %d/%d 组模板事件（本轮被取消）", i+1, len(templates)), nil
		}
	}
	return "已上报模板使用事件组 ×5", nil
}

// runPlaybookPrompt 上报灵感案例「做同款」发送事件组（playbook_prompt）。
func runPlaybookPrompt(r *Runner, ctx context.Context, a *auth.Auth, _ *upstream.GrowthTask) (string, error) {
	ms := time.Now().UnixMilli()
	conv := fmt.Sprintf("wb2api-pb-%d", ms)
	req := fmt.Sprintf("wb2api-pb-req-%d", ms)
	events := upstream.PlaybookPromptSequence(conv, req, "pm-gtm-launch-plan", "新产品上市 GTM 发布计划一页纸")
	if err := r.up.ReportDesktopEvents(a, events...); err != nil {
		return "", err
	}
	return "已上报 playbook_cta_click + playbook_prompt_send", nil
}

// runCreateCanvas 上报设计创意画布创建事件组（create_canvas）。
func runCreateCanvas(r *Runner, ctx context.Context, a *auth.Auth, _ *upstream.GrowthTask) (string, error) {
	ms := time.Now().UnixMilli()
	conv := fmt.Sprintf("wb2api-canvas-%d", ms)
	req := fmt.Sprintf("wb2api-canvas-req-%d", ms)
	if err := r.up.ReportDesktopEvents(a, upstream.DesignCanvasSequence(conv, req)...); err != nil {
		return "", err
	}
	return "已上报 wbx_design_canvas_task_create/open", nil
}

// runAppearance 设置主题 + 上报皮肤生效事件（Hp_Appearance）。
//
// 两步都要：只调 appearance/set 不计分（判据是设置页关闭时的
// appearance_skin_apply 事件），只发事件则缺少服务端侧的主题设置记录。
func runAppearance(r *Runner, ctx context.Context, a *auth.Auth, _ *upstream.GrowthTask) (string, error) {
	if err := r.up.SetAppearanceTheme(a, hpThemeKey); err != nil {
		return "", fmt.Errorf("设置主题失败: %w", err)
	}
	// 给服务端留出处理主题切换的时间，再发"生效"事件 ——
	// 两个请求挨着发会出现「先报生效、后设主题」的时序倒置。
	if err := sleepCtx(ctx, 2*time.Second); err != nil {
		return "主题已设置，本轮被取消（未上报生效事件）", nil
	}
	if err := r.up.ReportDesktopEvents(a, upstream.AppearanceApplyEvent(hpThemeKey)); err != nil {
		return "", err
	}
	return "已设置主题并上报皮肤生效事件", nil
}

// ---------------------------------------------------------------------------
// B 类实现
// ---------------------------------------------------------------------------

// runModelChat 用指定模型真实对话一次并对齐模型上报（Model_chat_GLM5.2）。
//
// 两次上报缺一不可：真实对话产生「用过该模型」的服务端事实，
// 对齐模型的上报事件则是任务计分器读取的字段 —— 只做其中之一都不计分。
func runModelChat(r *Runner, ctx context.Context, a *auth.Auth, _ *upstream.GrowthTask) (string, error) {
	const modelID, modelName = "glm-5.2", "GLM-5.2"
	conv, _, err := r.up.ChatWithModel(a, modelID, "hi，请回复一句话", "")
	if err != nil {
		return "", fmt.Errorf("glm-5.2 对话失败: %w", err)
	}
	if err := sleepCtx(ctx, r.reportGap); err != nil {
		return "对话已完成，本轮被取消（未上报）", nil
	}
	if err := r.up.ReportChatActivityModel(a, conv, "", modelID, modelName); err != nil {
		return "对话已完成，但模型对齐上报失败: " + err.Error(), nil
	}
	return "已完成 glm-5.2 真实对话并对齐模型上报", nil
}

// ---------------------------------------------------------------------------
// C 类实现
// ---------------------------------------------------------------------------

// runExpert5 使用 5 个平台专家（expert_5）。
func runExpert5(r *Runner, ctx context.Context, a *auth.Auth, _ *upstream.GrowthTask) (string, error) {
	return runExpertBatch(r, ctx, a, "agent", 5, "craft")
}

// runExpertTeam3 使用 3 个专家团（Expert_team_use_3）。
func runExpertTeam3(r *Runner, ctx context.Context, a *auth.Auth, _ *upstream.GrowthTask) (string, error) {
	return runExpertBatch(r, ctx, a, "team", 3, "craft")
}

// runExpertBatch 专家「召唤 + 使用」链的公共实现。
//
// 单次链路三件事，缺一不可（实测口径）：
//  1. 召唤事件组（expert_summon_click / expert_summoned）；
//  2. 真实对话拿服务端 requestId —— 自造 id 不计数；
//  3. 使用事件（expert_actual_use）JOIN 到该 requestId + 一条对话链。
//
// 逐个继续而非遇错中断：一位专家失败不该让其余 4 位也做不成，
// 而任务的判据是「计数」而非「全部成功」。
func runExpertBatch(r *Runner, ctx context.Context, a *auth.Auth, expertType string, count int, mode string) (string, error) {
	experts, err := r.up.MarketExpertList(a, expertType)
	if err != nil {
		return "", fmt.Errorf("拉取专家市场列表失败: %w", err)
	}
	if len(experts) == 0 {
		return "", errors.New("专家市场列表为空")
	}
	ok := 0
	var firstErr error
	for i := range experts {
		if ok >= count {
			break
		}
		if ctx.Err() != nil {
			break
		}
		if i > 0 {
			if err := sleepCtx(ctx, r.chatGap); err != nil {
				break
			}
		}
		if err := r.expertOnce(a, experts[i], mode); err != nil {
			if firstErr == nil {
				firstErr = err
			}
			continue
		}
		ok++
	}
	if ok == 0 {
		// 一次都没成：这才是真失败（列出首个原因便于排查）。
		if firstErr != nil {
			return "", fmt.Errorf("专家使用链全部失败（%s）: %w", expertType, firstErr)
		}
		return "", fmt.Errorf("专家使用链全部失败（%s）", expertType)
	}
	msg := fmt.Sprintf("已对 %d/%d 位真实专家完成召唤+使用链（类型 %s）", ok, count, expertType)
	if ok < count {
		msg += "（未达目标数，可稍后重试）"
	}
	return msg, nil
}

// expertOnce 对单个专家走一次完整的召唤 + 真实对话 + 使用事件。
func (r *Runner) expertOnce(a *auth.Auth, e upstream.MarketExpert, mode string) error {
	if err := r.up.ReportDesktopEvents(a, upstream.ExpertSummonSequence(e)...); err != nil {
		return fmt.Errorf("召唤链 %s: %w", e.ExpertID, err)
	}
	conv, req, err := r.up.ChatWithModel(a, freeModel, "1+1等于几？直接回答。", e.ExpertID)
	if err != nil {
		return fmt.Errorf("专家对话 %s: %w", e.ExpertID, err)
	}
	events := upstream.ChatSequence(conv, req, "msg-"+reqTail(req), freeModel, freeModel)
	events = append(events, upstream.ExpertActualUseEvent(e, conv, req, mode))
	if err := r.up.ReportDesktopEvents(a, events...); err != nil {
		return fmt.Errorf("使用事件 %s: %w", e.ExpertID, err)
	}
	return nil
}

// runExpertLighthouse 轻量云专家召唤 + 使用链（Expert_lighthouse）。
//
// 与 expert_5 同构，两处差异（实测真实样本）：
//   - 专家固定为腾讯轻量云专家，但版本等信息以市场列表为准（服务端下发的才真实）；
//   - 使用事件的 mode 为 LOCAL（而非 craft）。
func runExpertLighthouse(r *Runner, ctx context.Context, a *auth.Auth, _ *upstream.GrowthTask) (string, error) {
	lh := upstream.MarketExpert{
		ExpertID: lighthouseExpertID, ExpertType: "agent",
		DisplayNameZH: "腾讯轻量云专家", ProfessionZH: "腾讯轻量云专家", Version: "1.0.2",
	}
	// 市场列表命中则用服务端的字段（version / categories 等以服务端为准）。
	if experts, err := r.up.MarketExpertList(a, "agent"); err == nil {
		for _, e := range experts {
			if e.ExpertID == lighthouseExpertID {
				lh = e
				break
			}
		}
	}
	if err := r.expertOnce(a, lh, "LOCAL"); err != nil {
		return "", fmt.Errorf("轻量云专家链: %w", err)
	}
	return "已上报轻量云专家召唤+使用链（真实对话回执）", nil
}

// skillIDFresh / skillNameFresh 技能尝鲜判据里引用的技能。
const (
	skillNameFresh = "润泽小馆·日报撰写"
	skillIDFresh   = "skill_2097350077599879168"
)

// runSkill1 真实对话 + skill_info 技能加载事件（skill_1）。
//
// 判据是 skill_info（桌面指纹）JOIN 真实会话。注意方向：此前的
// skill_request_send / skill_installed 等事件都不计分，唯一有效的是 skill_info。
func runSkill1(r *Runner, ctx context.Context, a *auth.Auth, _ *upstream.GrowthTask) (string, error) {
	conv, req, err := r.up.ChatWithModel(a, freeModel, "1+1等于几？直接回答。", "")
	if err != nil {
		return "", fmt.Errorf("真实对话失败: %w", err)
	}
	events := upstream.SkillSequence(conv, req, skillNameFresh, skillIDFresh, "1.0.0")
	if err := r.up.ReportDesktopEvents(a, events...); err != nil {
		return "", fmt.Errorf("skill_info 事件上报失败: %w", err)
	}
	return "已上报真实对话 + skill_info 技能加载事件", nil
}

// ---------------------------------------------------------------------------
// D 类实现
// ---------------------------------------------------------------------------

// runBlackCat 夜猫子任务（black_cat）：仅夜猫时段内计入。
//
// 实测现状（2026-09-16 夜，真实账号，值得如实记录）：
// 目标值为 **3**（不是 1），奖励为 **0 积分 0 能量**，而同一夜内用三种判据
// 口径各做一次真实对话后进度都停在 1/3 不动：
//
//	A. CLI 域 chat（既有 ChatStream）+ billing 域模型对齐上报
//	B. 桌面域完整对话事件链（自造 conversationId/requestId）
//	C. 桌面域真实对话 + 用服务端返回的真实 requestId 构造对话链
//
// 最可能的原因是**按夜计数**（每夜最多计 1 次，需跨 3 夜完成）——
// 目标 3 + 无奖励的组合与"连续 3 晚"的语义吻合。但这是推断：
// 单夜实测既无法证实也无法证伪，故这里按"尽力补足差额"实现，
// 由调用方的进度回读如实反映结果，不假装成功。
//
// 窗口外**跳过而不报错**：这是上游规则决定的正常状态，不是故障。
// 报 error 会让「一键完成」的结果里出现一条看起来像失败的记录，
// 而用户完全无能为力（只能等到夜里）。
//
// 每轮实践：先发一条 chat_request_send 上报（成本最低、若上游认这条最划算），
// 再按差额补真实对话。差额每次最多补 3 次，避免一次点击打出远超需求的对话量。
func runBlackCat(r *Runner, ctx context.Context, a *auth.Auth, before *upstream.GrowthTask) (string, error) {
	if !upstream.InNightWindow(time.Now()) {
		return "当前不在夜猫时段（23:00–08:00 北京时间），上游不计入本次上报；请在时段内重试", nil
	}
	target := int64(3) // 实测目标值；未报名拿不到 progress 时的兜底
	if before != nil && before.Target > 0 {
		target = before.Target
	}
	var current int64
	if before != nil {
		current = before.Current
	}
	need := target - current
	if need <= 0 {
		return "进度已达标，无需补足", nil
	}
	if need > 3 {
		need = 3
	}

	// 先发一条对话活跃上报：若上游认这条，成本最低。
	cid := fmt.Sprintf("wb2api-night-%d", time.Now().UnixMilli())
	if err := r.up.ReportChatActivity(a, cid, cid+"-r1"); err != nil {
		return "", err
	}
	if err := sleepCtx(ctx, r.reportGap); err != nil {
		return "已上报一次夜猫活跃事件（本轮被取消）", nil
	}

	// 再补真实对话：夜猫任务的语义与"对话"绑定，故按差额补足。
	const modelID, modelName = "glm-5.2", "GLM-5.2"
	done := int64(0)
	for i := int64(0); i < need; i++ {
		if ctx.Err() != nil {
			break
		}
		conv, _, err := r.up.ChatWithModel(a, modelID, "1+1等于几？直接回答。", "")
		if err != nil {
			if done == 0 {
				return fmt.Sprintf("已上报夜猫活跃事件，但真实对话失败: %v", err), nil
			}
			return fmt.Sprintf("已完成 %d/%d 次夜间对话后中断: %v", done, need, err), nil
		}
		// 对齐模型的上报：让这次对话在计分器眼里确实"用过 glm-5.2"。
		if err := r.up.ReportChatActivityModel(a, conv, "", modelID, modelName); err != nil {
			if done == 0 {
				return "对话已完成，但模型对齐上报失败: " + err.Error(), nil
			}
			return fmt.Sprintf("已完成 %d/%d 次夜间对话（对齐上报失败: %v）", done, need, err), nil
		}
		done++
		if i < need-1 {
			if err := sleepCtx(ctx, r.chatGap); err != nil {
				break
			}
		}
	}
	return fmt.Sprintf("已在夜猫时段上报并完成 %d/%d 次 glm-5.2 真实对话", done, need), nil
}

// reqTail 取 requestId 末 8 位（用于拼 messageId）。
func reqTail(req string) string {
	if len(req) <= 8 {
		return req
	}
	return req[len(req)-8:]
}
