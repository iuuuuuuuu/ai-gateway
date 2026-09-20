package zcode

// claim_scheduler_test.go 自动领取**调度语义**的回归。
//
// # 守的是什么（所有者 2026-09-20 明确要求）
//
//	「zcode要按照实际逻辑去做啊，他那个仓库怎么做我们就怎么做」
//
// 参考实现（TriDefender/zcode-api `src/claim/scheduler.ts`）的调度是
// **一张决策表**，而这张表里每一条都影响"会不会被判成非人类活动"。
// 故这里逐条锁定：
//
//	① hold 硬闸：未到期**连 preview 都不发**（不是"发了但不管结果"）
//	② 成功 → hold 到套餐截止（不再每 5 分钟白探）
//	③ already_claimed / quota_exhausted → hold 到 failureEndsAt（上限 24h）
//	④ preview 404 → **正常轮询**（不是 error backoff）
//	⑤ login_required → **永久停止**（不是重试）
//	⑥ 其它失败 → cooldown
//	⑦ 业务码映射逐条正确
//
// # 为什么用 httptest 而不是打真上游
//
// ① 所有者的 ZCode 账号此前被风控封过，**不能拿它做压力测试**
// ② 决策表是纯逻辑，喂假上游能覆盖得比真机更全（真机只能碰到一两种码）
import (
	"context"
	"fmt"
	"net/http"
	"net/http/httptest"
	"sync/atomic"
	"testing"
	"time"
)

// claimUpstream 假的 ZCode 上游（只实现 preview/claim 两个端点）。
type claimUpstream struct {
	previewStatus int    // 0 = 200
	previewBody   string // 空 = 用默认（无可领）
	claimStatus   int    // 0 = 200
	claimBody     string

	previewHits int32
	claimHits   int32
}

func (u *claimUpstream) handler() http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		switch {
		case hasSub(r.URL.Path, "/billing/preview"):
			atomic.AddInt32(&u.previewHits, 1)
			if u.previewStatus != 0 {
				w.WriteHeader(u.previewStatus)
				_, _ = w.Write([]byte(u.previewBody))
				return
			}
			body := u.previewBody
			if body == "" {
				body = `{"code":0,"data":{"server_time":1,"plans":[]}}`
			}
			w.Header().Set("Content-Type", "application/json")
			_, _ = w.Write([]byte(body))
		case hasSub(r.URL.Path, "/billing/claim"):
			atomic.AddInt32(&u.claimHits, 1)
			if u.claimStatus != 0 {
				w.WriteHeader(u.claimStatus)
				_, _ = w.Write([]byte(u.claimBody))
				return
			}
			body := u.claimBody
			if body == "" {
				body = `{"code":0,"data":{"plan":{"plan_id":"p1"}}}`
			}
			w.Header().Set("Content-Type", "application/json")
			_, _ = w.Write([]byte(body))
		default:
			http.Error(w, "unexpected: "+r.URL.Path, 404)
		}
	}
}

func hasSub(s, sub string) bool {
	for i := 0; i+len(sub) <= len(s); i++ {
		if s[i:i+len(sub)] == sub {
			return true
		}
	}
	return false
}

// newClaimTestScheduler 造一个指向假上游的调度器 + 一个凭证加载器。
func newClaimTestScheduler(t *testing.T, u *claimUpstream, creds []*Cred) (*ClaimScheduler, *httptest.Server) {
	t.Helper()
	srv := httptest.NewServer(u.handler())
	t.Cleanup(srv.Close)

	client := New()
	client.HTTP = srv.Client()
	// 把两个 billing 端点都指到假上游。
	client.Identity = Identity{}
	// ClaimHost 由 Provider 决定；测试里用 SetClaimBaseForTest 覆盖。
	client.SetClaimBaseForTest(srv.URL)

	s := NewClaimScheduler(client,
		func() ([]*Cred, []string, error) { return creds, nil, nil },
		func() bool { return true },
	)
	return s, srv
}

// oneCred 一个带 JWT 与 deviceMid 的凭证。
func oneCred(uid string) *Cred {
	return &Cred{UID: uid, JWT: "jwt-" + uid, DeviceMid: "11111111-2222-4333-8444-555555555555"}
}

