package growtask

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"strings"
	"sync"
	"testing"
	"time"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/upstream"
)

// ---------------------------------------------------------------------------
// 假上游
//
// 用假上游而非 mock 接口：本包的编排逻辑是否正确的**唯一判据**是
// 「它按什么顺序、发了多少次、什么样的 HTTP 请求」，因此断言必须落在
// 真实 HTTP 层（路径、方法、请求头、请求体），而不是内部方法调用次数。
// 用真 *upstream.Client + 假 Transport，被测代码路径与生产完全一致。
// ---------------------------------------------------------------------------

// fakeUpstream 一个可编程的假上游。
type fakeUpstream struct {
	mu sync.Mutex

	// tasks 当前任务状态；按 task_code 索引。
	tasks map[string]*fakeTask
	// order 任务在列表里的顺序（决定返回顺序）。
	order []string

	// acceptCalls 收到的报名请求（每次调用一组 task_codes）。
	acceptCalls [][]string
	// acceptPlatforms 每次报名请求带的 X-Client-Platform（无该头则为空串）。
	//
	// 为什么必须记录这个头：小程序限定任务的报名缺该头时，上游照样返回
	// HTTP 200 + code=0，只是逐条结果是 {"status":"error","message":"task not
	// found"} —— 报名根本没发生。也就是说**从状态码和业务码都看不出区别**，
	// 只有请求头能证明走对了口径。假上游若不复刻这一层，就测不出
	// 「Sequential_Tasks_1 报名走错口径」这个真实故障（本次移植前它就发生在
	// 真实账号上：报名后 progress 恒为 null）。
	acceptPlatforms []string
	// claimCalls 收到的领奖请求（任务码序列）。
	claimCalls []string
	// reports 收到的上报（路径 → 次数）。
	reports map[string]int
	// reportBodies 记录 /v2/report 的请求体，供断言事件载荷。
	reportBodies [][]map[string]any
	// claimResponses 覆盖领奖响应（默认成功 +100/+5）。
	//
	// 带上 HTTP 状态码是必需的：上游用 **HTTP 400** 表达「进度未达标」，
	// 而 IsGrowthTaskNotCompleted 正是按 4xx 判定。若假上游一律回 200，
	// 就测不出「未达标」与「真故障」的区分 —— 而那正是调用方决定
	// 「该重试还是该放弃」的依据。
	claimResponses map[string]fakeResp
	// acceptFail 让报名请求失败。
	acceptFail bool
	// listFail 让列表请求失败。
	listFail bool
	// marketExpertFail 让专家市场列表失败。
	//
	// 需要它是因为 Sequential_Tasks_2 有一条**前置守卫**：判据事件的专家 id
	// 必须是市场里真实存在的 ex_ id，所以拉不到市场就整个任务不动作。
	// 没有这个开关就测不出「宁可不做，也不发一条注定不计分的事件」。
	marketExpertFail bool

	// acceptMaterializesProgress 报名后是否让 progress 出现（默认 true）。
	//
	// 这不是可有可无的开关，而是复刻真实行为：未报名时上游把 progress
	// 下发为 null，只有报名后才出现 {current,target}。如果本包的编排漏了
	// 「先报名」，回读就会永远拿不到进度。
	acceptMaterializesProgress bool

	// scoreDelayListCalls 行为事件引起的进度变化延后到第 N 次列表查询才可见。
	//
	// 为什么需要它：真实上游的计分是**异步**的（对话完成后立即回读仍是 0/1，
	// 数秒后才变 1/1）。不模拟这个延迟，就测不出「轮询等异步计分」这条
	// 关键路径 —— 而没有它，奖励会漏发且用户看不出来。
	// 0 = 立即生效（默认）。
	scoreDelayListCalls int
	// pendingProgress 待落定的进度：task_code → 目标 current。
	pendingProgress map[string]int64

	// failBillingReport 让 billing 域的 /v2/report 失败（chat 域不受影响）。
	//
	// 两个域的上报路径都是 /v2/report，只能靠 host 区分 —— 这正好用来
	// 独立地打掉「活跃上报」类动作（走 billing）而保留「桌面行为链」（走 chat）。
	failBillingReport bool

	// listCalls 列表被请求的次数（用于断言轮询行为）。
	listCalls int
}

type fakeTask struct {
	code         string
	title        string
	acceptStatus string
	current      int64
	target       int64
	hasProgress  bool
	claimed      bool
	// credit / energy 奖励值。默认 100/5，可用 addWithReward 覆盖。
	//
	// 需要可覆盖是因为真实上游**存在无奖励任务**（black_cat 实测 0 积分 0 能量），
	// 而"无奖励 + 进度不推进"正是最需要被诚实报告的组合（见 runAction 的守卫）。
	credit int64
	energy int64
	// locked / unlockAt 复刻日期锁任务（2026-09-30 实测 Sequential_Tasks_2：
	// locked=true + unlock_at=2026-10-01T00:00:00+08:00，progress=null）。
	//
	// 需要它是因为锁定任务**能被报名、能被发事件、服务端返回 HTTP 200**，
	// 只是静默丢弃不计分 —— 光看状态码根本发现不了，必须有回归用例钉住。
	locked   bool
	unlockAt string
}

// fakeResp 一条可编程的上游响应（状态码 + 响应体）。
type fakeResp struct {
	status int
	body   string
}

func newFakeUpstream() *fakeUpstream {
	return &fakeUpstream{
		tasks:                      map[string]*fakeTask{},
		reports:                    map[string]int{},
		claimResponses:             map[string]fakeResp{},
		acceptMaterializesProgress: true,
		pendingProgress:            map[string]int64{},
	}
}

// add 登记一个任务（默认奖励 100 积分 / 5 能量，与多数成长任务一致）。
func (f *fakeUpstream) add(code string, current, target int64, status string) *fakeUpstream {
	return f.addTask(code, current, target, status, 100, 5)
}

// addNoReward 登记一个**无奖励**任务（复刻 black_cat 的真实形态）。
func (f *fakeUpstream) addNoReward(code string, current, target int64, status string) *fakeUpstream {
	return f.addTask(code, current, target, status, 0, 0)
}

func (f *fakeUpstream) addTask(code string, current, target int64, status string, credit, energy int64) *fakeUpstream {
	f.tasks[code] = &fakeTask{
		code: code, title: "任务-" + code, acceptStatus: status,
		current: current, target: target, credit: credit, energy: energy,
		// 与真实上游一致：未报名时不下发 progress。
		hasProgress: status != upstream.TaskAcceptNotAccepted,
	}
	f.order = append(f.order, code)
	return f
}

// addLocked 登记一个**日期锁**任务（复刻 2026-09-30 实测的 Sequential_Tasks_2 形态）。
//
// 真实形态是 accept_status=not_accepted、progress=null（未报名且锁着），
// 但 locked 与 accept 是**正交**的两个维度：锁着的任务照样能被报名成功，
// 报名后仍是 locked，只是发事件不计分。这里保留 status 参数以便两种都测。
func (f *fakeUpstream) addLocked(code string, current, target int64, status, unlockAt string) *fakeUpstream {
	f.addTask(code, current, target, status, 100, 5)
	ft := f.tasks[code]
	ft.locked = true
	ft.unlockAt = unlockAt
	return f
}

// setProgress 直接改进度（模拟服务端异步计分）。
func (f *fakeUpstream) setProgress(code string, current int64) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if t := f.tasks[code]; t != nil {
		t.current = current
		t.hasProgress = true
	}
}

func (f *fakeUpstream) client() *upstream.Client {
	return &upstream.Client{
		HTTP:          &http.Client{Transport: rtFunc(f.roundTrip)},
		ChatHTTP:      &http.Client{Transport: rtFunc(f.roundTrip)},
		ChatBaseCN:    "https://chat.example",
		BillingBaseCN: "https://billing.example",
		WebBaseCN:     "https://web.example",
		WebBaseIntl:   "https://web-intl.example",
	}
}

type rtFunc func(*http.Request) (*http.Response, error)

func (fn rtFunc) RoundTrip(r *http.Request) (*http.Response, error) { return fn(r) }

func jsonResp(status int, body string) *http.Response {
	return &http.Response{
		StatusCode: status,
		Header:     http.Header{"Content-Type": []string{"application/json"}},
		Body:       io.NopCloser(strings.NewReader(body)),
	}
}

