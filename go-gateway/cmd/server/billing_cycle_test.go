package main

// billing_cycle_test.go 计费周期刷新（cmd/server 这一层）的回归。
//
// # 为什么这些测试必须存在
//
// 本任务之前，`internal/usage` 的 `SetBillingCycle` / `CycleOf` 是**死代码**：
// 逻辑写好了、单测也有，但没有任何生产路径调用它们。死代码的特征是
// **删掉它不会有任何东西变红** —— 所以"接上线"这件事本身必须由测试锁住，
// 否则下一次重构会静默把这条线剪断，而症状（界面永远显示 0）不会报错。
//
// 故这里测的不是 `internal/usage` 的周期算法（那在 cycle_test.go 里，
// 与本层无关），而是**本层的四个职责**：
//
//	① 产品闸门：只查 Qoder，不碰 WorkBuddy / ZCode
//	② 错误隔离：一个账号失败不影响其余账号，且失败账号**不写**周期
//	③ 不可信值不清零：0 / 哨兵 / 负数一律保留上一周期
//	④ 排程接线：`RunCreditRefreshLoop` 真的会调到执行体，且回调为 nil 不 panic
//
// # 关于测试用的周期边界为什么取 2030/2031
//
// `internal/usage/cycle.go` 的**读路径**（`CycleOf` → `projectCycleLocked`）
// 会按"套餐按月重置"的契约做 catch-up，把已经过去的边界一路推进到"现在之后"。
// 若测试里用一个过去的边界，读出来的就是**投影值**而不是写入值，断言会
// 看起来"莫名其妙对不上"。取远在未来的边界，投影就是恒等映射，
// 断言才能直接比精确值（catch-up 本身由 cycle_test.go 覆盖）。
//
// 上游原始边界是过去值这件事是**真实存在的**（实测国服账号的 nextResetAt
// 已过期 78 天），那种情况下的投影行为由隔离实例实测覆盖，不在单测里模拟。
import (
	"context"
	"errors"
	"testing"
	"time"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/pool"
	"workbuddy2api/internal/qoder"
	"workbuddy2api/internal/scheduler"
	"workbuddy2api/internal/usage"
)

const (
	// testCycleA 2030-01-01T00:00:00Z。
	testCycleA = int64(1893456000000)
	// testCycleB 2031-01-01T00:00:00Z（比 A 晚一个周期，用于测"新周期被采纳"）。
	testCycleB = int64(1924992000000)
	// dayMs 一天。
	dayMs = int64(24 * 60 * 60 * 1000)
)

// newBillingCycleTestPool 造一个不落盘的池：uid → 产品。
//
// `pool.New("")` = 纯内存（stateFp 为空 ⇒ 不 load 不 save），
// 与 internal/pool 的既有测试同口径。
func newBillingCycleTestPool(products map[string]string) *pool.Pool {
	p := pool.New("")
	for uid, product := range products {
		p.Add(&auth.Auth{
			UID:          uid,
			Product:      product,
			AccessToken:  "dt-" + uid,
			RefreshToken: "drt-" + uid,
			Domain:       "qoder.com.cn",
		})
	}
	return p
}

