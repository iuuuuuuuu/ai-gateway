package scheduler

// manual_trigger_record_test.go 「手动触发任务必须有账号级记录」的回归测试。
//
// # 守的是所有者实测反馈（2026-09-20）
//
// 原话：
//
//	「还有workbuddy的夜猫子任务也不知道到底执行了没，任务记录里面也没有
//	  开学季任务也是，我手动执行了但是查看记录却没有」
//
// 我查了他的真实记录文件（1181 条）证实了这一点：
//
//	task/夜猫子任务    56 条  [success]
//	task/活跃上报      56 条  [success]
//	task/自动签到     169 条  [success,already]
//	task/开学季活动    **0 条**            ← 完全没有
//	accountId 为空的记录: **0 条**
//
// # 根因（读 school.go / nightowl.go 的控制流得出）
//
// 成功的路径里**两条不写账号级记录**：
//
//	① 活动不在期   → `continue` 静默跳过（只在"所有账号都不在期"时
//	                 末尾写一条 TaskAllDaily 汇总，而那**不带 accountId**）
//	② 在期但无奖励 → `claimed == 0` 时**完全不写**
//
// 而用户是在**某个账号的菜单里**点的"开学季"—— 他期望在那张卡片的记录里
// 看到"我跑了、结果是什么"。汇总记录没有 accountId，按账号筛选时看不到。
//
// 夜猫子看起来"有记录"只是因为它在窗口内必然上报（故必然写 success）；
// 它在**窗口外**同样只写汇总（nightowl.go:69），故窗口外手动点同样看不到，
// 那正是所有者说"不知道到底执行了没"的原因之一。
//
// # 判据：手动触发要有反馈，自动排程可以按"有新变化才写"
//
// 区别不是主观偏好，而是**有没有人在看**：
//
//	· 手动触发 = 用户盯着结果 ⇒ 无论成败都必须留一条账号级记录
//	· 自动排程 = 没人看 ⇒ 只在"有新变化"时写，避免把记录刷成噪音
//
// 幸运的是代码里**已经有这个信息**：`RunTaskFor(name, accountUID)` 的
// accountUID 非空即手动。故不需要新管道，只要在出口处补记录。
import (
	"net/http/httptest"
	"testing"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/pool"
	"workbuddy2api/internal/upstream"
)

// newManualTestScheduler 造一个"单国服账号 + 记录写入器"的调度器。
//
// 复用既有的 `newRecordedScheduler`（它已装好 uid→宿主 id 的身份映射，
// 并关掉了其它任务）。这里只补上游与账号。
//
// ⚠ 身份映射刻意是「uid ≠ 宿主 id」（`u1` → `host-id-1`）：
// 界面按**宿主 id** 过滤记录，若把 uid 写进 accountId，用户永远查不到且不报错。
func newManualTestScheduler(t *testing.T, rec *schoolRecorder) (*Scheduler, string) {
	t.Helper()
	srv := httptest.NewServer(rec.handler())
	t.Cleanup(srv.Close)
	p := pool.New("")
	p.Add(&auth.Auth{UID: "u1", AccessToken: "tok", Product: auth.ProductWorkBuddy})
	return newRecordedScheduler(t, Config{
		Pool:     p,
		Upstream: &upstream.Client{HTTP: srv.Client(), ChatBaseCN: srv.URL, BillingBaseCN: srv.URL, BaseIntl: srv.URL},
	})
}

