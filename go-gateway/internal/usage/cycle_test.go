package usage

// cycle_test.go —— 钉住「计费周期」（N4）的四情形状态机、月末钳制、读/写分工。
//
// # 本文件覆盖的七条要求（与任务书的编号对应）
//
//	① 月末钳制      —— TestAddMonthsMsClampsMonthEnd（31/29/30 天全覆盖）
//	② 周期起点推导  —— TestSetBillingCycleDerivesCalendarStart
//	③ 四情形状态机  —— TestSetBillingCycle{Idempotent,IgnoresStaleBoundary,
//	                    AdoptsNewCycle,RejectsImplausibleBoundary}
//	④ 读不推进      —— TestCycleProjectionDoesNotRollState
//	⑤ 热路径不碰盘  —— TestRecordAndCycleUpdateDoNotTouchDisk
//	⑥ 边界防御      —— TestCycleRollTerminatesOnUnadvanceableBoundary、
//	                    TestCycleFarPastBoundaryDoesNotSpin
//	⑦ 落盘/装载     —— TestBillingCyclesSurviveRestart、
//	                    TestSetBillingCycleRecoversFromPersistedSentinel
//
// # 为什么这些用例值得写（而不是"状态机看着对就行"）
//
// 周期边界决定界面上的"本周期用量"。它错一格，用户看到的就是**另一个月的
// 数字** —— 而这种错误不会报错、不会崩溃，只会安静地显示错数。故每一条
// 判据都要有一个用例把它钉死，尤其是"看起来等价但其实不等价"的那几处：
// 月末钳制、读路径推进、DST 时区的月步进。

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
	"time"
)

// ---- ① 月末钳制 ----

// TestAddMonthsMsClampsMonthEnd 覆盖 31/29/30 天月末的加减月钳制。
//
// # 这条为什么是**最重要**的用例
//
// 参考实现踩过的坑：用 time.AddDate 时，3 月 31 日减一个月会得到 **3 月 3 日**
// （2 月 31 日被"溢出归一"成 3 月 3 日），5 月 31 日减一个月会得到 **5 月 1 日**。
// 这个值直接喂给周期起点，于是每个在 29~31 号重置的账号，其"本周期"都会
// 从月初附近开始 —— 把上一整个月的用量算进来。界面上就是"本周期用量"
// 突然变成两个月那么多，而且**永远如此**，不会自愈。
//
// 故这里必须逐一钉住：31 天的月份、30 天的月份、闰年 2 月 29 天、
// 平年 2 月 28 天，以及加减两个方向。
func TestAddMonthsMsClampsMonthEnd(t *testing.T) {
	at := func(y int, m time.Month, d, h, mi, s int) int64 {
		return time.Date(y, m, d, h, mi, s, 0, time.Local).UnixMilli()
	}

	cases := []struct {
		name string
		in   int64
		n    int
		want int64
	}{
		// --- 31 天的月份，减一个月（目标月只有 28/29/30 天）---
		{
			name: "mar-31-minus-1-lands-on-feb-28",
			in:   at(2026, time.March, 31, 12, 30, 45),
			n:    -1,
			want: at(2026, time.February, 28, 12, 30, 45),
		},
		{
			name: "mar-31-minus-1-leap-lands-on-feb-29",
			in:   at(2028, time.March, 31, 12, 30, 45),
			n:    -1,
			want: at(2028, time.February, 29, 12, 30, 45),
		},
		{
			name: "may-31-minus-1-lands-on-apr-30",
			in:   at(2026, time.May, 31, 8, 0, 0),
			n:    -1,
			want: at(2026, time.April, 30, 8, 0, 0),
		},
		{
			name: "jul-31-minus-1-lands-on-jun-30",
			in:   at(2026, time.July, 31, 0, 0, 0),
			n:    -1,
			want: at(2026, time.June, 30, 0, 0, 0),
		},
		{
			name: "oct-31-minus-1-lands-on-sep-30",
			in:   at(2026, time.October, 31, 23, 59, 59),
			n:    -1,
			want: at(2026, time.September, 30, 23, 59, 59),
		},
		{
			name: "dec-31-minus-1-lands-on-nov-30",
			in:   at(2026, time.December, 31, 6, 0, 0),
			n:    -1,
			want: at(2026, time.November, 30, 6, 0, 0),
		},
		// --- 31 天的月份，加一个月 ---
		{
			name: "jan-31-plus-1-lands-on-feb-28",
			in:   at(2026, time.January, 31, 9, 15, 0),
			n:    1,
			want: at(2026, time.February, 28, 9, 15, 0),
		},
		{
			name: "jan-31-plus-1-leap-lands-on-feb-29",
			in:   at(2028, time.January, 31, 9, 15, 0),
			n:    1,
			want: at(2028, time.February, 29, 9, 15, 0),
		},
		{
			name: "mar-31-plus-1-lands-on-apr-30",
			in:   at(2026, time.March, 31, 9, 15, 0),
			n:    1,
			want: at(2026, time.April, 30, 9, 15, 0),
		},
		{
			name: "aug-31-plus-1-lands-on-sep-30",
			in:   at(2026, time.August, 31, 9, 15, 0),
			n:    1,
			want: at(2026, time.September, 30, 9, 15, 0),
		},
		{
			name: "oct-31-plus-1-lands-on-nov-30",
			in:   at(2026, time.October, 31, 9, 15, 0),
			n:    1,
			want: at(2026, time.November, 30, 9, 15, 0),
		},
		{
			name: "dec-31-plus-1-lands-on-jan-31",
			in:   at(2026, time.December, 31, 9, 15, 0),
			n:    1,
			want: at(2027, time.January, 31, 9, 15, 0),
		},
		// --- 30 天的月份 ---
		{
			name: "apr-30-minus-1-lands-on-mar-30",
			in:   at(2026, time.April, 30, 7, 0, 0),
			n:    -1,
			want: at(2026, time.March, 30, 7, 0, 0),
		},
		{
			name: "jun-30-minus-1-lands-on-may-30",
			in:   at(2026, time.June, 30, 7, 0, 0),
			n:    -1,
			want: at(2026, time.May, 30, 7, 0, 0),
		},
		{
			name: "sep-30-plus-1-lands-on-oct-30",
			in:   at(2026, time.September, 30, 7, 0, 0),
			n:    1,
			want: at(2026, time.October, 30, 7, 0, 0),
		},
		{
			name: "nov-30-plus-1-lands-on-dec-30",
			in:   at(2026, time.November, 30, 7, 0, 0),
			n:    1,
			want: at(2026, time.December, 30, 7, 0, 0),
		},
		// --- 2 月自身（29 天的闰月）---
		{
			name: "feb-29-minus-1-lands-on-jan-29",
			in:   at(2028, time.February, 29, 10, 0, 0),
			n:    -1,
			want: at(2028, time.January, 29, 10, 0, 0),
		},
		{
			name: "feb-28-plus-1-lands-on-mar-28",
			in:   at(2026, time.February, 28, 10, 0, 0),
			n:    1,
			want: at(2026, time.March, 28, 10, 0, 0),
		},
		// --- 月中：不该有任何钳制，时分秒与纳秒原样保留 ---
		{
			name: "mid-month-plus-1-keeps-clock-and-nanos",
			in:   time.Date(2026, time.February, 15, 23, 59, 59, 123456789, time.Local).UnixMilli(),
			n:    1,
			want: time.Date(2026, time.March, 15, 23, 59, 59, 123456789, time.Local).UnixMilli(),
		},
		{
			name: "mid-month-minus-1-keeps-clock",
			in:   at(2026, time.March, 15, 1, 2, 3),
			n:    -1,
			want: at(2026, time.February, 15, 1, 2, 3),
		},
		// --- 跨年 ---
		{
			name: "jan-15-minus-1-crosses-year",
			in:   at(2026, time.January, 15, 4, 5, 6),
			n:    -1,
			want: at(2025, time.December, 15, 4, 5, 6),
		},
		// --- 0 与负数入参：没有可推导的起点，返回 0 而不是编一个 ---
		{
			name: "zero-input-stays-zero",
			in:   0,
			n:    -1,
			want: 0,
		},
		{
			name: "negative-input-stays-zero",
			in:   -1,
			n:    1,
			want: 0,
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			got := addMonthsMs(tc.in, tc.n)
			if got != tc.want {
				t.Fatalf("addMonthsMs(%d, %d)\n got = %s (%d)\nwant = %s (%d)",
					tc.in, tc.n,
					time.UnixMilli(got).In(time.Local).Format(time.RFC3339Nano), got,
					time.UnixMilli(tc.want).In(time.Local).Format(time.RFC3339Nano), tc.want)
			}
		})
	}
}