func (f *fakeUpstream) roundTrip(r *http.Request) (*http.Response, error) {
	f.mu.Lock()
	defer f.mu.Unlock()

	path := r.URL.Path
	switch {
	case r.Method == http.MethodGet && path == "/v2/activity/growth/tasks":
		f.listCalls++
		if f.listFail {
			return jsonResp(500, `{"code":500,"msg":"boom"}`), nil
		}
		// 异步计分落定：到点时把待落定的进度写进去。
		if f.scoreDelayListCalls > 0 && f.listCalls >= f.scoreDelayListCalls {
			for code, current := range f.pendingProgress {
				if t := f.tasks[code]; t != nil {
					t.current = current
					t.hasProgress = true
				}
			}
			f.pendingProgress = map[string]int64{}
		}
		return jsonResp(200, f.tasksJSON()), nil

	case r.Method == http.MethodPost && path == "/v2/activity/growth/tasks/accept":
		if f.acceptFail {
			return jsonResp(500, `{"code":500,"msg":"accept failed"}`), nil
		}
		var body struct {
			TaskCodes []string `json:"task_codes"`
		}
		raw, _ := io.ReadAll(r.Body)
		_ = json.Unmarshal(raw, &body)
		f.acceptCalls = append(f.acceptCalls, body.TaskCodes)
		f.acceptPlatforms = append(f.acceptPlatforms, r.Header.Get("X-Client-Platform"))
		type res struct {
			Code   string `json:"task_code"`
			Status string `json:"status"`
		}
		results := make([]res, 0, len(body.TaskCodes))
		for _, code := range body.TaskCodes {
			t := f.tasks[code]
			if t == nil {
				continue
			}
			// 复刻真实上游：小程序限定任务缺 mp 头时不登记，只回逐条 error。
			// 注意 HTTP 状态码与业务码都是「成功」，区别只在 results 里。
			if mpOnlyCodes[code] && r.Header.Get("X-Client-Platform") != "miniprogram" {
				results = append(results, res{code, "error"})
				continue
			}
			if t.acceptStatus == upstream.TaskAcceptAccepted || t.acceptStatus == upstream.TaskAcceptClaimed {
				results = append(results, res{code, "already_accepted"})
				continue
			}
			t.acceptStatus = upstream.TaskAcceptAccepted
			// 复刻真实行为：报名后才下发 progress。
			if f.acceptMaterializesProgress && t.target > 0 {
				t.hasProgress = true
			}
			results = append(results, res{code, upstream.TaskAcceptAccepted})
		}
		out, _ := json.Marshal(map[string]any{
			"code": 0, "msg": "OK", "data": map[string]any{"results": results},
		})
		return jsonResp(200, string(out)), nil

	case r.Method == http.MethodPost && strings.HasPrefix(path, "/activity/growth/tasks/"):
		code := strings.TrimSuffix(strings.TrimPrefix(path, "/activity/growth/tasks/"), "/claim")
		f.claimCalls = append(f.claimCalls, code)
		if custom, ok := f.claimResponses[code]; ok {
			return jsonResp(custom.status, custom.body), nil
		}
		t := f.tasks[code]
		if t == nil {
			return jsonResp(400, `{"code":400,"msg":"task not found"}`), nil
		}
		if t.claimed {
			return jsonResp(200, `{"code":0,"msg":"OK","data":{"already_claimed":true,"credit":0,"energy":0}}`), nil
		}
		// 与真实上游一致：未达标时拒绝领奖。
		if !(t.hasProgress && t.target > 0 && t.current >= t.target) {
			return jsonResp(400, `{"code":400,"msg":"task not completed"}`), nil
		}
		t.claimed = true
		t.acceptStatus = upstream.TaskAcceptClaimed
		// 奖励取自任务本身而不是写死 100/5：Sequential_Tasks_3 的真实奖励是
		// 300 积分，写死会让「到账多少」这类断言失去意义。
		out, _ := json.Marshal(map[string]any{
			"code": 0, "msg": "OK",
			"data": map[string]any{"already_claimed": false, "credit": t.credit, "energy": t.energy},
		})
		return jsonResp(200, string(out)), nil

	case r.Method == http.MethodPost && path == "/v2/report":
		// 两个域共用该路径，靠 host 区分哪个通道。
		isBilling := r.URL.Host == "billing.example"
		if isBilling && f.failBillingReport {
			return jsonResp(500, `{"code":500,"msg":"billing report down"}`), nil
		}
		raw, _ := io.ReadAll(r.Body)
		var arr []map[string]any
		if err := json.Unmarshal(raw, &arr); err == nil {
			f.reportBodies = append(f.reportBodies, arr)
			// 让 chat_request_send / 对话链的上报真的推进进度，
			// 以便端到端验证「上报 → 计分 → 领奖」闭环。
			for _, ev := range arr {
				f.applyEvent(ev)
			}
		}
		f.reports[path]++
		return jsonResp(200, `{"code":0,"msg":"OK","data":{}}`), nil

	case r.Method == http.MethodPost && path == "/v2/user-asset/appearance/set":
		f.reports[path]++
		return jsonResp(200, `{"code":0,"msg":"OK","data":{}}`), nil

	case r.Method == http.MethodGet && path == "/activity/growth/streak":
		return jsonResp(200, `{"code":0,"data":{"streak":{"days":1}}}`), nil

	case r.Method == http.MethodPost && path == "/activity/growth/buddy/agreement",
		r.Method == http.MethodPost && path == "/activity/growth/buddy/first":
		f.reports[path]++
		return jsonResp(200, `{"code":0,"msg":"OK","data":{}}`), nil

	case r.Method == http.MethodPost && path == "/portal/operation-platform/market/expert/list":
		if f.marketExpertFail {
			return jsonResp(500, `{"code":500,"msg":"market unavailable"}`), nil
		}
		return jsonResp(200, `{"code":0,"msg":"OK","data":{"experts":[
			{"expert_id":"ex_1","expert_type":"agent","display_name_zh":"专家一","profession_zh":"职业","version":"1.0.0"},
			{"expert_id":"ex_2","expert_type":"agent","display_name_zh":"专家二","profession_zh":"职业","version":"1.0.0"}
		]}}`), nil

	case r.Method == http.MethodPost && path == "/v2/chat/completions":
		// 服务端形状的 requestId（专家/skill 判据要求真实 id）。
		return &http.Response{
			StatusCode: 200,
			Header:     http.Header{"Content-Type": []string{"text/event-stream"}},
			Body: io.NopCloser(strings.NewReader(
				"data: {\"id\":\"cmb-0123456789abcdef0123456789abcdef\",\"choices\":[{\"delta\":{\"content\":\"2\"}}]}\n\n" +
					"data: [DONE]\n\n")),
		}, nil
	}
	return jsonResp(404, `{"code":404,"msg":"no route: `+path+`"}`), nil
}

// applyEvent 让行为事件真的推进进度（复刻服务端计分语义）。
//
// scoreDelayListCalls > 0 时进度不立即生效，而是变成"待落定"，
// 由后续的列表查询在到点时提交 —— 这就是真实上游的异步计分行为。
//
// 入参是**整个事件**而非只 eventCode：校园日（school_season）与桌面版
// chat_request_send 的 eventCode 相同，靠指纹字段（source/extName）与
// activityId 区分 —— 只传 code 就复刻不出「桌面事件不计分」这个实测语义。
func (f *fakeUpstream) applyEvent(ev map[string]any) {
	code, _ := ev["eventCode"].(string)
	set := func(taskCode string, current int64) {
		if f.scoreDelayListCalls > 0 {
			f.pendingProgress[taskCode] = current
			return
		}
		if t := f.tasks[taskCode]; t != nil {
			t.current = current
			t.hasProgress = true
		}
	}
	// chat_request_send 计入 chat_5（累计型）。
	if code == "chat_request_send" {
		if t := f.tasks["chat_5"]; t != nil {
			next := t.current + 1
			if next > t.target {
				next = t.target
			}
			set("chat_5", next)
		}
	}
	// school_season：只有**小程序指纹 + activityId** 才计分。
	//
	// 这是对真实上游的复刻（2026-09-18 实测）：桌面版形状的事件发到同一端点
	// 返回 200 但进度恒为 0；必须 source=mini_program、extName=workbuddy-mp
	// 且带 activityId=school_open_day_2026 才点亮。
	// 若把这里放宽成「只看 eventCode」，就会让「误用桌面指纹」的实现也通过测试 ——
	// 那正是本次要锁住的回归。
	// 成长任务集（Sequential_Tasks_1/_3）：同一条 chat_request_send，
	// **不带** activityId 才计分。
	//
	// 与上面 school_season 分支合起来构成一个互斥判据：同一个 eventCode、
	// 同一套 mp 指纹，服务端只靠「有没有 activityId」区分两个任务。
	// 若这里放宽成「有 mp 指纹就都给分」，那么把 _1 的事件写成带 activityId
	// （或反过来把 school_season 写成不带）的实现也能通过测试 ——
	// 而这两种写法在真实上游都会让进度恒为 0（HTTP 200，最难排查的一类失败）。
	if t := f.tasks["school_season"]; t != nil {
		src, _ := ev["source"].(string)
		ext, _ := ev["extName"].(string)
		act, _ := ev["activityId"].(string)
		if src == "mini_program" && ext == "workbuddy-mp" && act == upstream.SchoolSeasonActivityID {
			set("school_season", t.target)
		}
	}
	if code == "chat_request_send" {
		src, _ := ev["source"].(string)
		ext, _ := ev["extName"].(string)
		_, hasActivity := ev["activityId"]
		if src == "mini_program" && ext == "workbuddy-mp" && !hasActivity {
			for _, tc := range []string{"Sequential_Tasks_1", "Sequential_Tasks_3"} {
				if t := f.tasks[tc]; t != nil {
					next := t.current + 1
					if next > t.target {
						next = t.target
					}
					set(tc, next)
				}
			}
		}
	}
	// Sequential_Tasks_2：mp 指纹 expert_actual_use，专家 id 必须是真实市场 id。
	if code == "expert_actual_use" {
		src, _ := ev["source"].(string)
		ext, _ := ev["extName"].(string)
		id, _ := ev["id"].(string)
		if t := f.tasks["Sequential_Tasks_2"]; t != nil {
			if src == "mini_program" && ext == "workbuddy-mp" && strings.HasPrefix(id, "ex_") {
				set("Sequential_Tasks_2", t.target)
			}
		}
	}
	// 对话成功回执点亮 RichMeow_Chat。
	if code == "chat_message_response" {
		if t := f.tasks["RichMeow_Chat"]; t != nil {
			set("RichMeow_Chat", t.target)
		}
	}
}

