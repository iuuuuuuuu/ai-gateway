package usage

// cycle.go 账号**计费周期**（N4，2026-09-28）。
//
// # 这是什么
//
// Qoder 的套餐额度按"周期"重置（上游 `/algo/api/v3/user/status` 报一个
// `nextResetAt` 时刻）。要知道"本周期用了多少、下次什么时候重置"，就必须
// 记住这个边界 —— 本文件负责记住它、在写路径上推进它、在读路径上投影它。
//
// # 为什么不另建一本账（与 fileState.BillingCycles 的注释同一件事）
//
// 周期是"每账号一条"的**状态**，与 BillingMeta 同构（不可加、需要落盘、
// 需要跨重启稳定）。本包已有 fileState + load + Flush + dirty 这套管线，
// 周期字段直接挂上去，零新增管线。
//
// 更关键的是：**本周期用量不另存一份**。它是从既有的
// `accounts[uid][日期]` 里按周期窗口**投影**出来的（见 cycleUsageLocked）。
// 另存一份累加器就必须在每条记录路径上同步两处，任何一处漏掉都会让
// 界面上的"本周期用量"与实际用量悄悄分叉 —— 那种偏差无法自愈。
//
// # 读路径绝不推进周期（这条是硬约束）
//
// 面板会周期性拉 `/usage`。若读路径也"推进"边界，一个恰好落在重置点
// 之后的轮询就会把推进结果落盘，从而吃掉上一周期的状态。故：
//
//	读（CycleOf / Snapshot）—— 纯投影：在局部拷贝上推进，绝不回写
//	写（recordAt / SetBillingCycle）—— 真正推进 s.billingCycles
//
// 与参考实现（`test-bin/recon/qoder-echoping07/stats/stats.go` 的
// `projectCycleLocked` / `rollCycleLocked` 分工）一致。
//
// # 与参考实现的两处**有意分歧**（都是更保守的一侧）
//
//  1. 参考实现的 `setBillingCycleAt` 会采纳任何 >0 的边界，包括上游的
//     "永不过期"哨兵（`253402214400000` = 9999-12-31）。本实现**拒绝**
//     不可信的入参（见 case ⓪）：一旦把哨兵存进去，它的值大于一切真实
//     边界，会反过来让 case ③ 的"保留较新者"判据失效，于是下一个陈旧
//     报告就能覆盖掉一个好边界。从磁盘读回的哨兵仍然容忍并自愈
//     （见 load 与 TestCycleRecoversFromPersistedSentinel）。
//  2. 参考实现有 `cycleDriftToleranceMs`（7 天漂移容差），用来决定要不要
//     把 `CycleCredits` 清零。本实现**不需要**它：本周期用量是投影出来的，
//     推进边界不会销毁任何已记录的数据，所以"漂移"与"真新周期"在行为上
//     没有区别 —— 少一个阈值就少一处会判错的地方。

import "time"

// MaxPlausibleResetMs 可信边界的上限：2100-01-01T00:00:00Z。
//
// 与 `internal/qoder` 的 `MaxPlausibleResetMs` **同值但各自一份**：
// 本包是 stdlib-only 的统计层，qoder 是协议层，二者不互相依赖
// （qoder 依赖 usage 才是正确的方向，而本任务连那个方向都不需要）。
// 两边各自注释指向对方，改一处时另一处要一起改。
const MaxPlausibleResetMs = int64(4102444800000) // 2100-01-01T00:00:00Z

// maxCycleCatchUps 单次推进最多跨多少个月。
//
// 2400 个月 = 200 年：正常账号永远碰不到（一个月一次），而一个**损坏的**
// 远过去边界（例如 1970）不会让推进循环空转上百万次 —— 那个循环是持锁
// 跑的，空转等于把整个网关卡死。与参考实现同值。
const maxCycleCatchUps = 2400

// PlausibleResetMs 报告一个重置时刻是否可信。
//
// 不可信的有两类，都必须拒绝：
//
//	<= 0        —— 上游没报（或报了 0），语义是"未知"，不是"立刻重置"
//	>= 上限     —— 上游的"永不过期"哨兵（9999-12-31），不是真实时刻
func PlausibleResetMs(ms int64) bool {
	return ms > 0 && ms < MaxPlausibleResetMs
}

// CycleState 单个账号的计费周期**状态**（落盘）。
//
// 只有两个字段：本周期用量是从 accounts 投影出来的，不在这里存。
type CycleState struct {
	// NextResetMs 当前周期的结束时刻（下一次重置）。0 = 未知。
	NextResetMs int64 `json:"nextResetAtMs,omitempty"`
	// CycleStartMs 当前周期的开始时刻。
	//
	// 由 `addMonthsMs(NextResetMs, -1)` 推导（套餐是月度的），而不是由上游
	// 直接给出 —— 上游只报"下次什么时候重置"。推进边界时它等于被跨过的
	// 那个旧边界（比"再减一个月"更准：跨过的边界就是事实）。
	CycleStartMs int64 `json:"cycleStartAtMs,omitempty"`
}