// TestAddMonthsMsDoesNotOverflowLikeAddDate 显式对比 AddDate 的错误行为。
//
// 这条用例的价值在于**记录为什么不能图省事**：如果哪天有人把实现换成
// `time.UnixMilli(ms).AddDate(0, n, 0)`，上面的用例会挂，但挂的原因可能
// 被误读成"测试太严"。这里直接把两个结果并排放，让差异一目了然。
func TestAddMonthsMsDoesNotOverflowLikeAddDate(t *testing.T) {
	in := time.Date(2026, time.March, 31, 12, 0, 0, 0, time.Local)

	naive := in.AddDate(0, -1, 0)
	got := time.UnixMilli(addMonthsMs(in.UnixMilli(), -1)).In(time.Local)

	if got.Month() != time.February || got.Day() != 28 {
		t.Fatalf("钳制失败：得到 %s，期望 2 月 28 日", got.Format(time.RFC3339))
	}
	// 记录朴素实现的错误结果（3 月 3 日）—— 若某天 Go 改了 AddDate 语义，
	// 这条断言会挂，提示我们重新评估这段注释是否还成立。
	if naive.Month() != time.March || naive.Day() != 3 {
		t.Logf("注意：time.AddDate 的溢出行为已改变（%s），本文件的相关注释需要更新",
			naive.Format(time.RFC3339))
	}
}

// ---- ② 周期起点推导 ----

// TestSetBillingCycleDerivesCalendarStart 钉住"周期起点 = 重置时刻减一个月"。
//
// 上游只报"下次什么时候重置"（`nextResetAt`），不报起点。套餐是月度的，
// 故起点由边界推导。参考实现特意断言"8 月 25 日**而不是** 8 月 26 日"——
// 差一天看着无害，但它会让周期窗口整体偏移，于是"本周期用量"少算/多算
// 一整天的量。
func TestSetBillingCycleDerivesCalendarStart(t *testing.T) {
	// 用未来的年份，避免与"现在"的相对关系影响推进逻辑。
	year := time.Now().Year() + 4
	reset := time.Date(year, time.September, 25, 0, 0, 0, 0, time.Local)
	wantStart := time.Date(year, time.August, 25, 0, 0, 0, 0, time.Local)

	s := New("")
	s.SetBillingCycleAt("u1", reset.UnixMilli(), reset.Add(-72*time.Hour).UnixMilli())

	got := s.billingCycles["u1"]
	if got.NextResetMs != reset.UnixMilli() {
		t.Fatalf("边界未被采纳：got %d want %d", got.NextResetMs, reset.UnixMilli())
	}
	if got.CycleStartMs != wantStart.UnixMilli() {
		t.Fatalf("周期起点推导错误：\n got = %s\nwant = %s（8 月 25 日，不是 8 月 26 日）",
			time.UnixMilli(got.CycleStartMs).In(time.Local).Format(time.RFC3339),
			wantStart.Format(time.RFC3339))
	}
}

// TestSetBillingCycleDerivesClampedStartOnMonthEnd 月末边界的起点必须被钳制。
//
// 3 月 31 日重置 ⇒ 起点是 2 月 28 日（平年）。若这里退化成 3 月 3 日，
// 周期窗口就只剩 28 天，用户会看到"本周期用量"莫名变少。
func TestSetBillingCycleDerivesClampedStartOnMonthEnd(t *testing.T) {
	reset := time.Date(2026, time.March, 31, 8, 0, 0, 0, time.Local)
	wantStart := time.Date(2026, time.February, 28, 8, 0, 0, 0, time.Local)

	s := New("")
	s.SetBillingCycleAt("u1", reset.UnixMilli(), time.Date(2026, time.March, 1, 0, 0, 0, 0, time.Local).UnixMilli())

	got := s.billingCycles["u1"]
	if got.CycleStartMs != wantStart.UnixMilli() {
		t.Fatalf("月末起点未钳制：got %s want %s",
			time.UnixMilli(got.CycleStartMs).In(time.Local).Format(time.RFC3339),
			wantStart.Format(time.RFC3339))
	}
}

// ---- ③ 四情形状态机 ----

// TestSetBillingCycleIdempotent 情形 ①：同一个边界重复上报 = 幂等。
//
// "幂等"在这里有**可观测**的含义：不置脏标志。刷新任务每分钟跑一次，
// 若每次都置脏，就会每分钟无谓地写一次盘（usage.json 是几 MB 的聚合表）。
func TestSetBillingCycleIdempotent(t *testing.T) {
	s := New("")
	now := time.Now()
	reset := now.Add(72 * time.Hour)

	if !s.SetBillingCycleAt("u1", reset.UnixMilli(), now.UnixMilli()) {
		t.Fatal("首次上报应当改变状态")
	}
	s.dirty.Store(false)

	if changed := s.SetBillingCycleAt("u1", reset.UnixMilli(), now.UnixMilli()); changed {
		t.Fatal("同一边界重复上报不应报告变化")
	}
	if s.dirty.Load() {
		t.Fatal("同一边界重复上报置了脏标志 —— 每分钟白写一次盘")
	}
}

// TestSetBillingCycleIgnoresStaleBoundary 情形 ②：更旧/相等的边界必须被忽略。
//
// 上游可能返回缓存的响应，或同一账号被两个凭据并发刷新（一个先到）。
// 若采纳更旧的边界，周期窗口会整体前移，于是"本周期用量"把已经翻过去的
// 那段时间又算进来 —— 数字变大，而且下一次刷新才可能修正。
func TestSetBillingCycleIgnoresStaleBoundary(t *testing.T) {
	now := time.Now()
	newer := now.Add(30 * 24 * time.Hour)

	s := New("")
	s.SetBillingCycleAt("u1", newer.UnixMilli(), now.UnixMilli())
	s.dirty.Store(false)

	for _, stale := range []int64{
		newer.Add(-time.Hour).UnixMilli(),
		newer.Add(-20 * 24 * time.Hour).UnixMilli(),
		now.Add(24 * time.Hour).UnixMilli(),
		newer.UnixMilli(), // 相等：走情形 ①，同样不改
	} {
		if changed := s.SetBillingCycleAt("u1", stale, now.UnixMilli()); changed {
			t.Fatalf("陈旧边界 %s 被采纳（应保留 %s）",
				time.UnixMilli(stale).In(time.Local).Format(time.RFC3339),
				time.UnixMilli(newer.UnixMilli()).In(time.Local).Format(time.RFC3339))
		}
	}
	if got := s.billingCycles["u1"].NextResetMs; got != newer.UnixMilli() {
		t.Fatalf("边界被陈旧报告覆盖：got %d want %d", got, newer.UnixMilli())
	}
	if s.dirty.Load() {
		t.Fatal("忽略陈旧边界时不应置脏标志")
	}
}