// tasksJSON 生成与真实上游同形状的列表响应。
func (f *fakeUpstream) tasksJSON() string {
	type wire struct {
		TaskCode     string `json:"task_code"`
		Title        string `json:"title"`
		AcceptStatus string `json:"accept_status"`
		RewardCredit int64  `json:"reward_credit"`
		RewardEnergy int64  `json:"reward_energy"`
		Progress     any    `json:"progress"`
		// locked / unlock_at 与真实上游一致：未锁定时 omitempty 不出现。
		Locked   bool   `json:"locked,omitempty"`
		UnlockAt string `json:"unlock_at,omitempty"`
	}
	items := make([]wire, 0, len(f.order))
	for _, code := range f.order {
		t := f.tasks[code]
		var prog any
		if t.hasProgress {
			prog = map[string]int64{"current": t.current, "target": t.target}
		}
		items = append(items, wire{
			TaskCode: code, Title: t.title, AcceptStatus: t.acceptStatus,
			RewardCredit: t.credit, RewardEnergy: t.energy, Progress: prog,
			Locked: t.locked, UnlockAt: t.unlockAt,
		})
	}
	out, _ := json.Marshal(map[string]any{
		"code": 0, "msg": "OK", "data": map[string]any{"tasks": items},
	})
	return string(out)
}

// testRunner 构造一个用假上游、且把等待时间压到最小的 Runner。
//
// 压缩间隔是**必需**的：默认轮询预算是 4×3 秒、上报间隔 1 秒，
// 一轮完整测试要跑十几秒。缩短的是时序参数而非逻辑，覆盖的执行路径完全相同。
//
// ⚠ MPChatGap 尤其不能漏：默认是 45 秒（反作弊阈值，见 defaultMPChatGap），
// 一个 Sequential_Tasks_3 用例要发 5 条事件，漏了就真睡 3 分多钟。
func testRunner(f *fakeUpstream) *Runner {
	return New(Options{
		Upstream:     f.client(),
		PollAttempts: 3,
		PollGap:      time.Millisecond,
		ReportGap:    time.Millisecond,
		ChatGap:      time.Millisecond,
		MPChatGap:    time.Millisecond,
	})
}