// TestBillingCycleRefreshOnlyTouchesQoder 要求 5①：产品闸门。
//
// 池里同时放 WorkBuddy / Qoder / ZCode，断言**只有 Qoder** 被查询。
//
// ⚠ 这条测的是一个具体的写错方式：把闸门写成
// `a.ProductOf() != auth.ProductWorkBuddy`。那个判据读起来"更宽"，实际会把
// ZCode 账号一起放进来 —— 然后拿 ZCode 的凭证去打 Qoder 的端点。
// 这种错**不会报错**，只会让 Qoder 的周期数据被无意义的上游调用冲淡，
// 或者被 ZCode 返回的东西污染。故必须由测试锁住。
func TestBillingCycleRefreshOnlyTouchesQoder(t *testing.T) {
	p := newBillingCycleTestPool(map[string]string{
		"wb-1":    "", // 空串 = WorkBuddy
		"qoder-1": auth.ProductQoder,
		"zcode-1": auth.ProductZcode,
	})
	store := usage.New("")

	var seen []string
	fetch := func(_ context.Context, cr *qoder.Cred) (int64, error) {
		seen = append(seen, cr.UID)
		return testCycleA, nil
	}

	processed, updated := refreshBillingCyclesOnce(context.Background(), p, fetch, store)

	if processed != 1 {
		t.Errorf("应只处理 1 个账号（Qoder），实际 %d", processed)
	}
	if len(seen) != 1 || seen[0] != "qoder-1" {
		t.Errorf("只应查询 [qoder-1]，实际 %v ——\n"+
			"闸门必须用 a.IsQoder()（auth.go:77）。\n"+
			"写成 ProductOf() != ProductWorkBuddy 会把 ZCode 一起放进来。", seen)
	}
	if updated != 1 {
		t.Errorf("Qoder 账号的周期应被写入，实际 updated=%d", updated)
	}
	for _, uid := range []string{"wb-1", "zcode-1"} {
		if _, ok := store.CycleOf(uid); ok {
			t.Errorf("%s 不是 Qoder 账号，不该有计费周期", uid)
		}
	}
	pj, ok := store.CycleOf("qoder-1")
	if !ok || pj.NextResetMs != testCycleA {
		t.Errorf("qoder-1 的周期应为 %d，实际 ok=%v pj=%+v", testCycleA, ok, pj)
	}
}

// TestBillingCycleRefreshIsolatesFailures 要求 5②：单账号失败只影响它自己。
//
// 两个 Qoder 账号，第一个查询返错：第二个仍要被处理，而第一个**不写**周期。
// 「不写」比「写个 0」重要得多 —— 写 0 会被 `setBillingCycleAt` 的 ⓪ 情形
// 拒绝（保留上一周期），但那是**下游兜底**；本层不应把失败伪装成"上游报了 0"。
func TestBillingCycleRefreshIsolatesFailures(t *testing.T) {
	p := newBillingCycleTestPool(map[string]string{
		"qoder-1": auth.ProductQoder,
		"qoder-2": auth.ProductQoder,
	})
	store := usage.New("")
	boom := errors.New("上游 500")

	fetch := func(_ context.Context, cr *qoder.Cred) (int64, error) {
		if cr.UID == "qoder-1" {
			return 0, boom
		}
		return testCycleA, nil
	}

	processed, updated := refreshBillingCyclesOnce(context.Background(), p, fetch, store)

	if processed != 2 {
		t.Errorf("两个账号都应被尝试，实际 processed=%d", processed)
	}
	if updated != 1 {
		t.Errorf("只有一个账号有可信值，updated 应为 1，实际 %d", updated)
	}
	if _, ok := store.CycleOf("qoder-1"); ok {
		t.Error("查询失败的账号不该被写入周期状态（宁可没有数据，也不要一个假边界）")
	}
	if pj, ok := store.CycleOf("qoder-2"); !ok || pj.NextResetMs != testCycleA {
		t.Errorf("失败的账号不该影响其余账号：qoder-2 ok=%v pj=%+v", ok, pj)
	}
}