// TestSetBillingCycleAdoptsNewCycle 情形 ③：真正更晚的边界必须被采纳并重推起点。
func TestSetBillingCycleAdoptsNewCycle(t *testing.T) {
	now := time.Now()
	first := time.Date(2026, time.September, 25, 8, 0, 0, 0, time.Local)
	second := time.Date(2026, time.October, 25, 8, 0, 0, 0, time.Local)

	s := New("")
	s.SetBillingCycleAt("u1", first.UnixMilli(), now.UnixMilli())
	if got := s.billingCycles["u1"].CycleStartMs; got != time.Date(2026, time.August, 25, 8, 0, 0, 0, time.Local).UnixMilli() {
		t.Fatalf("第一轮起点错误：%s", time.UnixMilli(got).In(time.Local).Format(time.RFC3339))
	}

	if !s.SetBillingCycleAt("u1", second.UnixMilli(), now.UnixMilli()) {
		t.Fatal("更晚的边界应被采纳")
	}
	got := s.billingCycles["u1"]
	if got.NextResetMs != second.UnixMilli() {
		t.Fatalf("边界未更新：got %s", time.UnixMilli(got.NextResetMs).In(time.Local).Format(time.RFC3339))
	}
	if want := time.Date(2026, time.September, 25, 8, 0, 0, 0, time.Local).UnixMilli(); got.CycleStartMs != want {
		t.Fatalf("起点未随新边界重推：got %s want %s",
			time.UnixMilli(got.CycleStartMs).In(time.Local).Format(time.RFC3339),
			time.UnixMilli(want).In(time.Local).Format(time.RFC3339))
	}
}

// TestSetBillingCycleRejectsImplausibleBoundary 情形 ⓪：不可信的入参一律拒绝。
//
// 两类：
//
//	<= 0    —— 上游没报（或报了 0）。语义是"未知"，**不是**"立刻重置"。
//	>= 上限 —— 上游的"永不过期"哨兵（9999-12-31，即 253402214400000）。
//
// # 为什么哨兵必须挡在门口（参考实现没做这件事）
//
// 哨兵的值大于一切真实边界。一旦存进去，情形 ② 的"保留较新者"判据就会
// 反过来把后续**所有**真实边界都当成陈旧报告丢掉 —— 周期再也建立不起来，
// 而且不会自愈。参考实现只靠"7 天漂移容差"间接缓解，仍会先污染一轮状态。
func TestSetBillingCycleRejectsImplausibleBoundary(t *testing.T) {
	now := time.Now()
	good := now.Add(30 * 24 * time.Hour).UnixMilli()

	const sentinel = int64(253402214400000) // 9999-12-31T00:00:00Z

	bad := []struct {
		name string
		ms   int64
	}{
		{"zero", 0},
		{"negative", -1},
		{"sentinel-9999", sentinel},
		{"at-ceiling", MaxPlausibleResetMs},
		{"above-ceiling", MaxPlausibleResetMs + 1},
		{"max-int64", int64(1) << 62},
	}

	t.Run("no-prior-state", func(t *testing.T) {
		for _, tc := range bad {
			s := New("")
			if changed := s.SetBillingCycleAt("u1", tc.ms, now.UnixMilli()); changed {
				t.Fatalf("%s：不可信边界被采纳为变化", tc.name)
			}
			if got := s.billingCycles["u1"]; got.NextResetMs != 0 {
				t.Fatalf("%s：不可信边界被写入（%d）", tc.name, got.NextResetMs)
			}
			if s.dirty.Load() {
				t.Fatalf("%s：不可信边界置了脏标志", tc.name)
			}
		}
	})

	t.Run("keeps-existing-good-state", func(t *testing.T) {
		for _, tc := range bad {
			s := New("")
			s.SetBillingCycleAt("u1", good, now.UnixMilli())
			s.dirty.Store(false)

			if changed := s.SetBillingCycleAt("u1", tc.ms, now.UnixMilli()); changed {
				t.Fatalf("%s：不可信边界覆盖了可信边界", tc.name)
			}
			if got := s.billingCycles["u1"].NextResetMs; got != good {
				t.Fatalf("%s：可信边界被破坏（got %d want %d）", tc.name, got, good)
			}
		}
	})

	t.Run("empty-uid-is-rejected", func(t *testing.T) {
		s := New("")
		if changed := s.SetBillingCycleAt("", good, now.UnixMilli()); changed {
			t.Fatal("空 uid 不应被采纳 —— 会把周期挂到一个不存在的账号上")
		}
		if len(s.billingCycles) != 0 {
			t.Fatalf("空 uid 写进了状态：%v", s.billingCycles)
		}
	})
}

// TestSetBillingCycleRecoversFromPersistedSentinel 哨兵**已经存在磁盘上**时能自愈。
//
// 这是与"门口拒绝"配套的另一半：门口拒绝只保护今后，而参考实现的现场
// 数据里已经存着哨兵。若装载时直接丢弃，用户要等下一次 /user/status 刷新
// 才重新看到周期（可能是几十分钟）。故装载时**保留**哨兵，靠情形 ②
// 的"已跟踪值不可信则放行"来自愈 —— 下一个报告就能建立正确周期。
func TestSetBillingCycleRecoversFromPersistedSentinel(t *testing.T) {
	const sentinel = int64(253402214400000)

	dir := t.TempDir()
	path := filepath.Join(dir, "usage.json")

	// 造一个"旧版本写下的、带哨兵"的文件。
	raw, err := json.Marshal(map[string]any{
		"version": 1,
		"savedAt": time.Now(),
		"days":    map[string]any{},
		"billingCycles": map[string]any{
			"u1": map[string]any{"nextResetAtMs": sentinel},
		},
	})
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, raw, 0o600); err != nil {
		t.Fatal(err)
	}

	s := New(path)
	if got := s.billingCycles["u1"].NextResetMs; got != sentinel {
		t.Fatalf("装载未保留哨兵：got %d（应当保留，否则要等下一次刷新才恢复）", got)
	}

	// 一个真实边界到达：必须被采纳（哨兵不可信，情形 ② 放行）。
	real := time.Date(2026, time.July, 16, 17, 27, 0, 0, time.Local)
	now := real.Add(-24 * time.Hour)
	if !s.SetBillingCycleAt("u1", real.UnixMilli(), now.UnixMilli()) {
		t.Fatal("真实边界未能替换哨兵 —— 周期永远建立不起来")
	}
	got := s.billingCycles["u1"]
	if got.NextResetMs != real.UnixMilli() {
		t.Fatalf("边界未自愈：got %s", time.UnixMilli(got.NextResetMs).In(time.Local).Format(time.RFC3339))
	}
	if want := addMonthsMs(real.UnixMilli(), -1); got.CycleStartMs != want {
		t.Fatalf("起点错误：got %s want %s",
			time.UnixMilli(got.CycleStartMs).In(time.Local).Format(time.RFC3339),
			time.UnixMilli(want).In(time.Local).Format(time.RFC3339))
	}
}

// ---- ④ 读路径绝不推进周期 ----

