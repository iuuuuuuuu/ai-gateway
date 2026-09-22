package scheduler

// product_scope_test.go —— **产品闸门**的回归测试（2026-09-22）。
//
// # 所有者现场
//
//	「这个qoder怎么执行workbuddy的任务了?」
//	「zcode我看到记录里面,也会跑workbuddy的任务,这不要串任务好吗?」
//
// 截图里，一个 **Qoder** 账号（`{qoder-uid}-…`）的记录中出现了
// 「开学季活动」「活跃上报」「夜猫子任务」，而且开学季那条报的是
// `upstream client (http 401)`。
//
// # 根因
//
// 所有养号任务都遍历 `Pool.List()` —— 那是**全部产品**的账号。
// 而过滤条件只有区域（`checkinScopeAllows` / `upstream.IsIntl`），
// **一个产品判断都没有**。
//
// 于是 Qoder / ZCode 账号被当成 WorkBuddy 账号：
//
//  1. 带着自己的凭证去打 WorkBuddy 的 growth 端点 ⇒ **401**
//  2. 在账号记录里写下**不属于它**的任务条目 ⇒ 用户以为账号坏了
//
// ⚠ 区域与产品是**两个正交维度**，不能用其中一个代替另一个：
// Qoder/ZCode 账号的 Domain 不是 workbuddy.ai，所以
// `IsIntl` 判成 false ⇒ 被当成"国服 WorkBuddy 账号" ⇒ 放行。
//
// # 这组测试钉住什么
//
// 对**每一个**遍历账号的养号任务，断言：
//
//	· Qoder / ZCode 账号 ⇒ **一个请求都不该发**（连记录都不该写）
//	· WorkBuddy 账号     ⇒ 照常发（闸门不能把正常的也挡掉）
//
// 第二半同样重要 —— 只测"不发"的话，把闸门写成 `return` 常量也能过。

import (
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/pool"
	"workbuddy2api/internal/upstream"
)

// productProbe 记录**所有**收到的请求路径，用来判断"有没有发请求"。
//
// ⚠ 记录的是**路径**而不只是"收到几次"：不同任务打不同端点，
// 只看次数无法区分"闸门生效"与"打到了别的任务"。
type productProbe struct {
	mu    sync.Mutex
	paths []string
}

func (p *productProbe) handler() http.HandlerFunc {
	return func(w http.ResponseWriter, req *http.Request) {
		p.mu.Lock()
		p.paths = append(p.paths, req.URL.Path)
		p.mu.Unlock()

		w.Header().Set("Content-Type", "application/json")
		// ⚠ 每个端点都要回**形状正确**的响应，否则调用方会因为解析失败
		// 而提前 return —— 那样"发过请求"这件事仍能观测到（我们记的是路径），
		// 但 WorkBuddy 的对照组可能在中途就失败，掩盖后续行为。
		//
		// 这里的响应只求形状对（本测试不验证业务语义，那是各任务自己的测试）。
		switch {
		case strings.HasSuffix(req.URL.Path, "/growth/streak"):
			_, _ = w.Write([]byte(`{"code":0,"msg":"OK","data":{"streak":{"days":1}}}`))
		default:
			// ⚠ **不能**用 http.Error（那会回 4xx/5xx）：
			// 上游错误会让调用方走错误分支，可能提前 return，
			// 于是"有没有发请求"这个观测点就被后面的逻辑掩盖了。
			_, _ = w.Write([]byte(`{"code":0,"msg":"OK","data":{}}`))
		}
	}
}

func (p *productProbe) seen() []string {
	p.mu.Lock()
	defer p.mu.Unlock()
	out := make([]string, len(p.paths))
	copy(out, p.paths)
	return out
}

// newProductScopedScheduler 起一个指向 fake 上游的调度器，账号按传入的造。
func newProductScopedScheduler(t *testing.T, p *productProbe, accounts ...*auth.Auth) *Scheduler {
	t.Helper()
	srv := httptest.NewServer(p.handler())
	t.Cleanup(srv.Close)

	up := &upstream.Client{
		HTTP:          srv.Client(),
		ChatBaseCN:    srv.URL,
		BillingBaseCN: srv.URL,
		BaseIntl:      srv.URL,
	}
	pl := pool.New("")
	for _, a := range accounts {
		pl.Add(a)
	}
	return New(Config{
		Pool:                pl,
		Upstream:            up,
		ActivityReportCount: 1,
	})
}