// planJSON 造一个 preview 响应，含一个可领套餐与截止时刻。
func planJSON(planID string, endsAt int64, priority int) string {
	return fmt.Sprintf(`{"code":0,"data":{"server_time":1,"plans":[
		{"plan_id":%q,"name":"n","priority":%d,"status":"claimable","ends_at":%d}]}}`,
		planID, priority, endsAt)
}

// TestClaimBusinessCodeMapping 业务码 → 失败种类的映射**逐条**正确。
//
// 这张表镜像参考实现的 `classifyClaimCode`（它又镜像桌面端）。
// 每个码对应不同的下次轮询间隔，改错会让轮询节奏偏离参考实现。
func TestClaimBusinessCodeMapping(t *testing.T) {
	cases := map[string]ClaimFailureKind{
		"1001": ClaimFailNotFound,
		"1002": ClaimFailUnavailable,
		"1003": ClaimFailAlreadyClaimed,
		"1004": ClaimFailIneligible,
		"1005": ClaimFailQuotaExhausted,
		"3001": ClaimFailInvalidRequest,
		"3007": ClaimFailCaptcha,
		"401":  ClaimFailLoginRequired,
	}
	for code, want := range cases {
		got := claimKindOfCode(code)
		if got != want {
			t.Errorf("业务码 %s 应映射为 %q，实际 %q", code, want, got)
		}
	}
	// 未知码不映射（交给上层按 HTTP 状态推）
	if got := claimKindOfCode("9999"); got != ClaimFailNone {
		t.Errorf("未知码应返回空种类（交由 HTTP 状态推断），实际 %q", got)
	}
}

// TestClassifyClaimErrorFallsBackToStatus 无业务码时按 HTTP 状态推。
func TestClassifyClaimErrorFallsBackToStatus(t *testing.T) {
	k, _ := classifyClaimError(&Error{Status: 401})
	if k != ClaimFailLoginRequired {
		t.Errorf("HTTP 401 应判为 login_required，实际 %q", k)
	}
	k2, _ := classifyClaimError(&Error{Status: 500})
	if k2 != ClaimFailHTTPError {
		t.Errorf("HTTP 500 应判为 http_error，实际 %q", k2)
	}
	// 有业务码时**业务码优先**（HTTP 200 但 code=1004）
	k3, c3 := classifyClaimError(&Error{Status: 200, Code: "1004"})
	if k3 != ClaimFailIneligible || c3 != "1004" {
		t.Errorf("有业务码时应优先按码分类，实际 kind=%q code=%q", k3, c3)
	}
}

// TestClaimSuccessHoldsUntilPlanEnds 成功 → hold 到套餐截止。
//
// 这是参考实现最重要的一条：领到之后**不再每 5 分钟白探**，
// 而是等这个套餐结束再回来（那时可能有新的）。
func TestClaimSuccessHoldsUntilPlanEnds(t *testing.T) {
	ends := time.Now().Add(2 * time.Hour).Unix()
	u := &claimUpstream{previewBody: planJSON("plan-x", ends, 90)}
	s, _ := newClaimTestScheduler(t, u, []*Cred{oneCred("u1")})

	wait := s.tickOnce(context.Background())
	if wait < 90*time.Minute || wait > 3*time.Hour {
		t.Fatalf("领取成功后应 hold 到套餐截止（≈2h），实际 %v", wait)
	}
	if got := s.LastResult().Outcome; got != ClaimOutcomeClaimed {
		t.Errorf("结果应为 claimed，实际 %q", got)
	}
	if n := atomic.LoadInt32(&u.claimHits); n != 1 {
		t.Errorf("应只 claim 一次，实际 %d 次", n)
	}
}