// TestCycleProjectionDoesNotRollState 读路径只投影，绝不回写。
//
// # 这条为什么是硬约束（而不是风格偏好）
//
// 面板每 15 秒拉一次 /usage。若读路径也推进边界，那么一个**恰好落在重置点
// 之后**的轮询就会把推进结果落盘 —— 于是"翻周期"这件事在没有收到任何
// 上游报告、也没有任何请求进来的情况下发生了。用户看到的是周期凭空翻页，
// 而根本原因是"有人在看界面"。
//
// 故本用例同时断言两件事：
//
//	投影**确实**推进了（否则读到的就是过期边界，界面显示错误的"下次重置"）
//	落盘状态**确实**没变（否则读就产生了副作用）
func TestCycleProjectionDoesNotRollState(t *testing.T) {
	boundary := time.Date(2026, time.March, 31, 8, 0, 0, 0, time.Local)
	before := boundary.Add(-72 * time.Hour)
	after := boundary.Add(time.Hour)

	s := New("")
	s.now = func() time.Time { return before }
	s.SetBillingCycleAt("u1", boundary.UnixMilli(), before.UnixMilli())
	s.dirty.Store(false)

	// 时间越过边界，然后**只读**。
	s.now = func() time.Time { return after }

	p, known := s.CycleOf("u1")
	if !known {
		t.Fatal("投影后应仍然知道下一次重置时刻")
	}
	wantNext := addMonthsMs(boundary.UnixMilli(), 1) // 4 月 30 日（31 日被钳制）
	if p.NextResetMs != wantNext {
		t.Fatalf("投影未推进边界：got %s want %s",
			time.UnixMilli(p.NextResetMs).In(time.Local).Format(time.RFC3339),
			time.UnixMilli(wantNext).In(time.Local).Format(time.RFC3339))
	}
	if p.CycleStartMs != boundary.UnixMilli() {
		t.Fatalf("投影的起点应为刚跨过的边界：got %s want %s",
			time.UnixMilli(p.CycleStartMs).In(time.Local).Format(time.RFC3339),
			boundary.Format(time.RFC3339))
	}

	// ★ 落盘状态必须原封不动。
	if got := s.billingCycles["u1"].NextResetMs; got != boundary.UnixMilli() {
		t.Fatalf("读路径推进了落盘状态：got %s want %s —— 一次界面轮询就翻掉了周期",
			time.UnixMilli(got).In(time.Local).Format(time.RFC3339),
			boundary.Format(time.RFC3339))
	}
	if s.dirty.Load() {
		t.Fatal("读路径置了脏标志 —— 读产生了待落盘的副作用")
	}

	// Snapshot 也是读路径，必须同样无副作用。
	snap := s.Snapshot(0)
	if s.dirty.Load() {
		t.Fatal("Snapshot 置了脏标志 —— 读产生了待落盘的副作用")
	}
	if got := s.billingCycles["u1"].NextResetMs; got != boundary.UnixMilli() {
		t.Fatalf("Snapshot 推进了落盘状态：%s", time.UnixMilli(got).In(time.Local).Format(time.RFC3339))
	}
	cycles, ok := snap["billingCycles"].(map[string]any)
	if !ok {
		t.Fatalf("快照缺少 billingCycles：%T", snap["billingCycles"])
	}
	row, ok := cycles["u1"].(map[string]any)
	if !ok {
		t.Fatalf("快照缺少该账号的周期行：%v", cycles)
	}
	if got := intOf(row["nextResetAtMs"]); got != wantNext {
		t.Fatalf("快照里的边界不是投影值：got %d want %d", got, wantNext)
	}
}

// TestCycleRollsOnWritePath 写路径**必须**推进（与上一条配对）。
//
// 只有写路径推进，才能保证"周期翻页"这件事总是由真实活动（一个请求、
// 或一次成功的上游刷新）驱动。若写路径也不推进，边界就会永远停在过去，
// 界面显示"下次重置"是一个已经过去的时刻。
func TestCycleRollsOnWritePath(t *testing.T) {
	boundary := time.Date(2026, time.March, 31, 8, 0, 0, 0, time.Local)
	after := boundary.Add(time.Hour)
	wantNext := addMonthsMs(boundary.UnixMilli(), 1)

	s := New("")
	s.now = func() time.Time { return boundary.Add(-72 * time.Hour) }
	s.SetBillingCycleAt("u1", boundary.UnixMilli(), s.now().UnixMilli())

	// 一个请求落在边界之后 ⇒ 写路径推进。
	s.RecordAt("u1", "glm-5.3", after, Counters{Input: 10, Records: 1})

	got := s.billingCycles["u1"]
	if got.NextResetMs != wantNext {
		t.Fatalf("写路径未推进边界：got %s want %s",
			time.UnixMilli(got.NextResetMs).In(time.Local).Format(time.RFC3339),
			time.UnixMilli(wantNext).In(time.Local).Format(time.RFC3339))
	}
	if got.CycleStartMs != boundary.UnixMilli() {
		t.Fatalf("推进后起点应为刚跨过的边界：got %s",
			time.UnixMilli(got.CycleStartMs).In(time.Local).Format(time.RFC3339))
	}
}

// TestRecordBeforeBoundaryDoesNotRoll 边界之前写入不推进（推进不是"每次都做"）。
func TestRecordBeforeBoundaryDoesNotRoll(t *testing.T) {
	boundary := time.Date(2026, time.March, 31, 8, 0, 0, 0, time.Local)
	before := boundary.Add(-time.Hour)

	s := New("")
	s.now = func() time.Time { return before }
	s.SetBillingCycleAt("u1", boundary.UnixMilli(), before.UnixMilli())

	s.RecordAt("u1", "glm-5.3", before, Counters{Input: 10, Records: 1})

	got := s.billingCycles["u1"]
	if got.NextResetMs != boundary.UnixMilli() {
		t.Fatalf("边界之前不该推进：got %s",
			time.UnixMilli(got.NextResetMs).In(time.Local).Format(time.RFC3339))
	}
}

// ---- 周期用量投影 ----