// TestManualSchoolWithNoRewardsStillRecords 手动跑开学季、但**没有可领奖励**时，
// 必须留下账号级记录。
//
// 这正是所有者遇到的场景：他手动执行了，记录里却什么都没有。
func TestManualSchoolWithNoRewardsStillRecords(t *testing.T) {
	rec := &schoolRecorder{inPeriod: true, tasks: []map[string]any{
		// 只有 pending（未达标）与 claimed（已领），**没有 finished** ⇒ claimed == 0
		task("t1", "pending", 10),
		task("t2", "claimed", 10),
	}}
	s, path := newManualTestScheduler(t, rec)

	// 手动触发（accountUID 非空 = 用户在某个账号的菜单里点的）
	if _, err := s.RunTaskFor(TaskNameSchool, "u1"); err != nil {
		t.Fatalf("RunTaskFor 不应报错: %v", err)
	}

	got := findByTitle(recordsOf(t, path), "开学季活动")
	if len(got) == 0 {
		t.Fatalf("手动触发开学季、虽无可领奖励，也必须留一条**账号级**记录；"+
			"实际 0 条 —— 所有者看到的正是这个现象（手动执行了却查不到）")
	}
	if got[0]["accountId"] != "host-id-1" {
		t.Errorf("记录必须挂在**宿主 id** 上（界面按它过滤），实际 %v", got[0]["accountId"])
	}
	t.Logf("✓ 记录: result=%v detail=%v", got[0]["result"], got[0]["detail"])
}

// TestManualSchoolOutsidePeriodRecordsPerAccount 手动跑开学季、**活动不在期**时，
// 也要留账号级记录。
//
// 此前只在"所有账号都不在期"时写一条**汇总**（不带 accountId），
// 故用户在单账号卡片里看不到。
func TestManualSchoolOutsidePeriodRecordsPerAccount(t *testing.T) {
	rec := &schoolRecorder{inPeriod: false}
	s, path := newManualTestScheduler(t, rec)

	if _, err := s.RunTaskFor(TaskNameSchool, "u1"); err != nil {
		t.Fatalf("RunTaskFor 不应报错: %v", err)
	}

	got := findByTitle(recordsOf(t, path), "开学季活动")
	if len(got) == 0 {
		t.Fatalf("手动触发开学季、活动不在期时也要留一条记录（汇总记录没有 " +
			"accountId，用户在账号卡片里看不到）")
	}
	t.Logf("✓ 记录: result=%v detail=%v", got[0]["result"], got[0]["detail"])
}

// TestManualNightOwlOutsideWindowRecordsPerAccount 手动跑夜猫子、**不在时段**时，
// 同样要留账号级记录。
//
// 窗口外分支（nightowl.go:69）写的是 `TaskAllDaily` —— 整轮汇总、不带
// accountId。用户在卡片里点"夜猫子"再查该账号的记录，什么都看不到。
func TestManualNightOwlOutsideWindowRecordsPerAccount(t *testing.T) {
	if withinNightWindow() {
		t.Skip("当前恰在夜猫时段（23:00–08:00 CST），窗口外分支测不到")
	}
	rec := &schoolRecorder{inPeriod: true}
	s, path := newManualTestScheduler(t, rec)

	if _, err := s.RunTaskFor(TaskNameNightOwl, "u1"); err != nil {
		t.Fatalf("RunTaskFor 不应报错: %v", err)
	}

	got := findByTitle(recordsOf(t, path), "夜猫子任务")
	if len(got) == 0 {
		t.Fatalf("手动触发夜猫子、不在时段时也要留该账号的记录；" +
			"实际 0 条（所有者因此不知道到底执行了没）")
	}
	t.Logf("✓ 记录: result=%v detail=%v", got[0]["result"], got[0]["detail"])
}

// TestScheduledSchoolDoesNotSpamRecords 自动排程（无账号作用域）**不该**为
// "在期但无奖励"刷记录。
//
// 这条防"修过头"：原作者注释里的顾虑（写完会把记录刷成噪音）**对自动路径成立**，
// 故本次改动只该影响手动路径。
func TestScheduledSchoolDoesNotSpamRecords(t *testing.T) {
	rec := &schoolRecorder{inPeriod: true, tasks: []map[string]any{
		task("t1", "pending", 10),
	}}
	s, path := newManualTestScheduler(t, rec)

	// 排程路径：accountUID 为空
	if _, err := s.RunTaskFor(TaskNameSchool, ""); err != nil {
		t.Fatalf("RunTaskFor 不应报错: %v", err)
	}

	got := findByTitle(recordsOf(t, path), "开学季活动")
	if len(got) > 0 {
		t.Fatalf("自动排程下「在期但无奖励」不该写记录（会把记录刷成噪音）；"+
			"实际写了 %d 条: %v", len(got), got)
	}
}
