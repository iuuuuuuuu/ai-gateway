package main

// billing_cycle.go 把 Qoder 的**计费周期**（下次重置时刻）刷新接进网关的
// 积分巡检循环。
//
// # 这个文件解决什么（N4 的最后一公里）
//
// `internal/usage/cycle.go`（2026-09-28）已经实现了"边界 → 周期投影"的全部
// 逻辑，`internal/qoder/userstatus.go` 也已经能从上游取到 `nextResetAt`；
// 但**没有任何生产路径调用它们** —— `SetBillingCycle` / `CycleOf` 当时是
// 死代码，于是 `/usage` 的 `billingCycles` 永远是空 map，界面上的
// 「本周期用量 / 下次重置」永远没有数据，**而且不报任何错**。
//
// 本文件就是那条缺失的连线：遍历池里的 Qoder 账号 → 逐个取上游周期 →
// 写进 `usage.Stats`。
//
// # 为什么是只读巡检，不是领取动作
//
// 与 `product_tasks.go`（Qoder/ZCode 权益领取）刻意分开：那边是**写操作**
// （会消耗/占用上游资源，需要幂等、hold 闸门、配额判断），这边是**纯只读
// 状态轮询**，属"积分到期巡检"那一族 —— 故搭 `RunCreditRefreshLoop` 的
// 便车（见 internal/scheduler/scheduler.go 的 RunBillingCycleRefresh 注释）。
//
// # ⚠ 已知耦合：credit_refresh_enabled=false 时本刷新也不跑
//
// 它搭的是积分巡检那条循环。若用户关掉 `pool.credit_refresh_enabled`，
// 计费周期刷新会**一起停** —— 这一点写进了本任务的降级说明，不是疏漏。
//
// # UID 同源（最容易写错、且错了不报错的一处）
//
// `usage.RecordBilled` 记的是 `pool.Status.UID`，而 `SetBillingCycle` 必须
// 记在**同一个** uid 上，否则周期与用量永远 join 不上 —— 界面显示 0，
// 没有任何错误信息。证据链（本任务要求自查）：
//
//	cmd/server/main.go:40            qoderAuthOf: UID = cr.UID
//	internal/pool/pool.go:1447       upsertLocked 以 a.UID 为键
//	internal/qoder/dispatch.go:372   Cred{UID: a.UID, …}
//	internal/server/forward.go:967   uid := acct.UID
//	internal/server/handler.go:576   RecordBilled(s.uid, …)
//
// 故本文件一律用 `pool.Status.UID` 作键，并把查询用的 `Cred.UID` **强制**
// 对齐到它（见 billingCycleCredOf 的尾注）。

import (
	"context"
	"log"
	"time"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/pool"
	"workbuddy2api/internal/qoder"
	"workbuddy2api/internal/usage"
)

// billingCycleGap 账号之间的间隔，避免瞬间并发打满上游。
//
// 与 internal/scheduler 的 `creditRefreshGap` **同量级**（都是 300ms）：
// 那个常量是包内未导出的，而 `internal/scheduler` 不 import `internal/qoder`
// 也不 import `internal/usage` —— 这条依赖方向不能反过来，故这里独立声明
// 一份。**改一处时另一处要跟着看**（两处都在注释里互相指了名）。
const billingCycleGap = 300 * time.Millisecond

// billingCycleFetcher 取一个账号的"下次重置时刻"（epoch 毫秒）。
//
// 返回 0 表示**上游没有给出可信值**（调用方保留上一次的周期，不要清零）。
//
// # 为什么收成一个函数类型，而不是直接用 *qoder.Client
//
// `qoder.Client.userStatusBase` 是**包内未导出**字段（仅供测试注入 httptest
// 地址，见 client.go:33-37），`cmd/server` 的测试设不了它 —— 直接吃
// `*qoder.Client` 就只能打真上游。收窄成函数后：生产传
// `qoderBillingCycleFetcher(client)`，测试传桩。
//
// ⚠ **不要**为了测试方便去导出 `userStatusBase`（那是把测试需求泄漏成
// 公开 API，且会让"生产必须走真实区域域名"这条约束失去编译期保障）。
type billingCycleFetcher func(ctx context.Context, cr *qoder.Cred) (int64, error)