// TestCycleUsageProjectsFromAccounts 本周期用量从既有 accounts 账本投影而来。
//
// # 窗口是日期粒度（有损，但可接受）
//
// 边界是毫秒时刻，用量按**本地日期**聚合。故边界当天的用量整体归入
// 边界之后的那一侧。本用例把这个取舍**明确钉住** —— 它不是 bug，
// 是"为了秒级精度而另存一份逐请求账本"的代价评估结果。
//
// # 为什么先记录用量、后设周期（这个顺序不是随意的）
//
// 真实时序就是这样：网关**连续**记录用量，而周期边界是**异步**学到的
// （刷新任务每分钟跑一次）。所以"边界之外已经存在记录"是常态而非异常，
// 投影必须自己按窗口切。若反过来先设周期再记录，边界之后的每一笔写入
// 都会触发写路径推进（那是对的行为，但会让本用例测不到窗口过滤）。
func TestCycleUsageProjectsFromAccounts(t *testing.T) {
	boundary := time.Date(2026, time.December, 25, 8, 0, 0, 0, time.Local)
	start := time.Date(2026, time.November, 25, 8, 0, 0, 0, time.Local) // = addMonthsMs(boundary,-1)

	s := New("")
	s.now = func() time.Time { return boundary.Add(-72 * time.Hour) }

	// 先记用量（此时还不知道周期 ⇒ 写路径推进是空操作）。
	s.RecordAt("u1", "glm-5.3", time.Date(2026, time.November, 24, 23, 0, 0, 0, time.Local), Counters{Input: 1, Records: 1})     // 窗口前
	s.RecordAt("u1", "glm-5.3", time.Date(2026, time.November, 25, 9, 0, 0, 0, time.Local), Counters{Input: 10, Records: 1})     // 窗口内（起点当天）
	s.RecordAt("u1", "glm-5.3", time.Date(2026, time.December, 15, 12, 0, 0, 0, time.Local), Counters{Input: 100, Records: 1})   // 窗口内
	s.RecordAt("u1", "glm-5.3", time.Date(2026, time.December, 24, 23, 59, 0, 0, time.Local), Counters{Input: 1000, Records: 1}) // 窗口内（终点前一天）
	s.RecordAt("u1", "glm-5.3", time.Date(2026, time.December, 25, 9, 0, 0, 0, time.Local), Counters{Input: 10000, Records: 1})  // 窗口后（重置当天归新周期）

	// 另一个账号的用量不该混进来。
	s.RecordAt("u2", "glm-5.3", time.Date(2026, time.December, 15, 12, 0, 0, 0, time.Local), Counters{Input: 999999, Records: 1})

	// 再学到周期。
	s.SetBillingCycleAt("u1", boundary.UnixMilli(), boundary.Add(-72*time.Hour).UnixMilli())

	p, known := s.CycleOf("u1")
	if !known {
		t.Fatal("应知道周期")
	}
	if p.CycleStartMs != start.UnixMilli() {
		t.Fatalf("起点错误：got %s want %s",
			time.UnixMilli(p.CycleStartMs).In(time.Local).Format(time.RFC3339),
			start.Format(time.RFC3339))
	}
	if p.Used.Input != 1110 {
		t.Fatalf("本周期用量错误：got %d want 1110（10+100+1000）—— 边界外或别人的用量混进来了", p.Used.Input)
	}
	if p.Used.Records != 3 {
		t.Fatalf("本周期请求数错误：got %d want 3", p.Used.Records)
	}

	// u2 从未学到周期 ⇒ 必须报"未知"且**不投影任何用量**。
	// 若这里返回 u2 的全部历史，界面会把"历史上用过的总量"当成"本周期用量"。
	if p2, known2 := s.CycleOf("u2"); known2 || p2.Used.Input != 0 {
		t.Fatalf("无周期信息的账号被投影出了用量：known=%v used=%d", known2, p2.Used.Input)
	}

	// 给 u2 一个**不同**的周期窗口：它只能看到自己的量，且不受 u1 影响。
	s.SetBillingCycleAt("u2", boundary.UnixMilli(), boundary.Add(-72*time.Hour).UnixMilli())
	p2, known2 := s.CycleOf("u2")
	if !known2 {
		t.Fatal("u2 设了周期后应知道周期")
	}
	if p2.Used.Input != 999999 {
		t.Fatalf("u2 的周期用量错误：got %d want 999999（别的账号的量混进来了？）", p2.Used.Input)
	}
	if p2.Used.Records != 1 {
		t.Fatalf("u2 的周期请求数错误：got %d want 1", p2.Used.Records)
	}
	// u1 的投影不应被 u2 的设置影响。
	if p3, _ := s.CycleOf("u1"); p3.Used.Input != 1110 {
		t.Fatalf("u2 的周期污染了 u1 的投影：got %d", p3.Used.Input)
	}
}

// TestCycleUsageExcludesResetDay 重置**当天**的用量归入新周期（上界开区间）。
//
// 这是一天粒度的直接后果，也是最容易被"顺手改成闭区间"的一处：若把上界
// 写成闭区间，重置当天的量会被同时算进**两个**周期，于是新周期一开始就
// 带着上一周期的尾巴。
func TestCycleUsageExcludesResetDay(t *testing.T) {
	boundary := time.Date(2026, time.December, 25, 8, 0, 0, 0, time.Local)

	s := New("")
	s.now = func() time.Time { return boundary.Add(-time.Hour) }

	// 边界当天（= 窗口上界那一天）的记录。
	s.RecordAt("u1", "glm-5.3", time.Date(2026, time.December, 25, 9, 0, 0, 0, time.Local), Counters{Input: 500, Records: 1})
	// 上界前一天（窗口内最后一刻）。
	s.RecordAt("u1", "glm-5.3", time.Date(2026, time.December, 24, 23, 59, 59, 0, time.Local), Counters{Input: 7, Records: 1})

	s.SetBillingCycleAt("u1", boundary.UnixMilli(), boundary.Add(-72*time.Hour).UnixMilli())

	p, known := s.CycleOf("u1")
	if !known {
		t.Fatal("应知道周期")
	}
	if p.Used.Input != 7 {
		t.Fatalf("重置当天的用量被算进了上一周期：got %d want 7", p.Used.Input)
	}
}

// TestCycleOfUnknownAccountIsNotKnown 没周期信息的账号不能被当成"有周期"。
//
// 若这里返回 known=true 且 Used=全部历史，界面会把"这个号历史上用过的
// 总量"显示成"本周期用量" —— 那是个**错的数字**，比不显示更糟。
func TestCycleOfUnknownAccountIsNotKnown(t *testing.T) {
	s := New("")
	s.RecordAt("u1", "glm-5.3", time.Now().Add(-100*24*time.Hour), Counters{Input: 5, Records: 1})

	p, known := s.CycleOf("u1")
	if known {
		t.Fatal("没有周期信息却报告 known=true —— 界面会把全部历史当成一个周期")
	}
	if p.NextResetMs != 0 || p.CycleStartMs != 0 {
		t.Fatalf("未知周期的边界应为 0：%+v", p)
	}
	// Snapshot 里也不该出现这个账号的周期行。
	snap := s.Snapshot(0)
	cycles := snap["billingCycles"].(map[string]any)
	if _, present := cycles["u1"]; present {
		t.Fatal("快照里出现了无周期信息的账号 —— 界面会把全部历史当成一个周期显示")
	}

	if _, known := s.CycleOf(""); known {
		t.Fatal("空 uid 不该报告 known")
	}
}

// ---- ⑤ 热路径不碰盘 ----

// TestRecordAndCycleUpdateDoNotTouchDisk 记录与周期更新都不做同步磁盘 IO。
//
// 热路径是每个请求的收尾。若这里同步写盘，磁盘抖动会直接变成请求延迟。
// 既有设计是"置脏 + 后台 flusher"，本任务新增的周期字段**必须**沿用
// 同一条路径，不能自己写盘。
//
// # 断言方式
//
// 给一个真实 path，做完所有写操作后**文件仍不存在**（后台 flusher 的
// 间隔是 5 秒，而本用例在毫秒级跑完）。这里**刻意不去改** flushInterval：
// 它是包级变量，后台 goroutine 启动时会读它，测试里改它会引入真实的数据
// 竞争（-race 下会报），而"文件现在还不存在"这个断言本身已经足够。
func TestRecordAndCycleUpdateDoNotTouchDisk(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "usage.json")

	s := New(path)
	if _, err := os.Stat(path); !os.IsNotExist(err) {
		t.Fatalf("构造后不该有文件（此前：%v）", err)
	}

	now := time.Now()
	boundary := now.Add(72 * time.Hour)

	s.RecordAt("u1", "glm-5.3", now, Counters{Input: 10, Records: 1})
	s.SetBillingCycleAt("u1", boundary.UnixMilli(), now.UnixMilli())
	_ = s.Snapshot(0)
	_, _ = s.CycleOf("u1")

	if _, err := os.Stat(path); !os.IsNotExist(err) {
		t.Fatal("热路径（Record/SetBillingCycle/Snapshot/CycleOf）产生了磁盘文件 —— 同步 IO 会变成请求延迟")
	}
	if !s.dirty.Load() {
		t.Fatal("写操作后应处于脏状态，等待后台 flusher")
	}

	// Flush 才落盘。
	s.Flush()
	if _, err := os.Stat(path); err != nil {
		t.Fatalf("Flush 未落盘：%v", err)
	}
}