func testAuth() *auth.Auth {
	return &auth.Auth{AccessToken: "at", UID: "u1"}
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

// TestRunAllAcceptsBeforeActing 未报名的任务必须先报名：
// 未报名时上游把 progress 下发为 null，不先报名则回读永远拿不到进度，
// 「上报 200 ≠ 计分」的自检就失去了依据。
func TestRunAllAcceptsBeforeActing(t *testing.T) {
	f := newFakeUpstream().
		add("chat_5", 0, 5, upstream.TaskAcceptNotAccepted).
		add("RichMeow_Chat", 0, 1, upstream.TaskAcceptNotAccepted)
	r := testRunner(f)

	r.RunAll(context.Background(), testAuth())

	if len(f.acceptCalls) == 0 {
		t.Fatal("应先批量报名再执行动作")
	}
	got := f.acceptCalls[0]
	want := map[string]bool{"chat_5": true, "RichMeow_Chat": true}
	if len(got) != 2 {
		t.Fatalf("报名任务数=%d want 2 (%v)", len(got), got)
	}
	for _, code := range got {
		if !want[code] {
			t.Errorf("不该报名 %s", code)
		}
	}
	// 报名必须发生在任何行为上报之前。
	if f.reports["/v2/report"] == 0 {
		t.Error("报名后应执行行为上报")
	}
}

// TestRunAllSkipsAcceptForAcceptedTasks 已报名/已领奖的任务不重复报名。
func TestRunAllSkipsAcceptForAcceptedTasks(t *testing.T) {
	f := newFakeUpstream().add("chat_5", 0, 5, upstream.TaskAcceptAccepted)
	r := testRunner(f)

	r.RunAll(context.Background(), testAuth())

	if len(f.acceptCalls) != 0 {
		t.Errorf("无需报名时不该调用报名接口, got %v", f.acceptCalls)
	}
}

// TestRunAllClaimHappyPath 端到端闭环：报名 → 上报 → 轮询到达标 → 自动领奖。
func TestRunAllClaimHappyPath(t *testing.T) {
	f := newFakeUpstream().add("chat_5", 0, 2, upstream.TaskAcceptNotAccepted)
	r := testRunner(f)

	items := r.RunAll(context.Background(), testAuth())

	var item *ItemResult
	for i := range items {
		if items[i].Code == "chat_5" {
			item = &items[i]
		}
	}
	if item == nil {
		t.Fatal("结果里缺少 chat_5")
	}
	if item.Status != StatusDone {
		t.Errorf("status=%s message=%s", item.Status, item.Message)
	}
	if !item.Claimed {
		t.Errorf("应自动领奖, item=%+v", item)
	}
	if item.Credit != 100 || item.Energy != 5 {
		t.Errorf("领奖数=%d/%d want 100/5", item.Credit, item.Energy)
	}
	if item.ProgressAfter != "2/2" {
		t.Errorf("progress_after=%q want 2/2", item.ProgressAfter)
	}
	if len(f.claimCalls) != 1 || f.claimCalls[0] != "chat_5" {
		t.Errorf("领奖调用=%v", f.claimCalls)
	}
}

// TestRunAllPollsUntilScoreLands 异步计分：进度延后一拍才可见时，
// 轮询必须等到它落定并领奖。
//
// 为什么这条最关键：真实上游计分是异步的（对话完成后立即回读仍是 0/1，
// 数秒后才变 1/1）。一次性回读会把「还在计分」误判成「未达标」，
// 奖励就此漏掉，而用户完全看不出来 —— 这是本功能最隐蔽的失效方式。
func TestRunAllPollsUntilScoreLands(t *testing.T) {
	f := newFakeUpstream().add("RichMeow_Chat", 0, 1, upstream.TaskAcceptAccepted)
	// 计分延后到第 3 次列表查询才可见（首次读取 before 算第 1 次，
	// 动作完成后的第一次回读是第 2 次，轮询的第 2 拍才拿到达标）。
	f.scoreDelayListCalls = 3
	r := testRunner(f)

	item := r.RunOne(context.Background(), testAuth(), "RichMeow_Chat")

	if item.Status != StatusDone {
		t.Fatalf("status=%s msg=%s", item.Status, item.Message)
	}
	if !item.Claimed {
		t.Errorf("轮询等到计分落定后应自动领奖, item=%+v", item)
	}
	if item.ProgressAfter != "1/1" {
		t.Errorf("progress_after=%q want 1/1", item.ProgressAfter)
	}
	if len(f.claimCalls) != 1 {
		t.Errorf("应恰好领奖一次, got %v", f.claimCalls)
	}
	// 必须真的轮询过：至少 3 次列表查询（before + 回读 + 再轮询）。
	if f.listCalls < 3 {
		t.Errorf("列表查询次数=%d，未体现轮询等待", f.listCalls)
	}
}

// TestRunAllNoClaimWhenScoreNeverLands 计分始终不落定时**不能**领奖，
// 且不得谎报进度。
//
// 这条守的是诚实性：宁可如实报告「仍未达标」，也不能为了让结果好看
// 而假装达标（那会打出一个必然被上游拒绝的领奖请求，并误导用户）。
func TestRunAllNoClaimWhenScoreNeverLands(t *testing.T) {
	f := newFakeUpstream().add("RichMeow_Chat", 0, 1, upstream.TaskAcceptAccepted)
	// 计分落在第 99 次查询 —— 远超轮询预算（3 次）。
	f.scoreDelayListCalls = 99
	r := testRunner(f)

	item := r.RunOne(context.Background(), testAuth(), "RichMeow_Chat")

	if item.Claimed {
		t.Error("进度未达标时不该领奖")
	}
	if len(f.claimCalls) != 0 {
		t.Errorf("进度未达标时不该调用领奖接口, got %v", f.claimCalls)
	}
	if item.ProgressAfter == "1/1" {
		t.Error("进度未达标时不得谎报为达标")
	}
	// 动作本身确实执行了，所以状态仍是 done（区别于「执行失败」）。
	if item.Status != StatusDone {
		t.Errorf("status=%s want done（动作已执行，只是尚未计分）", item.Status)
	}
}

// TestRunAllContinuesAfterItemFailure 单项失败不影响后续项（尽力而为）。
//
// 构造：让 billing 域上报失败（chat_5 的动作会失败），
// 而 chat 域的桌面行为链正常 —— 断言后面的项照样被执行。
func TestRunAllContinuesAfterItemFailure(t *testing.T) {
	f := newFakeUpstream().
		add("chat_5", 0, 5, upstream.TaskAcceptAccepted).
		add("RichMeow_Chat", 0, 1, upstream.TaskAcceptAccepted)
	f.failBillingReport = true
	r := testRunner(f)

	items := r.RunAll(context.Background(), testAuth())

	byCode := map[string]ItemResult{}
	for _, it := range items {
		byCode[it.Code] = it
	}
	// chat_5 的动作失败（billing 上报挂了）→ 该项应为 error，
	// 或至少如实说明上报失败。
	chat5, ok := byCode["chat_5"]
	if !ok {
		t.Fatal("结果里缺少 chat_5")
	}
	failedOrExplained := chat5.Status == StatusError ||
		strings.Contains(chat5.Message, "失败") ||
		strings.Contains(chat5.Message, "中断")
	if !failedOrExplained {
		t.Errorf("billing 上报失败应被如实反映, got status=%s msg=%q", chat5.Status, chat5.Message)
	}
	// 关键断言：后续项仍然被执行（未被前一项的失败带崩）。
	rm, ok := byCode["RichMeow_Chat"]
	if !ok {
		t.Fatal("结果里缺少 RichMeow_Chat")
	}
	if rm.Status != StatusDone || !rm.Claimed {
		t.Errorf("前一项失败后，后续项仍应正常执行并领奖, got %+v", rm)
	}
}

// TestRunAllSkipsClaimedTask 已领奖的任务既不动手也不领奖（幂等，不浪费配额）。
func TestRunAllSkipsClaimedTask(t *testing.T) {
	f := newFakeUpstream().add("chat_5", 5, 5, upstream.TaskAcceptClaimed)
	r := testRunner(f)

	items := r.RunAll(context.Background(), testAuth())

	for _, it := range items {
		if it.Code == "chat_5" {
			if it.Status != StatusSkipped {
				t.Errorf("已领奖任务应跳过, got %+v", it)
			}
			if !strings.Contains(it.Message, "已领取") {
				t.Errorf("跳过原因应说明已领取: %q", it.Message)
			}
		}
	}
	if len(f.claimCalls) != 0 {
		t.Errorf("已领奖任务不该再调领奖接口: %v", f.claimCalls)
	}
	// 全部跳过时不写账号记录（无变化，写了只会刷屏）。
	if f.reports["/v2/report"] != 0 {
		t.Error("已领奖任务不该有任何行为上报")
	}
}

// TestRunAllClaimsAlreadyCompletedTask 进度已达标但没领过奖：
// 直接补领奖，不重复跑动作（省上游配额）。
func TestRunAllClaimsAlreadyCompletedTask(t *testing.T) {
	f := newFakeUpstream().add("chat_5", 5, 5, upstream.TaskAcceptAccepted)
	r := testRunner(f)

	items := r.RunAll(context.Background(), testAuth())

	for _, it := range items {
		if it.Code != "chat_5" {
			continue
		}
		if !it.Claimed {
			t.Errorf("达标未领奖的任务应直接补领, got %+v", it)
		}
		if !strings.Contains(it.Message, "直接领奖") {
			t.Errorf("message=%q", it.Message)
		}
	}
	// 已达标就不该再发行为上报（那是浪费）。
	if f.reports["/v2/report"] != 0 {
		t.Errorf("已达标任务不该再有行为上报, got %d 次", f.reports["/v2/report"])
	}
}

// TestRunAllListFailureReportsError 列表拉不到时整体报错，
// 不编造「全部跳过」—— 那会让用户以为任务都已处理过。
func TestRunAllListFailureReportsError(t *testing.T) {
	f := newFakeUpstream()
	f.listFail = true
	r := testRunner(f)

	items := r.RunAll(context.Background(), testAuth())

	if len(items) != 1 || items[0].Status != StatusError {
		t.Fatalf("items=%+v", items)
	}
	if !strings.Contains(items[0].Message, "拉取任务列表失败") {
		t.Errorf("message=%q", items[0].Message)
	}
}

// TestRunAllContinuesAfterAcceptFailure 报名失败记录在案，其余动作照常执行。
func TestRunAllContinuesAfterAcceptFailure(t *testing.T) {
	f := newFakeUpstream().add("chat_5", 0, 5, upstream.TaskAcceptNotAccepted)
	f.acceptFail = true
	r := testRunner(f)

	items := r.RunAll(context.Background(), testAuth())

	var acceptItem, chatItem *ItemResult
	for i := range items {
		switch items[i].Code {
		case "(批量报名)":
			acceptItem = &items[i]
		case "chat_5":
			chatItem = &items[i]
		}
	}
	if acceptItem == nil || acceptItem.Status != StatusError {
		t.Errorf("报名失败应如实汇报, got %+v", acceptItem)
	}
	if acceptItem != nil && !strings.Contains(acceptItem.Message, "不阻塞") {
		t.Errorf("应说明报名失败不阻塞后续: %q", acceptItem.Message)
	}
	if chatItem == nil {
		t.Fatal("报名失败后仍应尝试执行动作")
	}
}

// TestRunAllIgnoresUnknownTasks 该账号没有的任务报「没有此任务」而非失败。
func TestRunAllIgnoresUnknownTasks(t *testing.T) {
	f := newFakeUpstream().add("chat_5", 0, 5, upstream.TaskAcceptAccepted)
	r := testRunner(f)

	items := r.RunAll(context.Background(), testAuth())

	found := false
	for _, it := range items {
		if it.Code == "expert_5" {
			found = true
			if it.Status != StatusSkipped || !strings.Contains(it.Message, "没有此任务") {
				t.Errorf("不存在的任务应跳过: %+v", it)
			}
		}
	}
	if !found {
		t.Error("结果应包含动作表里的每个任务（含该账号没有的）")
	}
}

// TestRunOneUnsupportedTask 不可自动的任务返回明确指引，而不是假装执行。
func TestRunOneUnsupportedTask(t *testing.T) {
	f := newFakeUpstream()
	r := testRunner(f)

	item := r.RunOne(context.Background(), testAuth(), "Expert_Philanthropy")
	if item.Status != StatusUnsupported {
		t.Errorf("status=%s want unsupported", item.Status)
	}
	if !strings.Contains(item.Message, "无法自动完成") {
		t.Errorf("message=%q", item.Message)
	}
}

// TestRunOneAcceptsFirst 单任务入口也必须先报名（与整轮同一口径）。
func TestRunOneAcceptsFirst(t *testing.T) {
	f := newFakeUpstream().add("RichMeow_Chat", 0, 1, upstream.TaskAcceptNotAccepted)
	r := testRunner(f)

	item := r.RunOne(context.Background(), testAuth(), "RichMeow_Chat")
	if item.Status != StatusDone {
		t.Fatalf("status=%s msg=%s", item.Status, item.Message)
	}
	if len(f.acceptCalls) != 1 || f.acceptCalls[0][0] != "RichMeow_Chat" {
		t.Errorf("应只为该任务报名, got %v", f.acceptCalls)
	}
	if !item.Claimed {
		t.Errorf("应自动领奖, got %+v", item)
	}
}

// TestRunOneMissingTask 该账号没有此任务 → 跳过而非失败。
func TestRunOneMissingTask(t *testing.T) {
	f := newFakeUpstream().add("chat_5", 0, 5, upstream.TaskAcceptAccepted)
	r := testRunner(f)

	item := r.RunOne(context.Background(), testAuth(), "expert_5")
	if item.Status != StatusSkipped {
		t.Errorf("status=%s want skipped", item.Status)
	}
}

// TestAccountLock 账号级互斥：同账号并发跑会互相干扰进度回读，
// 必须能被挡住。
func TestAccountLock(t *testing.T) {
	r := testRunner(newFakeUpstream())
	if !r.Lock("u1") {
		t.Fatal("首次认领应成功")
	}
	if r.Lock("u1") {
		t.Error("同账号重复认领应失败")
	}
	// 不同账号互不影响。
	if !r.Lock("u2") {
		t.Error("不同账号应可同时认领")
	}
	r.Unlock("u1")
	if !r.Lock("u1") {
		t.Error("归还后应可重新认领")
	}
}

// TestSupportedCodesMatchActionTable 动作表与需求里的任务清单一致。
//
// 这个断言是**刻意的双向守卫**：既防漏实现（需求点名的没做），
// 也防多实现（表里冒出没人要求、也没被测过的任务）——
// 后者比漏实现更危险：多出来的动作会在「一键完成」里被执行，
// 而它可能伪造了不该伪造的行为。
//
// 因此**新增动作时必须同时在这里加上 code**，并补一个覆盖它的用例。
func TestSupportedCodesMatchActionTable(t *testing.T) {
	codes := SupportedCodes()
	if len(codes) != len(actions) {
		t.Fatalf("codes=%d actions=%d", len(codes), len(actions))
	}
	// 需求点名的任务必须都在表里。
	want := []string{
		"first_buddy", "create_canvas", "chat_5", "Model_chat_GLM5.2", "RichMeow_Chat",
		"Buddy_App", "Buddy_App_QQ", "automation_1", "Library_read", "template_5",
		"playbook_prompt", "expert_5", "Expert_team_use_3", "Hp_Appearance",
		"Expert_lighthouse", "skill_1", "black_cat",
		// 校园日（限时活动，growth 域下发但判据在 school 域：
		// 需小程序指纹 + activityId 才计分，见 runSchoolSeason 的注释）。
		// 活动 2026-09-24 截止；它过期后上游不再下发该 code，
		// RunOne 会走「该账号没有此任务」分支，动作表留着无害。
		"school_season",
		// 国服成长任务集（_2/_3 在 _1 完成前上游不下发；_1 完成后 _2
		// 露面但带日期锁 unlock_at —— 两道门槛，见 sequentialChatCode 注释）。
		// 判据都在 mp 指纹，但与 school_season 的关键区别是**不带**
		// activityId —— 见 runMPChatTask 的注释。
		"Sequential_Tasks_1", "Sequential_Tasks_2", "Sequential_Tasks_3",
	}
	have := map[string]bool{}
	for _, c := range codes {
		have[c] = true
	}
	for _, c := range want {
		if !have[c] {
			t.Errorf("动作表缺少任务 %s", c)
		}
	}
	if len(have) != len(want) {
		t.Errorf("动作表多余或缺少任务: have=%d want=%d（新增动作请同步更新本清单）", len(have), len(want))
	}
	// 不可自动的任务**不能**出现在动作表里。
	if actionFor("Expert_Philanthropy") != nil {
		t.Error("Expert_Philanthropy 需真实捐款，不应出现在动作表里")
	}
}

// TestBlackCatMessageIsHonest black_cat 的文案必须诚实。
//
// 实测背景：该任务目标为 3、奖励为 0，而同一夜内用三种判据口径各做一次
// 真实对话后进度都停在 1/3 不动（疑为按夜计数、需跨 3 夜）。
// 因此文案**不能**说"已完成" —— 那会让用户以为任务做完了。
// 要么说明为什么没做（不在窗口），要么如实报告只是"上报/对话"了。
func TestBlackCatMessageIsHonest(t *testing.T) {
	f := newFakeUpstream().addNoReward("black_cat", 0, 3, upstream.TaskAcceptAccepted)
	r := testRunner(f)

	item := r.RunOne(context.Background(), testAuth(), "black_cat")
	if item.Status != StatusDone {
		t.Fatalf("status=%s msg=%s", item.Status, item.Message)
	}
	if strings.Contains(item.Message, "已完成任务") || strings.Contains(item.Message, "任务完成") {
		t.Errorf("文案不该声称任务已完成: %q", item.Message)
	}
	// 非窗口时说明原因；窗口内则如实描述"上报/对话"这一动作本身。
	ok := strings.Contains(item.Message, "不在夜猫时段") ||
		strings.Contains(item.Message, "夜猫时段") ||
		strings.Contains(item.Message, "已达标")
	if !ok {
		t.Errorf("文案未说明时段或动作: %q", item.Message)
	}
}

// TestNoRewardStalledTaskIsReportedHonestly 无奖励且进度未推进时，
// 必须如实说明「进度未推进」，不能把"上报成功"写成"已完成"。
//
// 这条守的是本功能的核心诚实性：上报 200 ≠ 计分。
// 如果动作发出去但进度没动，用户必须能看出来 —— 否则他会以为奖励该到账了。
func TestNoRewardStalledTaskIsReportedHonestly(t *testing.T) {
	// 造一个**无奖励**、目标 3 的任务（复刻 black_cat 的真实形态），
	// 且让计分始终不落定，以便观察"进度未推进"时的报告是否诚实。
	f := newFakeUpstream().addNoReward("black_cat", 1, 3, upstream.TaskAcceptAccepted)
	f.scoreDelayListCalls = 999
	r := testRunner(f)

	item := r.RunOne(context.Background(), testAuth(), "black_cat")

	// 进度确实没动。
	if item.ProgressAfter != "1/3" {
		t.Fatalf("progress_after=%q（假上游不该推进它）", item.ProgressAfter)
	}
	// 必须明确点出"进度未推进"。
	if !strings.Contains(item.Message, "进度未推进") {
		t.Errorf("进度未推进时必须如实说明, got %q", item.Message)
	}
	if item.Claimed {
		t.Error("进度未达标时不该领奖")
	}
}

// TestRunAllContextCancelStops 宿主提前结束本轮时应停止后续项。
func TestRunAllContextCancelStops(t *testing.T) {
	f := newFakeUpstream().
		add("chat_5", 0, 5, upstream.TaskAcceptAccepted).
		add("RichMeow_Chat", 0, 1, upstream.TaskAcceptAccepted)
	r := testRunner(f)

	ctx, cancel := context.WithCancel(context.Background())
	cancel() // 立即取消

	items := r.RunAll(ctx, testAuth())

	cancelled := 0
	for _, it := range items {
		if it.Status == StatusSkipped && strings.Contains(it.Message, "取消") {
			cancelled++
		}
	}
	if cancelled == 0 {
		t.Errorf("取消后应有条目说明被取消, items=%+v", items)
	}
}

// TestClaimFailureKeepsDoneStatus 达标但领奖失败：主流程仍算 done，
// 用 ClaimError 单独表达 —— 标成失败会让用户以为白做了。
func TestClaimFailureKeepsDoneStatus(t *testing.T) {
	f := newFakeUpstream().add("chat_5", 5, 5, upstream.TaskAcceptAccepted)
	// 领奖返回无法解析的响应体 → doJSON 报解析错误（真故障，而非"未达标"）。
	f.claimResponses["chat_5"] = fakeResp{200, `not-json`}
	r := testRunner(f)

	items := r.RunAll(context.Background(), testAuth())

	var found bool
	for _, it := range items {
		if it.Code != "chat_5" {
			continue
		}
		found = true
		if it.Status != StatusDone {
			t.Errorf("领奖失败不该改变主流程状态, got %s", it.Status)
		}
		if it.ClaimError == "" {
			t.Error("领奖失败必须留下 ClaimError")
		}
		if it.Claimed {
			t.Error("领奖失败时不该标记为已领")
		}
	}
	if !found {
		t.Fatal("结果里缺少 chat_5")
	}
}

// TestClaimNotCompletedMessage 上游答「未达标」时的文案要指出
// 「计分可能还在落定」，而不是笼统的失败 —— 这决定用户该重试还是放弃。
func TestClaimNotCompletedMessage(t *testing.T) {
	f := newFakeUpstream().add("chat_5", 5, 5, upstream.TaskAcceptAccepted)
	// 进度回读达标但领奖被拒（计分尚未落定的真实形态）：HTTP 400 + 该文案。
	f.claimResponses["chat_5"] = fakeResp{400, `{"code":400,"msg":"task not completed"}`}
	r := testRunner(f)

	items := r.RunAll(context.Background(), testAuth())

	for _, it := range items {
		if it.Code != "chat_5" {
			continue
		}
		if it.Status != StatusDone {
			t.Errorf("status=%s want done", it.Status)
		}
		if it.ClaimError == "" {
			t.Error("应记录 ClaimError")
		}
		if !strings.Contains(it.Message, "计分") {
			t.Errorf("文案应指出计分可能还在落定: %q", it.Message)
		}
	}
}

// TestRecordedDailyNotRepeated 连点多轮时账号记录按天去重，不刷屏。
//
// Records 为 nil 时 record 必须是安全空操作（宿主的网关子进程无记录器时
// 不能让整个任务崩掉）—— 这条主要守这个不变量。
func TestRecordedDailyNotRepeated(t *testing.T) {
	f := newFakeUpstream().add("chat_5", 0, 5, upstream.TaskAcceptAccepted)
	r := testRunner(f)
	if r.rec != nil {
		t.Fatal("测试应使用 nil Recorder")
	}
	// 连跑两轮都不应 panic。
	for i := 0; i < 2; i++ {
		if items := r.RunAll(context.Background(), testAuth()); len(items) == 0 {
			t.Fatal("应有结果")
		}
	}
}

// TestReportPayloadIsEventArray 上报体必须是事件数组（上游按数组解析）。
func TestReportPayloadIsEventArray(t *testing.T) {
	f := newFakeUpstream().add("RichMeow_Chat", 0, 1, upstream.TaskAcceptAccepted)
	r := testRunner(f)

	r.RunOne(context.Background(), testAuth(), "RichMeow_Chat")

	if len(f.reportBodies) == 0 {
		t.Fatal("应收到上报")
	}
	events := f.reportBodies[0]
	if len(events) == 0 {
		t.Fatal("上报事件数不应为 0")
	}
	if events[0]["eventCode"] != "agent_task_created" {
		t.Errorf("首个事件应为 agent_task_created, got %v", events[0]["eventCode"])
	}
	// 成功回执必须在事件链里（判据）。
	sawSuccess := false
	for _, ev := range events {
		if ev["eventCode"] == "chat_message_response" && ev["isSuccessful"] == true {
			sawSuccess = true
		}
	}
	if !sawSuccess {
		t.Error("对话链缺少成功回执")
	}
	// 桌面指纹必须被注入。
	if events[0]["extName"] != "workbuddy-desktop" {
		t.Errorf("缺少桌面指纹 extName, got %v", events[0]["extName"])
	}
}

// ---------------------------------------------------------------------------
// school_season（校园日）
// ---------------------------------------------------------------------------

// TestSchoolSeasonSendsMiniFingerprintWithActivityID 校园日事件必须是
// **小程序指纹 + activityId**。
//
// 实测背景（2026-09-18，真实账号）：桌面版形状的 chat_request_send 发到同一端点
// 返回 HTTP 200，但 growth 域进度**恒为 0**；换成小程序指纹后 current 0→1、
// accept_status 变 completed、claim 到账 100 积分 + 5 能量。
//
// 所以这条断言锁的是「别为了图省事复用桌面版上报」——
// 复用会让任务永远停在 0/1，而且**不报错**（最难排查的一类）。
func TestSchoolSeasonSendsMiniFingerprintWithActivityID(t *testing.T) {
	f := newFakeUpstream().add("school_season", 0, 1, upstream.TaskAcceptAccepted)
	r := testRunner(f)

	item := r.RunOne(context.Background(), testAuth(), "school_season")
	if item.Status != StatusDone {
		t.Fatalf("status=%s msg=%s", item.Status, item.Message)
	}

	// 找到那条上报，逐字段核对指纹。
	var ev map[string]any
	for _, batch := range f.reportBodies {
		for _, e := range batch {
			if e["eventCode"] == "chat_request_send" {
				ev = e
			}
		}
	}
	if ev == nil {
		t.Fatal("没有发出 chat_request_send 事件")
	}

	checks := map[string]any{
		"source":     "mini_program",
		"ideName":    "wx_app_cloud",
		"ideType":    "WorkBuddy_MP",
		"extName":    "workbuddy-mp",
		"extVersion": "SaaS",
		"mode":       "chat",
		"activityId": upstream.SchoolSeasonActivityID,
	}
	for k, want := range checks {
		if got := ev[k]; got != want {
			t.Errorf("事件字段 %s = %v，want %v（小程序指纹缺一不可）", k, got, want)
		}
	}
	// userId 必须带：缺它服务端返回 200 但静默丢弃（与桌面版上报同一个坑）。
	if got, _ := ev["userId"].(string); got == "" {
		t.Error("事件缺 userId —— 服务端会静默丢弃")
	}
	// **不得**混入桌面版字段：两种指纹混用会被上游看出异常。
	for _, k := range []string{"agentName", "traceId", "requestModelId"} {
		if _, ok := ev[k]; ok {
			t.Errorf("小程序事件不该带桌面版字段 %s", k)
		}
	}
}

// TestSchoolSeasonProgressesAndClaims 端到端：上报 → 计分 → 可领。
func TestSchoolSeasonProgressesAndClaims(t *testing.T) {
	f := newFakeUpstream().add("school_season", 0, 1, upstream.TaskAcceptAccepted)
	r := testRunner(f)

	item := r.RunOne(context.Background(), testAuth(), "school_season")
	if item.Status != StatusDone {
		t.Fatalf("上报应成功，实际 status=%s msg=%s", item.Status, item.Message)
	}
	// 假上游按真实语义计分：只有小程序指纹 + activityId 才会推进。
	if t2 := f.tasks["school_season"]; t2 == nil || t2.current < t2.target {
		t.Fatalf("上报后进度未达标：%+v", t2)
	}
}

// TestSchoolSeasonSkipsWhenAlreadyDone 已达标时不重复上报（幂等）。
func TestSchoolSeasonSkipsWhenAlreadyDone(t *testing.T) {
	f := newFakeUpstream().add("school_season", 1, 1, upstream.TaskAcceptCompleted)
	r := testRunner(f)

	before := len(f.reportBodies)
	item := r.RunOne(context.Background(), testAuth(), "school_season")
	if item.Status != StatusDone {
		t.Fatalf("status=%s", item.Status)
	}
	if got := len(f.reportBodies) - before; got != 0 {
		t.Errorf("已达标不该再上报，实际发了 %d 批", got)
	}
}

// ---------------------------------------------------------------------------
// 国服成长任务集（Sequential_Tasks_1 / _2 / _3）
// ---------------------------------------------------------------------------

// findEvent 在假上游收到的全部上报里找指定 eventCode 的第一条事件。
func findEvent(t *testing.T, f *fakeUpstream, eventCode string) map[string]any {
	t.Helper()
	for _, batch := range f.reportBodies {
		for _, e := range batch {
			if e["eventCode"] == eventCode {
				return e
			}
		}
	}
	t.Fatalf("没有发出 %s 事件", eventCode)
	return nil
}

// countEvents 统计假上游收到的指定 eventCode 事件条数。
func countEvents(f *fakeUpstream, eventCode string) int {
	n := 0
	for _, batch := range f.reportBodies {
		for _, e := range batch {
			if e["eventCode"] == eventCode {
				n++
			}
		}
	}
	return n
}

// TestSequentialActionsRegistered 三个新码必须可查、且**不标 NeedsChat**。
//
// NeedsChat=true 会让界面提示「需要真实对话」，而这三个任务都是纯事件上报
// （_3 只是把同一个事件按真人节奏重复 5 次）—— 标错会让用户以为必须自己
// 去小程序里聊天。
func TestSequentialActionsRegistered(t *testing.T) {
	for _, code := range []string{sequentialChatCode, sequentialChat5Code, sequentialExpertCode} {
		act := actionFor(code)
		if act == nil {
			t.Errorf("%s 未注册动作", code)
			continue
		}
		if act.NeedsChat {
			t.Errorf("%s 被标成 NeedsChat —— 它是事件上报，不需要用户真的去对话", code)
		}
		if act.Desc == "" {
			t.Errorf("%s 缺 Desc（界面要展示）", code)
		}
	}
}

// TestSequentialChatOmitsActivityIDUnlikeSchoolSeason _1 的对话事件**必须不带** activityId。
//
// 这是本次移植里最容易写错、也最难排查的一处：_1 与 school_season 的判据事件
// eventCode 相同（chat_request_send）、mp 指纹完全相同，服务端只靠
// 「有没有 activityId」区分归属 —— 带上就计入校园日活动（成长任务集进度恒 0），
// 不带才计入成长任务集。两种写法都返回 HTTP 200，失败时没有任何错误可看。
//
// 断言用 `_, ok := ev["activityId"]` 而不是比较空字符串：字段注释里写明
// `"activityId":""` 与「没有该字段」在服务端是两种输入，只有 omitempty
// 能让它整体消失。
func TestSequentialChatOmitsActivityIDUnlikeSchoolSeason(t *testing.T) {
	// 一侧：_1 不带。
	f := newFakeUpstream().add("Sequential_Tasks_1", 0, 1, upstream.TaskAcceptAccepted)
	if item := testRunner(f).RunOne(context.Background(), testAuth(), sequentialChatCode); item.Status != StatusDone {
		t.Fatalf("status=%s msg=%s", item.Status, item.Message)
	}
	ev := findEvent(t, f, "chat_request_send")
	for k, want := range map[string]any{
		"source":     "mini_program",
		"ideName":    "wx_app_cloud",
		"ideType":    "WorkBuddy_MP",
		"extName":    "workbuddy-mp",
		"extVersion": "SaaS",
		"mode":       "chat",
	} {
		if got := ev[k]; got != want {
			t.Errorf("事件字段 %s = %v，want %v（mp 指纹缺一不可）", k, got, want)
		}
	}
	if v, ok := ev["activityId"]; ok {
		t.Errorf("Sequential_Tasks_1 的事件带了 activityId=%v —— 服务端会把它算进校园日活动，进度恒为 0", v)
	}
	if got, _ := ev["userId"].(string); got == "" {
		t.Error("事件缺 userId —— 服务端会静默丢弃")
	}

	// 另一侧：school_season 必须带。两者放在同一个用例里对照，
	// 任何「统一成一种形状」的改动都会立刻打破其中一侧。
	f2 := newFakeUpstream().add("school_season", 0, 1, upstream.TaskAcceptAccepted)
	if item := testRunner(f2).RunOne(context.Background(), testAuth(), "school_season"); item.Status != StatusDone {
		t.Fatalf("school_season status=%s msg=%s", item.Status, item.Message)
	}
	if got := findEvent(t, f2, "chat_request_send")["activityId"]; got != upstream.SchoolSeasonActivityID {
		t.Errorf("school_season 的 activityId = %v，want %s", got, upstream.SchoolSeasonActivityID)
	}
}

// TestSequentialAcceptUsesMPPlatformHeader 报名小程序限定任务必须带
// `X-Client-Platform: miniprogram`。
//
// 实测背景（2026-09-30，真实国服账号）：不带该头的报名返回 **HTTP 200 且
// 业务 code=0**，只有逐条结果是 {"status":"error","message":"task not found"} ——
// 从状态码和业务码都看不出失败。后果是任务停在「未报名」，后续上报的事件
// 全部不归账，进度恒为 null。这正是本次移植前真实账号上的故障形态。
func TestSequentialAcceptUsesMPPlatformHeader(t *testing.T) {
	f := newFakeUpstream().add("Sequential_Tasks_1", 0, 1, upstream.TaskAcceptNotAccepted)
	r := testRunner(f)

	item := r.RunOne(context.Background(), testAuth(), sequentialChatCode)
	if item.Status != StatusDone {
		t.Fatalf("status=%s msg=%s", item.Status, item.Message)
	}
	if len(f.acceptPlatforms) != 1 || f.acceptPlatforms[0] != "miniprogram" {
		t.Errorf("报名请求的 X-Client-Platform = %v，want [miniprogram]", f.acceptPlatforms)
	}
	// RunOne 会把整条链路走完（报名 → 上报 → 领奖），所以这里只能断言
	// 「不再是未报名」；若报名没登记，状态会停在 not_accepted，上报也不会计分。
	if t2 := f.tasks["Sequential_Tasks_1"]; t2 == nil || t2.acceptStatus == upstream.TaskAcceptNotAccepted {
		t.Errorf("报名未登记：%+v", t2)
	}
}

// TestAcceptTasksSplitsMPFromPlain 混合批次按码分组报名。
//
// 为什么不能给整批统一加 mp 头：批量报名通常混着普通任务与小程序任务，
// 给普通任务加小程序头是**另一种输入**，其行为未经验证。所以按码拆成两次
// 请求，各自带各自的口径。
func TestAcceptTasksSplitsMPFromPlain(t *testing.T) {
	f := newFakeUpstream().
		add("chat_5", 0, 5, upstream.TaskAcceptNotAccepted).
		add("Sequential_Tasks_1", 0, 1, upstream.TaskAcceptNotAccepted)
	r := testRunner(f)

	res, err := r.acceptTasks(testAuth(), []string{"chat_5", "Sequential_Tasks_1"})
	if err != nil {
		t.Fatalf("accept: %v", err)
	}
	if len(f.acceptCalls) != 2 {
		t.Fatalf("accept 请求数 = %d，want 2（普通 + 小程序各一次）：%v", len(f.acceptCalls), f.acceptCalls)
	}
	for i, want := range []string{"", "miniprogram"} {
		if f.acceptPlatforms[i] != want {
			t.Errorf("第 %d 次报名的 X-Client-Platform = %q，want %q", i+1, f.acceptPlatforms[i], want)
		}
	}
	byCode := map[string]string{}
	for _, one := range res {
		byCode[one.Code] = one.Status
	}
	for _, code := range []string{"chat_5", "Sequential_Tasks_1"} {
		if byCode[code] != upstream.TaskAcceptAccepted {
			t.Errorf("%s 报名状态 = %q，want accepted（res=%v）", code, byCode[code], res)
		}
	}
}

// TestSequentialChatTargetFallbackWhenNoProgress 上游未下发 progress 时 target 兜底 1。
//
// 未报名（或报名刚生效）时上游把 progress 下发为 null，解析出来 target=0。
// 若照 0 算差额就会「0 条上报、直接判定达标」，任务永远点不亮。
func TestSequentialChatTargetFallbackWhenNoProgress(t *testing.T) {
	f := newFakeUpstream().add("Sequential_Tasks_1", 0, 0, upstream.TaskAcceptNotAccepted)
	r := testRunner(f)

	// 直接调动作、绕过 RunOne 的报名，复刻「列表里还没有 progress」的形态。
	before := &upstream.GrowthTask{
		Code: sequentialChatCode, Title: "任务-Sequential_Tasks_1",
		AcceptStatus: upstream.TaskAcceptNotAccepted,
	}
	msg, err := runSequentialChat(r, context.Background(), testAuth(), before)
	if err != nil {
		t.Fatalf("动作返回错误: %v", err)
	}
	if got := countEvents(f, "chat_request_send"); got != 1 {
		t.Errorf("上报 %d 条，want 1（target 兜底 1）msg=%s", got, msg)
	}
}

// TestSequentialActionsRejectMissingTask 三个动作在 before==nil 时都要报错。
//
// RunOne 在动作之前就会拦掉这种形态（StatusSkipped），所以这条守卫是防御性的；
// 但它守的是「动作函数自己也能被安全地直接调用」，避免将来有别的调用方
// 绕过 RunOne 时静默发出一条无归属的上报。
func TestSequentialActionsRejectMissingTask(t *testing.T) {
	f := newFakeUpstream()
	r := testRunner(f)
	acts := []struct {
		code string
		fn   func(*Runner, context.Context, *auth.Auth, *upstream.GrowthTask) (string, error)
	}{
		{sequentialChatCode, runSequentialChat},
		{sequentialChat5Code, runSequentialChat5},
		{sequentialExpertCode, runMiniExpert},
	}
	for _, tc := range acts {
		if _, err := tc.fn(r, context.Background(), testAuth(), nil); err == nil {
			t.Errorf("%s 在任务缺失时应返回错误", tc.code)
		}
	}
	if len(f.reportBodies) != 0 {
		t.Errorf("任务缺失时不该上报，实际发了 %d 批", len(f.reportBodies))
	}
}

// TestSequentialChat5ProgressesAndClaims _3「5 次有效对话」端到端：
// 按差额逐条上报 → 累加到 5/5 → 领奖 300 积分。
//
// 用 addTask 而不是 add：这个任务的真实奖励是 300 积分（不是默认的 100），
// 写死会让「到账多少」失去意义（假上游已改成从任务本身取奖励值）。
func TestSequentialChat5ProgressesAndClaims(t *testing.T) {
	f := newFakeUpstream().addTask("Sequential_Tasks_3", 0, 5, upstream.TaskAcceptInProgress, 300, 5)
	r := testRunner(f)

	item := r.RunOne(context.Background(), testAuth(), sequentialChat5Code)
	if item.Status != StatusDone {
		t.Fatalf("status=%s msg=%s", item.Status, item.Message)
	}
	if got := countEvents(f, "chat_request_send"); got != 5 {
		t.Errorf("上报 %d 条，want 5（target=5）", got)
	}
	if !item.Claimed || item.Credit != 300 || item.Energy != 5 {
		t.Errorf("领奖结果 claimed=%v credit=%d energy=%d，want true/300/5",
			item.Claimed, item.Credit, item.Energy)
	}
	if t2 := f.tasks["Sequential_Tasks_3"]; t2 == nil || t2.current < t2.target {
		t.Errorf("进度未达标：%+v", t2)
	}

	// 每次上报的 conversationId 必须互不相同：服务端按会话去重，
	// 复用同一个 id 会让「5 次对话」被算成 1 次。
	ids := map[string]bool{}
	for _, batch := range f.reportBodies {
		for _, e := range batch {
			if e["eventCode"] != "chat_request_send" {
				continue
			}
			id, _ := e["conversationId"].(string)
			if id == "" {
				t.Error("上报缺 conversationId")
				continue
			}
			if ids[id] {
				t.Errorf("conversationId 重复：%s（会被服务端算作同一次对话）", id)
			}
			ids[id] = true
		}
	}
	if len(ids) != 5 {
		t.Errorf("不同 conversationId 有 %d 个，want 5", len(ids))
	}
}

// TestSequentialChat5OnlyReportsTheGap 已有部分进度时只补差额。
//
// 这不是优化而是预算问题：_3 每条事件要等 45 秒（反作弊节奏），
// 重报全部 5 条会白烧 3 分多钟 —— 而宿主给整轮的超时是 10 分钟
//（cmd/server 的 growTaskTimeout）。
func TestSequentialChat5OnlyReportsTheGap(t *testing.T) {
	f := newFakeUpstream().addTask("Sequential_Tasks_3", 3, 5, upstream.TaskAcceptInProgress, 300, 5)
	r := testRunner(f)

	item := r.RunOne(context.Background(), testAuth(), sequentialChat5Code)
	if item.Status != StatusDone {
		t.Fatalf("status=%s msg=%s", item.Status, item.Message)
	}
	if got := countEvents(f, "chat_request_send"); got != 2 {
		t.Errorf("上报 %d 条，want 2（3/5 → 只补 2 条）", got)
	}
	if !item.Claimed {
		t.Errorf("补满后应领奖，item=%+v", item)
	}
}

// TestMiniExpertReportsMarketExpert _2 的上报必须带**市场里真实的** ex_ 专家 id。
//
// 实测背景：专家 id 是服务端校验项 —— 编造的 id 照样返回 HTTP 200 但不计分，
// 所以动作必须先用 MarketExpertList 取一个真 id（这里断言取的正是市场返回的
// 第一个专家），而不是硬编码或留空。
func TestMiniExpertReportsMarketExpert(t *testing.T) {
	f := newFakeUpstream().add("Sequential_Tasks_2", 0, 1, upstream.TaskAcceptAccepted)
	r := testRunner(f)

	item := r.RunOne(context.Background(), testAuth(), sequentialExpertCode)
	if item.Status != StatusDone {
		t.Fatalf("status=%s msg=%s", item.Status, item.Message)
	}
	ev := findEvent(t, f, "expert_actual_use")
	for k, want := range map[string]any{
		"source":     "mini_program",
		"ideName":    "wx_app_cloud",
		"ideType":    "WorkBuddy_MP",
		"extName":    "workbuddy-mp",
		// extVersion 与对话事件的 SaaS 刻意不同：两个事件来自小程序内
		// 不同 SDK 版本，统一反而会成为指纹异常。
		"extVersion":     "2.2.8",
		"type":           "send_message",
		"characterCount": float64(12),
		"id":             "ex_1",
		"name":           "ex_1",
		"expertTitle":    "专家一",
		"expertType":     "agent",
	} {
		if got := ev[k]; got != want {
			t.Errorf("事件字段 %s = %v，want %v", k, got, want)
		}
	}
	if got, _ := ev["userId"].(string); got == "" {
		t.Error("事件缺 userId —— 服务端会静默丢弃")
	}
	// 与 school 域的专家事件是两套口径：这两个字段不该出现。
	for _, k := range []string{"activityId", "conversationId"} {
		if v, ok := ev[k]; ok {
			t.Errorf("mp 专家事件不该带 %s=%v（它与 school 域专家事件是两套口径）", k, v)
		}
	}
}

// TestMiniExpertSkipsWhenMarketUnavailable 市场拉不到时**整个任务不动作**。
//
// 这是「宁可不做」的守卫：判据事件的专家 id 必须真实存在，拿不到市场就发不出
// 有效事件 —— 而发一条注定不计分的事件比什么都不做更糟：用户看到「已上报」，
// 任务却永远点不亮。所以此时连上报都不该发。
func TestMiniExpertSkipsWhenMarketUnavailable(t *testing.T) {
	f := newFakeUpstream().add("Sequential_Tasks_2", 0, 1, upstream.TaskAcceptAccepted)
	f.marketExpertFail = true
	r := testRunner(f)

	before := len(f.reportBodies)
	item := r.RunOne(context.Background(), testAuth(), sequentialExpertCode)
	if item.Status != StatusDone {
		t.Fatalf("status=%s msg=%s", item.Status, item.Message)
	}
	if got := len(f.reportBodies) - before; got != 0 {
		t.Errorf("市场不可用时发了 %d 批上报，want 0（宁可不做也不发注定不计分的事件）", got)
	}
	if !strings.Contains(item.Message, "专家市场不可用") {
		t.Errorf("文案应说明市场不可用，got %q", item.Message)
	}
	if len(f.claimCalls) != 0 {
		t.Errorf("未上报就领奖：%v", f.claimCalls)
	}
}

// ---------------------------------------------------------------------------
// 日期锁任务（locked + unlock_at）
// ---------------------------------------------------------------------------

// lockedUnlockAt 复刻 2026-09-30 实测 Sequential_Tasks_2 的解锁时刻。
const lockedUnlockAt = "2026-10-01T00:00:00+08:00"

// lockedWantText 按 lockedText 的口径算出期望文案。
//
// 不写死 "2026-10-01 00:00"：lockedText 用 Local() 渲染，写死会在非 +08:00
// 的机器上失败 —— 那是时区差异，不是缺陷。
func lockedWantText(t *testing.T) string {
	t.Helper()
	ts, err := time.Parse(time.RFC3339, lockedUnlockAt)
	if err != nil {
		t.Fatalf("用例自身的解锁时刻写错了: %v", err)
	}
	return "尚未解锁（" + ts.Local().Format("2006-01-02 15:04") + " 开放）"
}

// TestLockedTaskIsSkippedNotRun 锁着的任务**不发任何事件**，判 skipped。
//
// 这是 2026-09-30 实测出来的真缺陷的回归钉：当时对 locked 的
// Sequential_Tasks_2 执行 run，上游返回 **HTTP 200**、耗时 15.9s、
// 报文是 `{"status":"done","message":"已上报小程序专家使用事件（福帮手）"}`，
// 而进度 before/after 都是 not_accepted —— 事件被服务端静默丢弃。
//
// 只挡 Claimed()/Claimable() 是不够的：锁定是**第三种等待**，它既不是
// 「做完了」也不是「现在能做」。光看状态码根本发现不了，所以必须断言
// 「一条上报都没发」而不是断言返回值好看。
func TestLockedTaskIsSkippedNotRun(t *testing.T) {
	f := newFakeUpstream().addLocked(sequentialExpertCode, 0, 1, upstream.TaskAcceptAccepted, lockedUnlockAt)
	r := testRunner(f)

	item := r.RunOne(context.Background(), testAuth(), sequentialExpertCode)

	if item.Status != StatusSkipped {
		t.Fatalf("锁定任务 status=%s，want %s（不能报 done —— 那是假成功）", item.Status, StatusSkipped)
	}
	if got := len(f.reportBodies); got != 0 {
		t.Errorf("锁定任务发了 %d 批上报，want 0（上游会静默丢弃，发了只会让人以为已完成）", got)
	}
	if got := len(f.claimCalls); got != 0 {
		t.Errorf("锁定任务调了领奖：%v", got)
	}
	if !strings.Contains(item.Message, "尚未解锁") {
		t.Errorf("文案应说明尚未解锁，got %q", item.Message)
	}
	// 解锁时刻必须透传出来，否则用户只知道「不能做」而不知道「等到什么时候」。
	if want := lockedWantText(t); !strings.Contains(item.Message, want) {
		t.Errorf("文案应带解锁时刻 %q，got %q", want, item.Message)
	}
}

// TestLockedTaskSkipsAcceptToo 锁定任务连**报名**都不该走。
//
// NeedsAccept() 里已含 `&& !t.Locked`（上游 tasks.go），所以 RunOne 不会
// 为锁定任务发报名请求。这条用例把该契约钉住：报名本身虽无害，但会给用户
// 「已经开始做了」的错觉，而实际什么都推不动。
func TestLockedTaskSkipsAcceptToo(t *testing.T) {
	f := newFakeUpstream().addLocked(sequentialExpertCode, 0, 0, upstream.TaskAcceptNotAccepted, lockedUnlockAt)
	r := testRunner(f)

	item := r.RunOne(context.Background(), testAuth(), sequentialExpertCode)

	if item.Status != StatusSkipped {
		t.Fatalf("status=%s，want %s", item.Status, StatusSkipped)
	}
	if got := len(f.acceptCalls); got != 0 {
		t.Errorf("对锁定任务发了 %d 次报名，want 0", got)
	}
}

// TestPendingTasksExcludesLocked 锁定任务不进「一键完成」的作用范围。
//
// 这是缺陷的另一半：runAction 的守卫只管「被点到的那一项」，
// 而 PendingTasks 决定「一键完成会点哪些」。漏了这里，RunAll 照样会
// 对锁定任务发事件 —— 守卫形同虚设。
func TestPendingTasksExcludesLocked(t *testing.T) {
	f := newFakeUpstream().
		addLocked(sequentialExpertCode, 0, 1, upstream.TaskAcceptAccepted, lockedUnlockAt).
		add("chat_5", 0, 5, upstream.TaskAcceptNotAccepted)
	r := testRunner(f)

	pending, err := r.PendingTasks(context.Background(), testAuth())
	if err != nil {
		t.Fatalf("PendingTasks: %v", err)
	}
	for _, v := range pending {
		if v.Code == sequentialExpertCode {
			t.Errorf("锁定任务 %s 出现在待办里（一键完成会对它发事件）：%+v", v.Code, v)
		}
	}
	// 反向断言：不能因为过滤把正常任务也滤掉了（否则这条用例可以靠「返回空」通过）。
	var found bool
	for _, v := range pending {
		if v.Code == "chat_5" {
			found = true
		}
	}
	if !found {
		t.Errorf("正常任务 chat_5 被误滤：%+v", pending)
	}
}

// TestListTasksShowsUnlockTime 列表把锁定与解锁时刻透出给界面。
//
// 界面需要区分「等时间到」和「得自己去点」：两者都不可自动执行，
// 但一个会自己变好，另一个永远不变。只看 status 字符串是分不出来的。
func TestListTasksShowsUnlockTime(t *testing.T) {
	f := newFakeUpstream().addLocked(sequentialExpertCode, 0, 0, upstream.TaskAcceptNotAccepted, lockedUnlockAt)
	r := testRunner(f)

	views, err := r.ListTasks(context.Background(), testAuth())
	if err != nil {
		t.Fatalf("ListTasks: %v", err)
	}
	var got *TaskView
	for i := range views {
		if views[i].Code == sequentialExpertCode {
			got = &views[i]
		}
	}
	if got == nil {
		t.Fatalf("列表里没有 %s：%+v", sequentialExpertCode, views)
	}
	if !got.Locked {
		t.Error("Locked 未透出（界面无法区分「等时间到」与「需人工」）")
	}
	if got.UnlockAt != lockedUnlockAt {
		t.Errorf("UnlockAt = %q，want %q", got.UnlockAt, lockedUnlockAt)
	}
	if got.Status != ViewLocked {
		t.Errorf("Status = %q，want %q", got.Status, ViewLocked)
	}
	if want := lockedWantText(t); got.StatusText != want {
		t.Errorf("StatusText = %q，want %q", got.StatusText, want)
	}
}