// newBillingCycleRefreshRunner 构造计费周期刷新的执行体。
//
// 返回 nil 表示"当前构建没有 Qoder 账号可刷"（回调不装，排程直接跳过）——
// 与 `newProductTasksRunner` 同一口径：单产品部署是**合法配置**，
// 不该在巡检里报错。
func newBillingCycleRefreshRunner(
	p *pool.Pool,
	fetch billingCycleFetcher,
	store *usage.Stats,
) func(ctx context.Context) {
	if p == nil || fetch == nil || store == nil {
		return nil
	}
	return func(ctx context.Context) {
		processed, updated := refreshBillingCyclesOnce(ctx, p, fetch, store)
		if processed > 0 {
			log.Printf("计费周期刷新：处理 %d 个 Qoder 账号，%d 个周期有更新", processed, updated)
		}
	}
}

// refreshBillingCyclesOnce 跑一轮；返回（处理过的账号数，周期被更新的账号数）。
//
// # 循环体的三条约定（与 scheduler.refreshCreditsWithGap 同口径）
//
//	① **严格串行**，账号之间留 billingCycleGap —— 上游对短时并发不友好
//	② 单账号失败**只记日志并 continue**：一个号取不到不该让整轮停摆，
//	   也**不写**该账号的周期状态（宁可没有数据，也不要一个假边界）
//	③ ctx 取消立即返回：网关停机时不该还在等上游
//
// # 关于 NoRoute 账号
//
// **照刷**。`NoRoute` 的语义是"只不接流量"（见 auth.NoRoute 的注释），
// 养号任务照跑；周期刷新是只读轮询，同属这一族。而且用户把开关打开后，
// 我们应当已经有它的周期，而不是等到下一次巡检才有。
func refreshBillingCyclesOnce(
	ctx context.Context,
	p *pool.Pool,
	fetch billingCycleFetcher,
	store *usage.Stats,
) (processed, updated int) {
	for _, st := range p.List() {
		if ctx.Err() != nil {
			return processed, updated
		}
		a := p.AuthByUID(st.UID)
		// ⚠ 产品闸门必须是 IsQoder()（auth.go:77）。
		//
		// **不要**写成 `a.ProductOf() != auth.ProductWorkBuddy` —— 那个判据
		// 会把 ZCode 账号一起放进来，然后拿 ZCode 的凭证去打 Qoder 的端点。
		// 这种错不会报错，只会让 Qoder 的周期被 ZCode 的账号数冲淡。
		if a == nil || !a.IsQoder() {
			continue
		}
		// 间隔加在"已经处理过至少一个账号"之后：若第一个 Qoder 账号排在
		// 池里的第 3 位，前面跳过的两个不该让我们白等 300ms。
		if processed > 0 {
			select {
			case <-ctx.Done():
				return processed, updated
			case <-time.After(billingCycleGap):
			}
		}
		processed++

		cr := billingCycleCredOf(a)
		if cr == nil {
			log.Printf("billing-cycle %s: 凭证不可用于查询（缺 uid），已跳过", maskUID(st.UID))
			continue
		}
		nextMs, err := fetch(ctx, cr)
		if err != nil {
			// 错误隔离：这一行日志是唯一的痕迹，整轮继续。
			log.Printf("billing-cycle %s: %v", maskUID(st.UID), err)
			continue
		}
		// nextMs <= 0 / 不可信时 `SetBillingCycle` 内部会**拒绝并保留**
		// 上一周期（cycle.go 的四情形 ⓪）—— 这里刻意**不做二次判断**：
		// 可信度判据只能有一处，两处必然漂移。
		if store.SetBillingCycle(st.UID, nextMs) {
			updated++
		}
	}
	return processed, updated
}