// TestHoldGateSkipsPreviewEntirely hold 期内**连 preview 都不发**。
//
// ⚠ 这条是关键：没有它，"5 分钟轮询"会变成对上游的持续探测。
// 判据不能只看"没 claim"，必须断言 **previewHits 没有增加**。
//
// ⚠ 也**不能**用 `LastResult().Outcome` 判断第二轮的结果：
// 被 hold 拦住时根本不进 `tickAccount`，故那个字段仍是**第一轮的**
//（我第一版就是这么写的，读到 "claimed" 并一度误以为 hold 没生效 ——
//  实际 hold 是好的，是断言读错了字段）。
// 正确的判据是**副作用**（preview 命中数）与**返回的等待时长**。
func TestHoldGateSkipsPreviewEntirely(t *testing.T) {
	ends := time.Now().Add(3 * time.Hour).Unix()
	u := &claimUpstream{previewBody: planJSON("plan-x", ends, 90)}
	s, _ := newClaimTestScheduler(t, u, []*Cred{oneCred("u1")})

	// 第一轮：领到 → hold
	s.tickOnce(context.Background())
	hitsAfterFirst := atomic.LoadInt32(&u.previewHits)

	// 第二轮：应被 hold 闸住 —— **不发** preview
	wait := s.tickOnce(context.Background())
	hitsAfterSecond := atomic.LoadInt32(&u.previewHits)

	if hitsAfterSecond != hitsAfterFirst {
		t.Fatalf("hold 期内**不该发 preview**（参考实现的 skipped_hold 语义）；"+
			"preview 命中从 %d 变成 %d —— 那会让 5 分钟轮询变成持续探测",
			hitsAfterFirst, hitsAfterSecond)
	}
	// 等待时长应仍是那个 hold（≈3h），而不是回落到正常轮询
	if wait < 2*time.Hour {
		t.Errorf("被 hold 时应返回**剩余 hold**（≈3h），实际 %v —— "+
			"若接近 5 分钟说明 hold 没参与下次等待的计算", wait)
	}
}

// TestAlreadyClaimedHoldsUntilFailureEnds already_claimed → hold 到 failureEndsAt。
//
// 参考实现：`already_claimed`/`quota_exhausted` 且 failureEndsAt 在未来时，
// hold 到那个时刻（上限 24h），不必每 5 分钟白探。
func TestAlreadyClaimedHoldsUntilFailureEnds(t *testing.T) {
	ends := time.Now().Add(6 * time.Hour).Unix()
	u := &claimUpstream{
		previewBody: planJSON("plan-x", ends, 90),
		claimStatus: 200,
		claimBody:   `{"code":1003,"msg":"already claimed"}`,
	}
	s, _ := newClaimTestScheduler(t, u, []*Cred{oneCred("u1")})

	wait := s.tickOnce(context.Background())
	if wait < 5*time.Hour {
		t.Fatalf("already_claimed 且有未来截止时，应 hold 到那个时刻（≈6h），实际 %v", wait)
	}
	if k := s.LastResult().Kind; k != ClaimFailAlreadyClaimed {
		t.Errorf("种类应为 already_claimed，实际 %q", k)
	}
}

// TestHoldCappedAt24h 上游给很远的截止时，hold 有 24h 上限。
//
// 没有上限就再也不会探测 —— 而中途可能上新别的套餐。
func TestHoldCappedAt24h(t *testing.T) {
	ends := time.Now().Add(30 * 24 * time.Hour).Unix() // 30 天后
	u := &claimUpstream{previewBody: planJSON("plan-x", ends, 90)}
	s, _ := newClaimTestScheduler(t, u, []*Cred{oneCred("u1")})

	wait := s.tickOnce(context.Background())
	if wait > ClaimHoldCap+time.Minute {
		t.Fatalf("hold 应被上限 %v 截断，实际 %v", ClaimHoldCap, wait)
	}
	if wait < ClaimHoldCap-time.Minute {
		t.Errorf("hold 应达到上限 %v（上游给的是 30 天），实际 %v", ClaimHoldCap, wait)
	}
}