// CycleProjection 某账号当前周期的**只读投影**。
//
// 读路径返回它而不是 CycleState：投影出来的边界可能已经推进过（时间过了
// 重置点），但那个推进**没有**被写回。这样面板随时可读，而"落盘的状态"
// 只由写路径改变。
type CycleProjection struct {
	// NextResetMs 投影后的下一次重置时刻；0 = 未知（无周期信息或边界不可推进）。
	NextResetMs int64 `json:"nextResetAtMs"`
	// CycleStartMs 投影后的当前周期开始时刻；0 = 未知。
	CycleStartMs int64 `json:"cycleStartAtMs"`
	// Used 本周期已记录的用量（按周期窗口从 accounts 投影）。
	//
	// 窗口是**日期粒度**的：周期边界是毫秒时刻，而用量按本地日期聚合。
	// 因此边界当天的用量整体归入该边界所属的那一侧。这是刻意的取舍 ——
	// 为了把精度做到秒级而另存一份逐请求账本，代价远大于收益
	//（见本文件开头"为什么不另建一本账"）。
	Used Counters `json:"used"`
	// Known 是否知道"下一次重置在什么时候"（= NextResetMs > 0）。
	//
	// false 时 Used **不是**"本周期用量"：它只保证"从 CycleStartMs 起"
	//（CycleStartMs 也为 0 时就是全部历史）。界面必须据此显示"未知"，
	// 而不是把它当成一个周期用量 —— 那会把全部历史说成一个月的量。
	Known bool `json:"known"`
}

// SetBillingCycle 采纳上游报告的周期边界（**写路径**）。
//
// 返回是否改变了已跟踪的状态（供调用方统计"本轮更新了几个账号"）。
//
// 详见 setBillingCycleAt 的四情形说明。
func (s *Stats) SetBillingCycle(uid string, nextResetMs int64) bool {
	return s.setBillingCycleAt(uid, nextResetMs, s.clock().UnixMilli())
}

// SetBillingCycleAt 同 SetBillingCycle，但显式指定"现在"（测试用）。
func (s *Stats) SetBillingCycleAt(uid string, nextResetMs, nowMs int64) bool {
	return s.setBillingCycleAt(uid, nextResetMs, nowMs)
}

// setBillingCycleAt 是周期边界的**四情形状态机**。
//
// 输入是上游刚报的 `nextResetMs` 与本地"现在"。四种情形：
//
//	⓪ 不可信（<=0 或 >= 2100 上限）—— 保留已跟踪的边界，什么都不做
//	① 与已跟踪的边界相同        —— 幂等：不置脏、不写盘
//	② 比已跟踪的边界更旧/相等   —— 陈旧报告：保留较新的那个
//	③ 真新周期                  —— 采纳，并推导周期起点
//
// （编号与参考实现的四情形一一对应，只是参考实现没有 ⓪、且 ③ 里多一步
// 把 `CycleCredits` 清零 —— 本实现没有可清零的累加器，见文件头。）
//
// # 为什么先推进再比较
//
// 已跟踪的边界可能**已经过去**（网关很久没刷新，或上游一直没报）。
// 那种情况下先 roll 到"现在所属的周期"，再拿新报告去比，才比得准：
// 否则一个本应等于"推进后的边界"的报告会被 case ② 当成陈旧报告丢掉。
func (s *Stats) setBillingCycleAt(uid string, nextResetMs, nowMs int64) bool {
	if uid == "" {
		return false
	}
	if !PlausibleResetMs(nextResetMs) {
		// ⓪ 上游没报 / 报了哨兵。保留既有状态 —— 不要用一个不可信的值
		// 覆盖一个可信的值（那会让 case ② 的判据失效，见文件头分歧 1）。
		return false
	}

	s.mu.Lock()
	defer s.mu.Unlock()

	changed := s.rollCycleLocked(uid, nowMs)

	cur := s.billingCycles[uid]
	if cur.NextResetMs == nextResetMs {
		// ① 同一个周期：幂等。**不置脏** —— 否则每轮刷新都会写一次盘，
		// 而这个值根本没变（参考实现同样在此提前返回）。
		return changed
	}
	if PlausibleResetMs(cur.NextResetMs) && nextResetMs <= cur.NextResetMs {
		// ② 陈旧报告：上游（或缓存的响应）报了一个不晚于当前边界的时刻。
		// 保留较新的那个。判据要求 cur **可信** —— 若 cur 是不可信的哨兵
		//（从旧版本文件读回来的），这里必须放行，否则真实边界永远进不来。
		return changed
	}

	// ③ 真新周期：采纳边界，并按"月度套餐"推导周期起点。
	cur.NextResetMs = nextResetMs
	cur.CycleStartMs = addMonthsMs(nextResetMs, -1)
	s.billingCycles[uid] = cur
	s.dirty.Store(true)
	return true
}