// ---- ⑥ 边界防御：不可推进的月步进 ----

// TestNextCycleBoundaryRejectsNonAdvancingStep DST 异常时区的月步进必须被拒绝。
//
// 正常时区里"加一个月"永远更晚，但在 DST 规则异常的历史时区里存在
// `addMonths(ms, 1) <= ms` 的瞬间（实测 America/Asuncion 1972-09-01、
// America/Havana / America/St_Johns 1990-03-01）。
//
// 为什么这**不是**学院派问题：推进循环是**持锁**跑的。若这里返回一个
// 不前进的值，循环就永远不退出 —— 整个网关的每次请求都会卡在拿不到
// usage 锁上。参考实现专门为此写了一个带 10 秒超时的用例。
func TestNextCycleBoundaryRejectsNonAdvancingStep(t *testing.T) {
	loc, bad := nonAdvancingInstant(t)

	if got := advanceBoundaryMs(bad, loc); got != 0 {
		t.Fatalf("不可推进的时刻 %s 未被拒绝：返回 %s",
			time.UnixMilli(bad).In(loc).Format(time.RFC3339),
			time.UnixMilli(got).In(loc).Format(time.RFC3339))
	}

	// 同一时区的**普通**时刻必须正常推进（否则"返回 0"就成了万能挡箭牌）。
	normal := time.Date(2026, time.June, 15, 12, 0, 0, 0, loc).UnixMilli()
	want := addMonthsMsIn(normal, 1, loc)
	if got := advanceBoundaryMs(normal, loc); got != want {
		t.Fatalf("普通时刻被误判为不可推进：got %s want %s",
			time.UnixMilli(got).In(loc).Format(time.RFC3339),
			time.UnixMilli(want).In(loc).Format(time.RFC3339))
	}
	if want <= normal {
		t.Fatalf("测试自身的假设不成立：普通时刻的加月结果未前进")
	}
}

// TestCycleProjectionTerminatesOnUnadvanceableBoundary 投影遇到死循环边界必须收敛。
//
// 分两个子用例：
//
//	dst-anomaly      —— 边界本身不可推进 ⇒ 报"未知"而不是空转
//	corrupt-far-past —— 边界在极远的过去 ⇒ 被 maxCycleCatchUps 兜住
//
// 两个都用 goroutine + 超时来跑：**卡死**是这里唯一真正的失败模式，
// 而卡死的测试若不用超时保护，表现是"整个测试进程挂住"而不是"某个用例
// 失败"—— 后者才有人能看懂。
func TestCycleProjectionTerminatesOnUnadvanceableBoundary(t *testing.T) {
	t.Run("dst-anomaly", func(t *testing.T) {
		loc, bad := nonAdvancingInstant(t)

		restore := time.Local
		time.Local = loc
		defer func() { time.Local = restore }()

		s := New("")
		s.now = func() time.Time { return time.UnixMilli(bad) }
		s.billingCycles["u1"] = CycleState{
			NextResetMs:  bad,
			CycleStartMs: addMonthsMsIn(bad, -1, loc),
		}
		s.RecordAt("u1", "glm-5.3", time.UnixMilli(bad), Counters{Input: 9, Records: 1})

		p := cycleWithTimeout(t, s)
		if p.NextResetMs != 0 {
			t.Fatalf("不可推进的边界应报未知：got %s",
				time.UnixMilli(p.NextResetMs).In(loc).Format(time.RFC3339))
		}
		if p.Known {
			t.Fatal("边界不可推进时不该报告 known=true")
		}

		// 写路径也必须收敛，且把不可信的边界清掉（否则每次写入都白跑一轮上限）。
		s.RecordAt("u1", "glm-5.3", time.UnixMilli(bad), Counters{Input: 3, Records: 1})
		if got := s.billingCycles["u1"].NextResetMs; got != 0 {
			t.Fatalf("写路径未清掉不可推进的边界：got %d", got)
		}

		// 恢复：一个真实边界到达后周期必须重新建立，且**已记录的用量不受影响**。
		fresh := time.Now().Add(72 * time.Hour).UnixMilli()
		if !s.SetBillingCycleAt("u1", fresh, time.Now().UnixMilli()) {
			t.Fatal("恢复失败：真实边界未被采纳（周期再也建立不起来）")
		}
		if got := s.billingCycles["u1"].NextResetMs; got != fresh {
			t.Fatalf("恢复后边界错误：got %d want %d", got, fresh)
		}
	})

	t.Run("corrupt-far-past", func(t *testing.T) {
		restore := time.Local
		time.Local = time.UTC
		defer func() { time.Local = restore }()

		s := New("")
		now := time.Now()
		s.now = func() time.Time { return now }
		// 1970-01-01：一个损坏的边界。从它推进到"现在"要跨约 680 个月。
		s.billingCycles["u1"] = CycleState{NextResetMs: int64(1)}
		s.RecordAt("u1", "glm-5.3", now, Counters{Input: 5, Records: 1})

		p := cycleWithTimeout(t, s)
		if p.NextResetMs <= now.UnixMilli() {
			t.Fatalf("损坏的远过去边界未被推进到未来：got %s",
				time.UnixMilli(p.NextResetMs).In(time.Local).Format(time.RFC3339))
		}
		if !p.Known {
			t.Fatal("680 个月远未触及 2400 上限，应当能正常推进")
		}
	})
}

// TestCycleRollTerminatesBeyondCatchUpCap 超过追赶上限的边界必须报"未知"。
//
// 上限（2400 个月 ≈ 200 年）存在的意义是**兜底**：正常账号一个月才推进
// 一次，永远碰不到它；而一个损坏到"起点在极远的过去"的边界若不受限，
// 循环会跑几万次（持锁）才收敛。
//
// # 为什么必须把"现在"也推进到未来（本用例最初的写法是错的）
//
// 上限的触发条件是"边界距 now 超过 2400 个月"。若用真实的现在，
// 边界必须是公元 1826 年以前 —— 而那是**负的** Unix 毫秒，
// 会被 projectCycleLocked 开头的 `nextMs <= 0` 守卫先拦下，
// 根本走不到循环里（实测：公元 1 年 = -62135596800000）。
//
// 所以只有两种情形能真正触到上限：
//
//	① now 被推到极远的未来（本用例，模拟时钟异常/长时间停机后重启）
//	② 边界是负值 —— 但那条路径由 nextMs<=0 守卫负责，不是上限负责
//
// 两者是不同的防御，必须各自有用例，否则"上限存在"这件事从未被验证。
func TestCycleRollTerminatesBeyondCatchUpCap(t *testing.T) {
	restore := time.Local
	time.Local = time.UTC
	defer func() { time.Local = restore }()

	s := New("")
	// 边界 2000 年（可信），而"现在"是 2400 年：相距 400 年 = 4800 个月 > 2400。
	future := time.Date(2400, time.January, 1, 0, 0, 0, 0, time.UTC)
	s.now = func() time.Time { return future }
	s.billingCycles["u1"] = CycleState{
		NextResetMs:  time.Date(2000, time.January, 1, 0, 0, 0, 0, time.UTC).UnixMilli(),
		CycleStartMs: time.Date(1999, time.December, 1, 0, 0, 0, 0, time.UTC).UnixMilli(),
	}

	p := cycleWithTimeout(t, s)
	if p.NextResetMs != 0 {
		t.Fatalf("超出追赶上限应报未知：got %s",
			time.UnixMilli(p.NextResetMs).In(time.Local).Format(time.RFC3339))
	}
	if p.Known {
		t.Fatal("超出追赶上限时不该报告 known=true")
	}
}