// qoderAccount / zcodeAccount 造非 WorkBuddy 的账号。
//
// ⚠ 显式设 Product —— 这是本测试的自变量。
// 同时给 Domain 一个**看起来像国服**的值：这正是真实故障成立的条件
//（若 Domain 是 workbuddy.ai，区域过滤会顺手挡掉，问题就不会暴露）。
func qoderAccount(uid string) *auth.Auth {
	return &auth.Auth{
		UID: uid, AccessToken: "qoder-tok", RefreshToken: "qoder-rt",
		Product: auth.ProductQoder, Domain: "qoder.com",
	}
}

func zcodeAccount(uid string) *auth.Auth {
	return &auth.Auth{
		UID: uid, AccessToken: "zcode-tok", RefreshToken: "zcode-rt",
		Product: auth.ProductZcode, Domain: "z.ai",
	}
}

func workbuddyAccount(uid string) *auth.Auth {
	return &auth.Auth{
		UID: uid, AccessToken: "wb-tok", RefreshToken: "wb-rt",
		Product: auth.ProductWorkBuddy, Domain: "www.workbuddy.cn",
	}
}

// TestActivitySkipsNonWorkBuddyAccounts 活跃上报不能碰 Qoder/ZCode 账号。
//
// 这是所有者截图里最刺眼的一条：Qoder 账号的记录里写着「活跃上报 已上报 3/3」。
func TestActivitySkipsNonWorkBuddyAccounts(t *testing.T) {
	probe := &productProbe{}
	s := newProductScopedScheduler(t, probe, qoderAccount("q1"), zcodeAccount("z1"))

	s.RunActivityNow()

	if got := probe.seen(); len(got) != 0 {
		t.Errorf("活跃上报对 Qoder/ZCode 账号发了 %d 个请求（应为 0）——\n"+
			"这些任务打的是 **WorkBuddy** 端点，带 Qoder/ZCode 凭证必然 401，\n"+
			"而且会在账号记录里写下不属于它的任务。\n"+
			"收到的路径：%v", len(got), got)
	}
}

// TestActivityStillRunsForWorkBuddy 闸门不能把正常的 WorkBuddy 账号也挡掉。
//
// ⚠ 这条是上一条的**对照**：只测"不发"的话，
// 把闸门误写成"永远不发"也能过 —— 那才是更严重的回归。
func TestActivityStillRunsForWorkBuddy(t *testing.T) {
	probe := &productProbe{}
	s := newProductScopedScheduler(t, probe, workbuddyAccount("w1"))

	s.RunActivityNow()

	if got := probe.seen(); len(got) == 0 {
		t.Error("活跃上报对 **WorkBuddy** 账号一个请求都没发 —— 闸门写过宽，把正常的也挡掉了")
	}
}

// TestCheckinSkipsNonWorkBuddyAccounts 签到不能碰 Qoder/ZCode 账号。
func TestCheckinSkipsNonWorkBuddyAccounts(t *testing.T) {
	probe := &productProbe{}
	s := newProductScopedScheduler(t, probe, qoderAccount("q1"), zcodeAccount("z1"))

	s.RunCheckinNow()

	if got := probe.seen(); len(got) != 0 {
		t.Errorf("签到对 Qoder/ZCode 账号发了 %d 个请求（应为 0）：%v", len(got), got)
	}
}