// billingCycleCredOf 把池里的账号还原成 Qoder 凭证。
//
// # 为什么优先从**文件**读，而不是照抄 dispatch 的 credOf
//
// `internal/qoder/credOf`（dispatch.go:367）是包内未导出的，而且它从
// `auth.Auth` 重建时会**丢掉机器指纹** —— `auth.Auth` 根本没有那三个字段
// （grep Machine/Device/Fingerprint 无匹配）。而 `/user/status` 走的正是
// **机器身份**通道（见 userstatus.go 的文件头），`cosy-machineid` 就是从
// `Cred.MachineID` 取的（userstatus.go:252-254）—— 从池重建会让这个头
// **静默不发**。凭证文件里有完整的指纹（cred.go:144-161 读回来、
// SaveAtomic 写回去），故优先按 FilePath 重新加载。
//
// 文件读不到时（凭证是运行时注入的 / 文件被删 / 宿主正在改写）**回退**到
// 从池重建：少一个头在国服仍能查到周期（实测国服 host 不校验指纹），
// 比整轮跳过好。
func billingCycleCredOf(a *auth.Auth) *qoder.Cred {
	if a == nil || a.UID == "" {
		return nil
	}
	var cr *qoder.Cred
	if a.FilePath != "" {
		if c, err := qoder.LoadFile(a.FilePath); err == nil {
			cr = c
		}
	}
	if cr == nil {
		cr = &qoder.Cred{
			UID:         a.UID,
			Nickname:    a.Nickname,
			DT:          a.AccessToken,
			DRT:         a.RefreshToken,
			DTExpiresAt: a.ExpiresAt,
			FilePath:    a.FilePath,
			// 区域由域名判定（与加载层、dispatch.credOf 同口径）。
			Region: qoder.RegionFromDomain(a.Domain),
		}
		if cr.Region == qoder.RegionUnknown {
			// 域名认不出区域时按国服（endpointsOf 的默认行为），
			// 但**不写回** Domain —— 让用户能在界面上修正。
			cr.Region = qoder.RegionCN
		}
	}
	// ⚠ 强制把 uid 对齐到**池里的键**。
	//
	// `/user/status` 的 `userId` 与 `usage.SetBillingCycle` 的 uid 必须是
	// 同一个值（见文件头的 UID 同源说明）。文件里的 uid 与池键不同只可能
	// 来自手改或重复导入，此时以池键为准 —— 否则会"查 A 账号的周期、
	// 记在 B 账号名下"，而那种错**不会报错**，只会让两个账号的数据都不对。
	cr.UID = a.UID
	return cr
}

// qoderBillingCycleFetcher 把真 client 收窄成 billingCycleFetcher。
//
// ⚠ 这里**只调** `/user/status`，不调 `FetchQuota` ⇒
// `qoder.ResolveNextResetMs` 的 **Quota 兜底分支（第二个参数）在生产路径上
// 永远不会走到，仅由 userstatus_test.go 的单测覆盖**。
//
// 为什么不顺手把兜底也接上：`/api/v2/quota/usage` 的 `expiresAt` 是
// **套餐到期**，不是周期重置（无到期套餐报 9999-12-31 哨兵）。多打一个
// 端点换来一个语义不同的数字，只会让"周期"看起来更确定而实际更错 ——
// 参考实现也只把它当"nextResetAt 缺失时的兜底"。
//
// 于是这里只做**可信度净化**：`ResolveNextResetMs(st, nil)` 会在
// `nextResetAt` 不可信时返回 0，由 `SetBillingCycle` 拒绝并保留上一周期。
func qoderBillingCycleFetcher(c *qoder.Client) billingCycleFetcher {
	if c == nil {
		return nil
	}
	return func(ctx context.Context, cr *qoder.Cred) (int64, error) {
		st, err := c.FetchUserStatus(ctx, cr)
		if err != nil {
			return 0, err
		}
		return qoder.ResolveNextResetMs(st, nil), nil
	}
}