// TestPreview404PollsNormally 活动未上线（404）走**正常轮询**，不是 error 退避。
//
// 参考实现的分法：「preview 抛 404 → idle → normal poll」，
// 因为"活动还没部署"是**预期状态**，不是故障。
func TestPreview404PollsNormally(t *testing.T) {
	u := &claimUpstream{previewStatus: 404, previewBody: `{"error_msg":"404 Route Not Found"}`}
	s, _ := newClaimTestScheduler(t, u, []*Cred{oneCred("u1")})

	wait := s.tickOnce(context.Background())
	if wait != ClaimPollInterval {
		t.Fatalf("preview 404 应走正常轮询（%v），实际 %v", ClaimPollInterval, wait)
	}
	if got := s.LastResult().Outcome; got != ClaimOutcomeIdle {
		t.Errorf("结果应为 idle，实际 %q", got)
	}
}

// TestEmptyPlansPollsNormally 没有可领套餐 → 正常轮询。
func TestEmptyPlansPollsNormally(t *testing.T) {
	u := &claimUpstream{} // 默认就是空 plans
	s, _ := newClaimTestScheduler(t, u, []*Cred{oneCred("u1")})

	wait := s.tickOnce(context.Background())
	if wait != ClaimPollInterval {
		t.Fatalf("无可领套餐应正常轮询，实际 %v", wait)
	}
	if got := s.LastResult().Outcome; got != ClaimOutcomeIdle {
		t.Errorf("结果应为 idle，实际 %q", got)
	}
	if n := atomic.LoadInt32(&u.claimHits); n != 0 {
		t.Errorf("无可领套餐时**不该**调 claim，实际 %d 次", n)
	}
}

// TestOtherFailureCooldowns 其它失败 → cooldown（不是正常轮询，也不是停）。
func TestOtherFailureCooldowns(t *testing.T) {
	u := &claimUpstream{
		previewBody: planJSON("plan-x", time.Now().Add(time.Hour).Unix(), 90),
		claimStatus: 200,
		claimBody:   `{"code":3007,"msg":"captcha required"}`,
	}
	s, _ := newClaimTestScheduler(t, u, []*Cred{oneCred("u1")})

	wait := s.tickOnce(context.Background())
	if wait != ClaimCooldown {
		t.Fatalf("captcha 失败应冷却 %v，实际 %v", ClaimCooldown, wait)
	}
	if k := s.LastResult().Kind; k != ClaimFailCaptcha {
		t.Errorf("种类应为 captcha，实际 %q", k)
	}
}

// TestLoginRequiredStopsPermanently login_required → **永久停止**（不重试）。
//
// 参考实现：`login_required` → `stop()`。理由：凭证失效重试一万次也不会好，
// 只会把上游的失败计数打上去。
func TestLoginRequiredStopsPermanently(t *testing.T) {
	u := &claimUpstream{
		previewStatus: 401,
		previewBody:   `{"code":401,"msg":"unauthorized"}`,
	}
	s, _ := newClaimTestScheduler(t, u, []*Cred{oneCred("u1")})

	s.tickOnce(context.Background())
	if !s.Stopped() {
		t.Fatal("凭证失效（401）后应**永久停止**自动领取（参考实现的 stop()）")
	}
	hits := atomic.LoadInt32(&u.previewHits)

	// 再跑若干轮：不该再发任何请求
	for i := 0; i < 3; i++ {
		s.tickOnce(context.Background())
	}
	if got := atomic.LoadInt32(&u.previewHits); got != hits {
		t.Errorf("永久停止后**不该**再发 preview（停止前 %d，现在 %d）", hits, got)
	}
	// ResetStop 后恢复（用户重新登录）
	s.ResetStop()
	if s.Stopped() {
		t.Error("ResetStop 后应恢复运行")
	}
}

// TestDisabledPollsButDoesNotClaim 用户关掉开关时不领取，但仍按间隔检查。
//
// 为什么"仍检查"：这样在界面上重新打开能立刻生效，不必重启网关。
func TestDisabledPollsButDoesNotClaim(t *testing.T) {
	u := &claimUpstream{previewBody: planJSON("plan-x", time.Now().Add(time.Hour).Unix(), 90)}
	srv := httptest.NewServer(u.handler())
	defer srv.Close()
	client := New()
	client.HTTP = srv.Client()
	client.SetClaimBaseForTest(srv.URL)

	s := NewClaimScheduler(client,
		func() ([]*Cred, []string, error) { return []*Cred{oneCred("u1")}, nil, nil },
		func() bool { return false }, // 关掉
	)
	wait := s.tickOnce(context.Background())
	if wait != ClaimPollInterval {
		t.Errorf("关闭时仍应按正常间隔检查（便于即时生效），实际 %v", wait)
	}
	if n := atomic.LoadInt32(&u.previewHits); n != 0 {
		t.Errorf("关闭时**不该**发任何请求，实际 preview %d 次", n)
	}
}