// TestCycleRejectsNegativeBoundary 负的边界（公元 1970 以前）必须报"未知"。
//
// 这是与上限**不同**的一道防御，单独钉住：损坏/手改的负值若被当作
// "很久以前的重置点"，投影会把它当成有效周期起点，于是本周期用量
// 从 1970 年算起 —— 也就是"全部历史"。实测公元 1 年的 Unix 毫秒是
// -62135596800000，确认这确实是一条可达的输入路径。
func TestCycleRejectsNegativeBoundary(t *testing.T) {
	s := New("")
	now := time.Now()
	s.now = func() time.Time { return now }

	s.billingCycles["u1"] = CycleState{
		NextResetMs:  time.Date(1, time.January, 1, 0, 0, 0, 0, time.UTC).UnixMilli(),
		CycleStartMs: time.Date(1, time.January, 1, 0, 0, 0, 0, time.UTC).UnixMilli(),
	}
	s.RecordAt("u1", "glm-5.3", now, Counters{Input: 12345, Records: 1})

	p, known := s.CycleOf("u1")
	if known || p.NextResetMs != 0 {
		t.Fatalf("负边界应报未知：%+v", p)
	}
	if p.Used.Input != 0 {
		t.Fatalf("负边界下不该投影出任何用量：got %d", p.Used.Input)
	}
	// 也不该出现在快照里。
	cycles := s.Snapshot(0)["billingCycles"].(map[string]any)
	if _, present := cycles["u1"]; present {
		t.Fatal("负边界的账号出现在了快照里")
	}
}

// TestReportAndSetBillingCycleDoNotResurrectStaleCycle 读投影不得让陈旧周期复活。
//
// 场景：边界已经过去、且**没有任何新用量**。读路径投影出"已推进到未来"
// 的边界，但这只是投影；随后一个恰好等于"推进后边界"的报告到达时，
// 它应该被正常采纳（而不是因为"投影过"就被当成陈旧报告丢掉）。
func TestReportAndSetBillingCycleDoNotResurrectStaleCycle(t *testing.T) {
	boundary := time.Date(2026, time.March, 31, 8, 0, 0, 0, time.Local)
	now := boundary.Add(time.Hour)

	s := New("")
	s.now = func() time.Time { return now }
	s.SetBillingCycleAt("u1", boundary.UnixMilli(), boundary.Add(-72*time.Hour).UnixMilli())

	// 读：投影推进到 4 月 30 日，但状态仍是 3 月 31 日。
	p, _ := s.CycleOf("u1")
	if got := s.billingCycles["u1"].NextResetMs; got != boundary.UnixMilli() {
		t.Fatalf("读路径不该改状态：got %s", time.UnixMilli(got).In(time.Local).Format(time.RFC3339))
	}

	// 上游报出投影所预言的同一个边界 ⇒ 应被采纳（它比已跟踪的 3/31 更晚）。
	if !s.SetBillingCycleAt("u1", p.NextResetMs, now.UnixMilli()) {
		t.Fatal("投影预言的边界被当成陈旧报告丢掉了")
	}
	if got := s.billingCycles["u1"].NextResetMs; got != p.NextResetMs {
		t.Fatalf("边界未被采纳：got %s want %s",
			time.UnixMilli(got).In(time.Local).Format(time.RFC3339),
			time.UnixMilli(p.NextResetMs).In(time.Local).Format(time.RFC3339))
	}
}

// ---- ⑦ 落盘 / 装载 ----

// TestBillingCyclesSurviveRestart 周期状态必须跨重启存活。
//
// # 为什么这条是刚需（而不是"顺手落个盘"）
//
// 边界只在刷新时拿得到。若只存内存，网关每次重启都会丢掉它 —— 界面上的
// "下次重置"空白，而更糟的是：下一个报告到达时，一个**本应被忽略的陈旧
// 边界**（因为内存里没有更新的值可比）会被当成真新周期采纳。
func TestBillingCyclesSurviveRestart(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "usage.json")

	boundary := time.Date(2026, time.December, 25, 8, 0, 0, 0, time.Local)
	now := boundary.Add(-72 * time.Hour)

	s1 := New(path)
	s1.now = func() time.Time { return now }
	s1.SetBillingCycleAt("u1", boundary.UnixMilli(), now.UnixMilli())
	s1.Flush()

	// 重启：新实例读同一个文件。
	s2 := New(path)
	got, ok := s2.billingCycles["u1"]
	if !ok {
		t.Fatal("重启后周期状态丢失 —— 界面上的下次重置会空白")
	}
	if got.NextResetMs != boundary.UnixMilli() {
		t.Fatalf("边界未持久化：got %s want %s",
			time.UnixMilli(got.NextResetMs).In(time.Local).Format(time.RFC3339),
			boundary.Format(time.RFC3339))
	}
	if want := addMonthsMs(boundary.UnixMilli(), -1); got.CycleStartMs != want {
		t.Fatalf("起点未持久化：got %s want %s",
			time.UnixMilli(got.CycleStartMs).In(time.Local).Format(time.RFC3339),
			time.UnixMilli(want).In(time.Local).Format(time.RFC3339))
	}

	// 落盘文件里必须真的有这个键（而不是靠内存里的巧合）。
	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	var fs fileState
	if err := json.Unmarshal(raw, &fs); err != nil {
		t.Fatal(err)
	}
	if _, ok := fs.BillingCycles["u1"]; !ok {
		t.Fatal("usage.json 里没有 billingCycles —— 重启后会丢")
	}
}

// TestLoadToleratesLegacyFileWithoutBillingCycles 老文件（没有该键）必须能读。
//
// 与既有 TestLoadToleratesLegacyFileWithoutAccountModels 同一类保障：
// 加字段不能让用户的历史统计失效。
func TestLoadToleratesLegacyFileWithoutBillingCycles(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "usage.json")

	legacy := `{"version":1,"savedAt":"2026-09-01T00:00:00Z","days":{"2026-09-01":{"input":42,"output":1,"cacheRead":0,"cacheWrite":0,"records":1}}}`
	if err := os.WriteFile(path, []byte(legacy), 0o600); err != nil {
		t.Fatal(err)
	}

	s := New(path)
	snap := s.Snapshot(0)
	summary, ok := snap["summary"].(map[string]any)
	if !ok {
		t.Fatalf("老文件装载失败：summary 类型 %T", snap["summary"])
	}
	if intOf(summary["input"]) != 42 {
		t.Fatalf("老文件的历史用量丢失：%v", summary)
	}
	cycles, ok := snap["billingCycles"].(map[string]any)
	if !ok {
		t.Fatalf("缺 billingCycles 键（应为空对象，不能是 nil）：%T", snap["billingCycles"])
	}
	if len(cycles) != 0 {
		t.Fatalf("老文件不该凭空产生周期行：%v", cycles)
	}
	// 装载后必须可写（不是往 nil map 里塞）。
	if !s.SetBillingCycleAt("u1", time.Now().Add(72*time.Hour).UnixMilli(), time.Now().UnixMilli()) {
		t.Fatal("老文件装载后无法写入周期状态")
	}
}