// CycleOf 读某账号当前周期的**只读投影**（读路径，绝不修改状态）。
//
// 第二个返回值报告该账号是否**有周期信息**（false = 从没收到过边界，
// 此时 Used 是全部历史用量，不能当"本周期"用）。
func (s *Stats) CycleOf(uid string) (CycleProjection, bool) {
	if uid == "" {
		return CycleProjection{}, false
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	p := s.projectCycleLocked(uid, s.clock().UnixMilli())
	return p, p.Known
}

// projectCycleLocked 在**局部拷贝**上推进周期并投影本周期用量。
//
// 调用方必须已持有 s.mu。**绝不回写** s.billingCycles —— 面板轮询恰好
// 落在重置点之后时，回写会让"推进"被落盘（见文件头）。
func (s *Stats) projectCycleLocked(uid string, nowMs int64) CycleProjection {
	cur := s.billingCycles[uid] // 值拷贝：推进只发生在这份拷贝上
	startMs, nextMs := cur.CycleStartMs, cur.NextResetMs

	if nextMs <= 0 {
		// 没有可信边界（从未刷新过 / 被 load 归一成 0 / 被推进清空）⇒
		// 我们**不知道**这个账号的周期。返回零值而不是"从某个起点算起"：
		// 把全部历史当成一个周期报出去，界面会显示一个错的数字，
		// 比不显示更糟（见 cycleReportLocked 的说明）。
		return CycleProjection{}
	}
	if startMs <= 0 {
		// 有边界但缺起点：只可能来自旧文件或手改（setBillingCycleAt 总会
		// 一起写起点）。按月度契约推导，与写入路径保持一致 —— 若这里留 0，
		// 窗口就没有下界，本周期用量会把**全部历史**算进来。
		startMs = addMonthsMs(nextMs, -1)
	}

	for i := 0; nextMs > 0 && nowMs >= nextMs && i < maxCycleCatchUps; i++ {
		// 跨过一个边界：新周期的起点就是刚跨过的那个边界。
		startMs = nextMs
		adv := nextCycleBoundary(nextMs)
		if adv == 0 {
			// 该时刻的下一个月步进**不前进**（DST 异常时区）。
			// 再推下去就是死循环 ⇒ 报"未知"，让调用方显示"无周期信息"。
			nextMs = 0
			break
		}
		nextMs = adv
	}
	if nextMs > 0 && nowMs >= nextMs {
		// 追上了 maxCycleCatchUps 上限仍未越过 now：边界不可信，报"未知"。
		nextMs = 0
	}

	return CycleProjection{
		NextResetMs:  nextMs,
		CycleStartMs: startMs,
		Used:         s.cycleUsageLocked(uid, startMs, nextMs),
		// Known 在**推进之后**判定：边界不可推进（DST 异常）或超出追赶上限
		// 时 nextMs 被清成 0，此时我们确实不知道"下次什么时候重置" ——
		// 报 known 会让界面把一个投影不出来的周期当成已知周期来显示。
		Known: nextMs > 0,
	}
}

// rollCycleLocked 在**真实状态**上推进周期（写路径）。
//
// 调用方必须已持有 s.mu。返回是否改变了状态。
//
// 与 projectCycleLocked 同构，但改的是 s.billingCycles 本身 —— 只有写路径
// （有请求进来、或刷新拿到了新边界）才允许这么做。
func (s *Stats) rollCycleLocked(uid string, nowMs int64) bool {
	cur, ok := s.billingCycles[uid]
	if !ok || cur.NextResetMs <= 0 {
		return false
	}
	changed := false
	for i := 0; cur.NextResetMs > 0 && nowMs >= cur.NextResetMs && i < maxCycleCatchUps; i++ {
		cur.CycleStartMs = cur.NextResetMs
		adv := nextCycleBoundary(cur.NextResetMs)
		if adv == 0 {
			// 不可推进的边界：清成"未知"而不是留在原地 —— 留在原地会让
			// 每次写路径都再试一遍同一个死循环（虽然被上限兜住，但那是
			// 每写一次白跑 2400 次）。清掉后，下一个真实边界会重新建立周期。
			cur.NextResetMs = 0
			changed = true
			break
		}
		cur.NextResetMs = adv
		changed = true
	}
	if cur.NextResetMs > 0 && nowMs >= cur.NextResetMs {
		// 上限用尽仍未越过 now：这个边界不可信，同样清成"未知"。
		cur.NextResetMs = 0
		changed = true
	}
	if changed {
		s.billingCycles[uid] = cur
	}
	return changed
}

// cycleUsageLocked 把周期窗口 [startMs, nextMs) 投影到按日聚合的用量上。
//
// 调用方必须已持有 s.mu。
//
// # 为什么可以这么做（而不是另存一个累加器）
//
// `s.accounts[uid]` 已经是"账号 × 本地日期"的完整账本，而周期窗口天然
// 落在日期区间上：把窗口两端各映射成一个日期键，落在区间内的日期求和即可。
// 窗口两端是毫秒，映射到日期是**有损**的（见 CycleProjection.Used 的注释）。
//
// # 边界情况
//
//	startMs <= 0  —— 起点未知：不设下界（能确定的上界仍然生效）
//	nextMs  <= 0  —— 终点未知：不设上界
//	两者都未知    —— 返回全部历史（调用方据 Known=false 区分语义）
func (s *Stats) cycleUsageLocked(uid string, startMs, nextMs int64) Counters {
	perDay := s.accounts[uid]
	if len(perDay) == 0 {
		return Counters{}
	}
	// dayLayout 是定宽字符串（2006-01-02），故可直接按字典序比较。
	startDay, endDay := "", ""
	if startMs > 0 {
		startDay = time.UnixMilli(startMs).In(time.Local).Format(dayLayout)
	}
	if nextMs > 0 {
		endDay = time.UnixMilli(nextMs).In(time.Local).Format(dayLayout)
	}

	var total Counters
	for day, c := range perDay {
		if startDay != "" && day < startDay {
			continue
		}
		if endDay != "" && day >= endDay {
			// 上界是**开区间**：重置当天的用量属于新周期。
			continue
		}
		total = total.Add(c)
	}
	return total
}

// addMonthsMs 把毫秒时刻按"日历月"加减 n 个月（本地时区）。
//
// # 为什么不能直接用 time.AddDate（这条是真踩过的坑）
//
// AddDate 遇到"月末没有那一天"时**不钳制，而是溢出**：
//
//	3 月 31 日 减一个月 → 3 月 3 日（2 月 31 日溢出成 3 月 3 日）
//	5 月 31 日 减一个月 → 5 月 1 日（4 月 31 日溢出成 5 月 1 日）
//
// 而这里的返回值要用来推导"周期起点"，起点一旦落到月初附近，就会让
// 按窗口投影出的"本周期用量"错得离谱（把上一周期整月的用量算进来，
// 或者把本周期的头几天漏掉）。故必须**钳制到当月最后一天**。
func addMonthsMs(ms int64, n int) int64 {
	return addMonthsMsIn(ms, n, time.Local)
}

// addMonthsMsIn 同 addMonthsMs，但显式指定时区（测试用）。
func addMonthsMsIn(ms int64, n int, loc *time.Location) int64 {
	if ms <= 0 {
		return 0
	}
	t := time.UnixMilli(ms).In(loc)
	// 先定位到目标月的 1 号（避免"先算日再溢出"），再求该月最后一天，
	// 最后把日钳制进去。时分秒与纳秒原样保留 —— 重置时刻的"几点几分"
	// 是上游给的，不能因为跨月就丢掉。
	target := time.Date(t.Year(), t.Month()+time.Month(n), 1,
		t.Hour(), t.Minute(), t.Second(), t.Nanosecond(), t.Location())
	// time.Date 的"第 0 日"就是上个月最后一天，故 Month()+1 的 0 日 =
	// target 所在月的最后一天。
	last := time.Date(target.Year(), target.Month()+1, 0,
		t.Hour(), t.Minute(), t.Second(), t.Nanosecond(), t.Location()).Day()
	day := t.Day()
	if day > last {
		day = last
	}
	return time.Date(target.Year(), target.Month(), day,
		t.Hour(), t.Minute(), t.Second(), t.Nanosecond(), t.Location()).UnixMilli()
}

// nextCycleBoundary 求"下一个周期边界"；0 表示不可推进。
func nextCycleBoundary(nextMs int64) int64 {
	return advanceBoundaryMs(nextMs, time.Local)
}

// advanceBoundaryMs 同 nextCycleBoundary，但显式指定时区（测试用）。
//
// 返回 0 表示**这一步没有前进**（`adv <= nextMs`）。这在正常时区不会发生，
// 但 DST 异常的历史时区里存在"某个月步进后时刻反而更早"的瞬间
// （实测 America/Asuncion 1972-09、America/Havana、America/St_Johns）。
// 那种时刻若不返回 0，推进循环就会**永远不前进**，而它是持锁跑的 ——
// 等于把网关卡死。故这里必须显式拒绝，由调用方把边界清成"未知"。
func advanceBoundaryMs(nextMs int64, loc *time.Location) int64 {
	adv := addMonthsMsIn(nextMs, 1, loc)
	if adv <= nextMs {
		return 0
	}
	return adv
}