// TestPickOfferSkipsUnclaimable 挑选时跳过空的与已结束的。
func TestPickOfferSkipsUnclaimable(t *testing.T) {
	if got := pickOffer(nil); got != nil {
		t.Errorf("空列表应返回 nil，实际 %+v", got)
	}
	ended := []PlanOffer{
		{PlanID: "a", Status: "expired"},
		{PlanID: "b", Status: "ENDED"},
	}
	if got := pickOffer(ended); got != nil {
		t.Errorf("全是已结束的应返回 nil，实际 %+v", got)
	}
	blank := []PlanOffer{
		{PlanID: "   ", Status: "claimable"},
		{PlanID: "real", Status: "claimable"},
	}
	if got := pickOffer(blank); got == nil || got.PlanID != "real" {
		t.Errorf("空 planId 应跳过、取后面的 real，实际 %+v", got)
	}
}

// TestEndsAtTimeZeroWhenMissing 上游没给截止时返回零值（调用方据此不 hold）。
func TestEndsAtTimeZeroWhenMissing(t *testing.T) {
	if got := (&PlanOffer{PlanID: "x"}).endsAtTime(); !got.IsZero() {
		t.Errorf("EndsAt=0 时应返回零值（不 hold），实际 %v", got)
	}
	if got := (*PlanOffer)(nil).endsAtTime(); !got.IsZero() {
		t.Error("nil 应安全返回零值")
	}
	want := time.Unix(1700000000, 0)
	if got := (&PlanOffer{EndsAt: 1700000000}).endsAtTime(); !got.Equal(want) {
		t.Errorf("应返回 %v，实际 %v", want, got)
	}
}

// TestSchedulerParamsMatchReference 调度参数与参考实现一致。
//
// ⚠ 这条防"被优化"：参考实现是 5 分钟轮询 / 10 分钟冷却，
// 有人若"为了省资源"改成 1 小时，抢限量名额的能力就没了
//（而所有者要的正是参考实现的行为）。
func TestSchedulerParamsMatchReference(t *testing.T) {
	if ClaimPollInterval != 5*time.Minute {
		t.Errorf("轮询间隔应为 5 分钟（参考实现 pollIntervalMs=300000），实际 %v", ClaimPollInterval)
	}
	if ClaimCooldown != 10*time.Minute {
		t.Errorf("失败冷却应为 10 分钟（参考实现 cooldownMs=600000），实际 %v", ClaimCooldown)
	}
	if ClaimHoldCap != 24*time.Hour {
		t.Errorf("hold 上限应为 24h（参考实现），实际 %v", ClaimHoldCap)
	}
}

// TestClaimOnceSharesLogicWithScheduler 手动路径与自动路径**共用同一套逻辑**。
//
// 各写一份必然出现"手动能领、自动领不到"这类分歧，而那种缺陷极难查
//（两条路径看起来都对，只是行为不同）。这条断言两者对同一上游给出同样的 outcome。
func TestClaimOnceSharesLogicWithScheduler(t *testing.T) {
	ends := time.Now().Add(time.Hour).Unix()
	u := &claimUpstream{previewBody: planJSON("plan-x", ends, 90)}
	s, _ := newClaimTestScheduler(t, u, []*Cred{oneCred("u1")})

	// 通过 Dispatch 的手动入口跑一次
	d := NewDispatch(s.client)
	d.SetAuthDir(t.TempDir())
	res := d.ClaimOnce(context.Background(), oneCred("u2"), s)
	if res.Outcome != ClaimOutcomeClaimed {
		t.Errorf("手动路径应同样能领到（结果 claimed），实际 %q（msg=%s）", res.Outcome, res.Msg)
	}
}