// TestCreditRefreshSkipsNonWorkBuddyAccounts 余额刷新不能碰 Qoder/ZCode 账号。
//
// ⚠ 这条尤其重要：余额刷新是**每 15 分钟**跑一次的巡检。
// 若不判产品，那两个账号会**每 15 分钟**吃一次 401，
// 把账号记录刷满无意义的错误 —— 用户会以为账号持续故障。
func TestCreditRefreshSkipsNonWorkBuddyAccounts(t *testing.T) {
	probe := &productProbe{}
	s := newProductScopedScheduler(t, probe, qoderAccount("q1"), zcodeAccount("z1"))

	s.RunCreditRefreshNow()

	if got := probe.seen(); len(got) != 0 {
		t.Errorf("余额刷新对 Qoder/ZCode 账号发了 %d 个请求（应为 0）：%v\n"+
			"它是 15 分钟一次的巡检 —— 不判产品会让这两个账号持续吃 401，"+
			"把账号记录刷满不属于它们的错误。", len(got), got)
	}
}

// TestKeepaliveSkipsNonWorkBuddyAccounts token 保活不能碰 Qoder/ZCode 账号。
func TestKeepaliveSkipsNonWorkBuddyAccounts(t *testing.T) {
	probe := &productProbe{}
	s := newProductScopedScheduler(t, probe, qoderAccount("q1"), zcodeAccount("z1"))

	s.RunKeepaliveNow()

	if got := probe.seen(); len(got) != 0 {
		t.Errorf("token 保活对 Qoder/ZCode 账号发了 %d 个请求（应为 0）：%v", len(got), got)
	}
}

// TestTravelSkipsNonWorkBuddyAccounts 猫猫旅行不能碰 Qoder/ZCode 账号。
func TestTravelSkipsNonWorkBuddyAccounts(t *testing.T) {
	probe := &productProbe{}
	s := newProductScopedScheduler(t, probe, qoderAccount("q1"), zcodeAccount("z1"))

	s.RunTravelNow()

	if got := probe.seen(); len(got) != 0 {
		t.Errorf("猫猫旅行对 Qoder/ZCode 账号发了 %d 个请求（应为 0）：%v\n"+
			"Qoder/ZCode 账号根本没有猫猫，跑它只会拿 401。", len(got), got)
	}
}

// TestAccountScopeSkipRejectsNonWorkBuddy 闸门函数本身对所有 WorkBuddy 专属任务生效。
//
// 直接测函数（而不是只测各任务的副作用）：新增任务时若忘了接闸门，
// 这条测试能立刻暴露 —— 而不是等用户在记录里看到脏数据。
//
// ⚠ 返回值语义是 `(reason, ok)`：**第二个才是"能不能跑"**。
// 我第一版测试把它当成了 `skip`，于是断言全反 —— 那种错法很隐蔽，
// 因为测试会"稳定地失败"而不是报编译错，容易被误判成实现有问题。
func TestAccountScopeSkipRejectsNonWorkBuddy(t *testing.T) {
	s := New(Config{CheckinScope: "cn"})

	// 这些任务全部是 WorkBuddy 专属（打 WorkBuddy 的 growth/billing 端点）
	workbuddyOnly := []string{
		TaskNameActivity, TaskNameNightOwl, TaskNameSchool,
		TaskNameGrowthMap, TaskNameTrial,
	}

	for _, name := range workbuddyOnly {
		for _, acc := range []*auth.Auth{qoderAccount("q1"), zcodeAccount("z1")} {
			reason, ok := s.accountScopeSkip(name, acc)
			if ok {
				t.Errorf("accountScopeSkip(%q, product=%q) 应判定**不能跑**（该任务 WorkBuddy 专属），实际放行",
					name, acc.ProductOf())
			}
			if !strings.Contains(reason, "WorkBuddy") {
				t.Errorf("accountScopeSkip(%q) 的说明应点明「WorkBuddy 专属」以便排查，实际：%q",
					name, reason)
			}
		}
		// 对照：WorkBuddy 账号**不该**因为产品被挡
		//（trial 会因区域被挡，那是另一条规则，故只断言"不是产品原因"）
		if reason, ok := s.accountScopeSkip(name, workbuddyAccount("w1")); !ok {
			if strings.Contains(reason, "WorkBuddy 专属") {
				t.Errorf("accountScopeSkip(%q, workbuddy) 不应因**产品**被挡，实际：%q",
					name, reason)
			}
		}
	}
}