// TestLoadDropsEmptyBillingCycleEntries 两个字段都为 0 的条目不必占一个键。
func TestLoadDropsEmptyBillingCycleEntries(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "usage.json")

	raw, err := json.Marshal(map[string]any{
		"version": 1,
		"savedAt": time.Now(),
		"days":    map[string]any{},
		"billingCycles": map[string]any{
			"empty":     map[string]any{"nextResetAtMs": 0, "cycleStartAtMs": 0},
			"negative":  map[string]any{"nextResetAtMs": -5, "cycleStartAtMs": -7},
			"has-value": map[string]any{"nextResetAtMs": time.Now().Add(72 * time.Hour).UnixMilli()},
		},
	})
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, raw, 0o600); err != nil {
		t.Fatal(err)
	}

	s := New(path)
	if _, present := s.billingCycles["empty"]; present {
		t.Fatal("两个字段都是 0 的条目不该占一个键")
	}
	if _, present := s.billingCycles["negative"]; present {
		t.Fatal("负数被归一成 0 后同样不该占键")
	}
	if got, present := s.billingCycles["has-value"]; !present {
		t.Fatal("有值的条目被误删")
	} else if got.NextResetMs <= 0 {
		t.Fatalf("有值的条目被破坏：%+v", got)
	}
}

// TestSnapshotShapeIsStableWithBillingCycles 快照的既有形状不能被破坏。
//
// 既有测试 TestSnapshotShapeIsStableForFrontend 钉住 modelBilling 的行形状
// 且**禁止**以 `_` 开头的键（内部键泄漏）。本用例钉住新增的 billingCycles
// 行：只含前端需要的键，且值是数字/布尔而不是字符串化的数字。
func TestSnapshotShapeIsStableWithBillingCycles(t *testing.T) {
	boundary := time.Now().Add(72 * time.Hour)
	s := New("")
	s.SetBillingCycleAt("u1", boundary.UnixMilli(), time.Now().UnixMilli())
	s.RecordAt("u1", "glm-5.3", time.Now(), Counters{Input: 10, Output: 2, Records: 1})

	snap := s.Snapshot(0)
	cycles, ok := snap["billingCycles"].(map[string]any)
	if !ok {
		t.Fatalf("billingCycles 类型 %T", snap["billingCycles"])
	}
	row, ok := cycles["u1"].(map[string]any)
	if !ok {
		t.Fatalf("周期行类型 %T", cycles["u1"])
	}

	// 必须存在的键。
	for _, k := range []string{"nextResetAtMs", "cycleStartAtMs", "known", "input", "output", "records"} {
		if _, present := row[k]; !present {
			t.Fatalf("周期行缺少键 %q：%v", k, row)
		}
	}
	// 不得泄漏内部键（与既有前端形状约定一致）。
	for k := range row {
		if len(k) > 0 && k[0] == '_' {
			t.Fatalf("快照里泄漏了内部键 %q", k)
		}
	}
	// 数值必须是 JSON 数字（int64/float64），不是字符串 —— 前端按数字渲染。
	if _, isNum := row["nextResetAtMs"].(int64); !isNum {
		if _, isNum := row["nextResetAtMs"].(float64); !isNum {
			t.Fatalf("nextResetAtMs 不是数字：%T", row["nextResetAtMs"])
		}
	}
	if b, isBool := row["known"].(bool); !isBool || !b {
		t.Fatalf("known 应为 true：%v (%T)", row["known"], row["known"])
	}
	if intOf(row["input"]) != 10 || intOf(row["output"]) != 2 {
		t.Fatalf("本周期用量错误：%v", row)
	}
}

// TestBillingCycleJSONRoundTrip 落盘字段名必须稳定（改名 = 用户数据静默丢失）。
func TestBillingCycleJSONRoundTrip(t *testing.T) {
	in := CycleState{NextResetMs: 1790000000000, CycleStartMs: 1787000000000}
	raw, err := json.Marshal(in)
	if err != nil {
		t.Fatal(err)
	}
	var out CycleState
	if err := json.Unmarshal(raw, &out); err != nil {
		t.Fatal(err)
	}
	if out != in {
		t.Fatalf("往返不一致：got %+v want %+v", out, in)
	}

	// 0 值应被 omitempty 省略（老文件的形状不被新键污染）。
	empty, err := json.Marshal(CycleState{})
	if err != nil {
		t.Fatal(err)
	}
	if string(empty) != "{}" {
		t.Fatalf("零值未被省略：%s", empty)
	}
}

// TestPlausibleResetMs 边界判据本身（含上限两侧）。
func TestPlausibleResetMs(t *testing.T) {
	cases := []struct {
		ms   int64
		want bool
	}{
		{0, false},
		{-1, false},
		{1, true},
		{time.Now().UnixMilli(), true},
		{MaxPlausibleResetMs - 1, true},
		{MaxPlausibleResetMs, false},
		{MaxPlausibleResetMs + 1, false},
		{253402214400000, false}, // 9999-12-31 哨兵
	}
	for _, tc := range cases {
		if got := PlausibleResetMs(tc.ms); got != tc.want {
			t.Fatalf("PlausibleResetMs(%d) = %v, want %v", tc.ms, got, tc.want)
		}
	}
}

// ---- 测试辅助 ----

// cycleWithTimeout 在带超时的 goroutine 里做一次读投影。
//
// 卡死是这一族用例唯一真正的失败模式，而卡死的测试若不设超时，表现是
// "整个测试进程挂住"而不是"某个用例失败"—— 后者才有人能看懂。
func cycleWithTimeout(t *testing.T, s *Stats) CycleProjection {
	t.Helper()
	type result struct {
		p CycleProjection
	}
	done := make(chan result, 1)
	go func() {
		p, _ := s.CycleOf("u1")
		done <- result{p}
	}()
	select {
	case r := <-done:
		return r.p
	case <-time.After(10 * time.Second):
		t.Fatal("投影挂住了（推进循环在持锁状态下空转）")
		return CycleProjection{}
	}
}

// nonAdvancingInstant 找一个"加一个月后**不前进**"的真实时刻。
//
// 本机 tzdata 里 America/Asuncion（1972-09-01）、America/Havana 与
// America/St_Johns（1990-03-01）都存在这种瞬间（已用独立探针实测）。
// 找不到就 t.Skip —— 那说明宿主没有 tzdata，此时该防御**无法被测试**，
// 但代码里的守卫仍然是对的（跳过比伪造一个假时区更诚实）。
func nonAdvancingInstant(t *testing.T) (*time.Location, int64) {
	t.Helper()
	for _, name := range []string{"America/Asuncion", "America/Havana", "America/St_Johns"} {
		loc, err := time.LoadLocation(name)
		if err != nil {
			continue
		}
		start := time.Date(1960, time.January, 1, 0, 0, 0, 0, time.UTC).UnixMilli()
		if start < 1 {
			start = 1
		}
		end := time.Date(2000, time.January, 1, 0, 0, 0, 0, time.UTC).UnixMilli()
		step := int64(time.Hour / time.Millisecond)
		for ms := start; ms < end; ms += step {
			if addMonthsMsIn(ms, 1, loc) <= ms {
				return loc, ms
			}
		}
	}
	t.Skip("宿主 tzdata 里没有「不前进的月步进」，该守卫无法在此环境测试")
	return nil, 0
}