// TestBillingCycleRefreshKeepsCycleOnUnusableValue 要求 5③：不可信值不清零。
//
// 先写入一个可信周期，再让上游返回 0 / 哨兵 / 负数 —— 盘上的周期必须原样保留。
//
// 三种值各有来源，不是凑数：
//   - 0：上游没报 nextResetAt（字段缺失），`ResolveNextResetMs` 返回 0
//   - 253402214400000：9999-12-31，无到期套餐常见的"永不过期"哨兵
//   - 负数：上游给了个畸形值（或时区/单位算错）
//
// 若这里不清零的保证失效，用户会看到"本周期用量"在每轮巡检后莫名归零。
func TestBillingCycleRefreshKeepsCycleOnUnusableValue(t *testing.T) {
	p := newBillingCycleTestPool(map[string]string{"qoder-1": auth.ProductQoder})
	store := usage.New("")

	// 显式给"现在"，与真实时钟解耦（nowMs=1000 时 testCycleA 远在未来）。
	if !store.SetBillingCycleAt("qoder-1", testCycleA, 1000) {
		t.Fatal("前置条件失败：可信周期没写进去")
	}

	cases := []struct {
		name string
		next int64
	}{
		{"上游没报周期（0）", 0},
		{"上游报 9999-12-31 哨兵", 253402214400000},
		{"上游报负数", -1},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			fetch := func(_ context.Context, _ *qoder.Cred) (int64, error) {
				return c.next, nil
			}
			_, updated := refreshBillingCyclesOnce(context.Background(), p, fetch, store)
			if updated != 0 {
				t.Errorf("不可信值不该被计入「更新」，实际 updated=%d", updated)
			}
			pj, ok := store.CycleOf("qoder-1")
			if !ok {
				t.Fatal("不可信值不该让已跟踪的周期消失")
			}
			if pj.NextResetMs != testCycleA {
				t.Errorf("周期被不可信值覆盖了：want %d, got %d", testCycleA, pj.NextResetMs)
			}
		})
	}
}

// TestBillingCycleRefreshAdoptsNewerCycle 要求 5④：新周期被采纳 / 旧报告被忽略。
//
// ① 更晚的可信值 ⇒ `NextResetMs` 更新，且 `CycleStartMs` 按"月度套餐"**重推**
// ② 更旧的可信值 ⇒ 保留较新的那个（陈旧报告，例如上游缓存了旧响应）
//
// ② 的判据（`setBillingCycleAt` 情形 ②）是防止"周期边界被往回拨"：
// 一旦回拨，"本周期用量"会把上一周期的整月用量算进来。
func TestBillingCycleRefreshAdoptsNewerCycle(t *testing.T) {
	p := newBillingCycleTestPool(map[string]string{"qoder-1": auth.ProductQoder})
	store := usage.New("")
	if !store.SetBillingCycleAt("qoder-1", testCycleA, 1000) {
		t.Fatal("前置条件失败：可信周期没写进去")
	}

	// ① 更晚的可信值 ⇒ 采纳
	fetchNew := func(_ context.Context, _ *qoder.Cred) (int64, error) { return testCycleB, nil }
	if _, updated := refreshBillingCyclesOnce(context.Background(), p, fetchNew, store); updated != 1 {
		t.Errorf("更晚的可信周期应被采纳，实际 updated=%d", updated)
	}
	pj, ok := store.CycleOf("qoder-1")
	if !ok || pj.NextResetMs != testCycleB {
		t.Fatalf("边界未更新：ok=%v pj=%+v（want %d）", ok, pj, testCycleB)
	}
	// 周期起点必须**重推**（不是沿用旧起点的残留值）：应落在新边界之前约一个月。
	span := pj.NextResetMs - pj.CycleStartMs
	if pj.CycleStartMs <= 0 || span < 28*dayMs || span > 32*dayMs {
		t.Errorf("周期起点应重推为「新边界前一个月」：start=%d next=%d span=%d 天",
			pj.CycleStartMs, pj.NextResetMs, span/dayMs)
	}

	// ② 更旧的可信值 ⇒ 保留较新的（陈旧报告）
	fetchStale := func(_ context.Context, _ *qoder.Cred) (int64, error) { return testCycleA, nil }
	if _, updated := refreshBillingCyclesOnce(context.Background(), p, fetchStale, store); updated != 0 {
		t.Errorf("陈旧报告不该被采纳，实际 updated=%d", updated)
	}
	pj2, _ := store.CycleOf("qoder-1")
	if pj2.NextResetMs != testCycleB {
		t.Errorf("陈旧报告把边界往回拨了：want %d, got %d", testCycleB, pj2.NextResetMs)
	}
}

// TestBillingCycleRefreshStopsOnContextCancel 要求 5⑤：ctx 取消提前返回。
//
// ① 一进循环就已取消 ⇒ 一个账号都不查（网关停机时不该还在打上游）
// ② 第一个账号查完后取消 ⇒ 立刻返回，后面的账号不再查
func TestBillingCycleRefreshStopsOnContextCancel(t *testing.T) {
	p := newBillingCycleTestPool(map[string]string{
		"qoder-1": auth.ProductQoder,
		"qoder-2": auth.ProductQoder,
		"qoder-3": auth.ProductQoder,
	})
	store := usage.New("")

	// ① 已取消
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	calls := 0
	fetch := func(_ context.Context, _ *qoder.Cred) (int64, error) {
		calls++
		return testCycleA, nil
	}
	if processed, _ := refreshBillingCyclesOnce(ctx, p, fetch, store); processed != 0 {
		t.Errorf("ctx 已取消时不该处理任何账号，实际 processed=%d", processed)
	}
	if calls != 0 {
		t.Errorf("ctx 已取消时不该查询上游，实际调用 %d 次", calls)
	}

	// ② 处理第一个账号后取消
	ctx2, cancel2 := context.WithCancel(context.Background())
	calls = 0
	fetch2 := func(_ context.Context, _ *qoder.Cred) (int64, error) {
		calls++
		cancel2()
		return testCycleA, nil
	}
	processed, _ := refreshBillingCyclesOnce(ctx2, p, fetch2, store)
	if processed != 1 {
		t.Errorf("取消后应立即返回，已处理 1 个，实际 processed=%d", processed)
	}
	if calls != 1 {
		t.Errorf("取消后不该继续查询：实际调用 %d 次", calls)
	}
}

// TestNewBillingCycleRefreshRunnerNilWhenMissingDeps 依赖不全时返回 nil。
//
// 与 `newProductTasksRunner` 同口径：返回 nil = 排程静默跳过，**不是错误**。
// 单产品部署（没有 Qoder 账号）是合法配置，不该在巡检里刷错误日志。
func TestNewBillingCycleRefreshRunnerNilWhenMissingDeps(t *testing.T) {
	p := pool.New("")
	store := usage.New("")
	fetch := func(_ context.Context, _ *qoder.Cred) (int64, error) { return 0, nil }

	if fn := newBillingCycleRefreshRunner(nil, fetch, store); fn != nil {
		t.Error("没有账号池时不该给出执行体")
	}
	if fn := newBillingCycleRefreshRunner(p, nil, store); fn != nil {
		t.Error("没有取数函数时不该给出执行体")
	}
	if fn := newBillingCycleRefreshRunner(p, fetch, nil); fn != nil {
		t.Error("没有统计器时不该给出执行体")
	}
	if fn := newBillingCycleRefreshRunner(p, fetch, store); fn == nil {
		t.Error("三个依赖齐全时必须给出执行体")
	}
}

// TestQoderBillingCycleFetcherNilClient 没有 client 时取数函数也是 nil。
//
// 这保证「多产品路由关闭 ⇒ qoderClient 为 nil」这条路径不会在巡检里
// 变成一次 nil 解引用。
func TestQoderBillingCycleFetcherNilClient(t *testing.T) {
	if f := qoderBillingCycleFetcher(nil); f != nil {
		t.Error("没有 client 时不该给出取数函数（否则执行体会拿着 nil 去打上游）")
	}
}

// TestBillingCycleCredOfAlignsUIDToPoolKey 凭证重建时 uid 必须对齐到池键。
//
// 这是本层最容易写错、且**错了不报错**的一处：`SetBillingCycle` 的 uid 与
// `usage.RecordBilled` 的 uid 必须同源，否则周期与用量永远 join 不上
// （界面显示 0，没有任何错误信息）。见 billing_cycle.go 文件头的证据链。
//
// 同时验证从池重建凭证时的区域判定与字段映射（`auth.Auth` 没有机器指纹字段，
// 故这里只保证令牌与区域不丢）。
func TestBillingCycleCredOfAlignsUIDToPoolKey(t *testing.T) {
	a := &auth.Auth{
		UID:         "qoder-pool-key",
		Product:     auth.ProductQoder,
		AccessToken: "dt-value",
		Domain:      "qoder.com.cn",
	}
	cr := billingCycleCredOf(a)
	if cr == nil {
		t.Fatal("有 uid 的账号应能重建凭证")
	}
	if cr.UID != a.UID {
		t.Errorf("凭证 uid 必须对齐池键：want %q, got %q", a.UID, cr.UID)
	}
	if cr.DT != "dt-value" {
		t.Errorf("accessToken 应被带上：got %q", cr.DT)
	}
	if cr.Region != qoder.RegionCN {
		t.Errorf("qoder.com.cn 应判为国服：got %q", cr.Region)
	}

	if cr := billingCycleCredOf(nil); cr != nil {
		t.Error("nil 账号应返回 nil")
	}
	if cr := billingCycleCredOf(&auth.Auth{Product: auth.ProductQoder}); cr != nil {
		t.Error("没有 uid 的账号无法查询（uid 是 SetBillingCycle 的键），应返回 nil")
	}
}

// TestRunCreditRefreshLoopToleratesNilBillingCycleRunner 要求 5⑥：回调 nil 不 panic。
//
// 走真实的 `RunCreditRefreshLoop` 入口 —— 这正是生产路径的形状。
// 单产品部署下 `SetBillingCycleRefreshRunner` 可能从未被调用（回调为 nil），
// 此时巡检必须静默跳过，而不是 panic 把网关带走。
func TestRunCreditRefreshLoopToleratesNilBillingCycleRunner(t *testing.T) {
	sch := scheduler.New(scheduler.Config{}) // Pool 为 nil：没有账号可跑，合法
	sch.SetBillingCycleRefreshRunner(nil)

	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	done := make(chan struct{})
	go func() {
		defer close(done)
		sch.RunCreditRefreshLoop(ctx, time.Hour)
	}()
	select {
	case <-done:
	case <-time.After(10 * time.Second):
		t.Fatal("ctx 已取消时该循环应立即返回（回调为 nil 时卡住 = 网关停不下来）")
	}
}

// TestRunCreditRefreshLoopInvokesBillingCycleRunner 端到端：排程真的会调到执行体。
//
// 这是"接线生效"的**唯一直接判据**：前面那些测试都直接调
// `refreshBillingCyclesOnce`，即使 `RunCreditRefreshLoop` 里忘了调用回调，
// 它们也全绿 —— 而那正是本任务要修的缺陷形态（代码在，但没被调用）。
//
// 故这里从 `scheduler.New` + `SetBillingCycleRefreshRunner` + `RunCreditRefreshLoop`
// 一路走到底，只把**上游查询**换成桩。
func TestRunCreditRefreshLoopInvokesBillingCycleRunner(t *testing.T) {
	p := newBillingCycleTestPool(map[string]string{"qoder-1": auth.ProductQoder})
	store := usage.New("")
	fetch := func(_ context.Context, _ *qoder.Cred) (int64, error) { return testCycleA, nil }

	sch := scheduler.New(scheduler.Config{Pool: p})
	sch.SetBillingCycleRefreshRunner(newBillingCycleRefreshRunner(p, fetch, store))

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	go sch.RunCreditRefreshLoop(ctx, time.Hour)

	deadline := time.Now().Add(10 * time.Second)
	for time.Now().Before(deadline) {
		if pj, ok := store.CycleOf("qoder-1"); ok && pj.NextResetMs == testCycleA {
			cancel()
			return
		}
		time.Sleep(20 * time.Millisecond)
	}
	cancel()
	t.Fatal("RunCreditRefreshLoop 的启动轮没有把周期写进统计器 —— 接线没生效")
}
