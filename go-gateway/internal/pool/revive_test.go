package pool

// 复活入口（ReviveDisabled）的回归用例。
//
// 缺陷背景：disabled 会持久化进 state.json，而它此前**只有写入方没有任何清除
// 入口** —— 唯一能清冷却的 ReenableIfCredits 显式要求 `!e.disabled`。
// 于是被自动判定为死号的账号（连续 12153 session 死 / 额度冻结）在网关内
// **永远救不回来**：重新导入凭证也没用（upsertLocked 只换凭证、不碰 disabled），
// 用户唯一出路是手改 state.json。
//
// 本文件锁住三件事：
//  1. 禁用 → 复活 → 账号确实能重新被选中（缺陷本身）；
//  2. 幂等 —— 复活一个本来就启用的账号返回 false，且不改任何状态；
//  3. 边界 —— 只解系统判定位，**不动**用户手工轴（NoRoute）与冷却/熔断。

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"workbuddy2api/internal/auth"
)

// newRevivePool 建一个只含单个账号的池（禁用由调用方制造）。
func newRevivePool(t *testing.T, uid string) *Pool {
	t.Helper()
	p := New("")
	p.SetRandomSource(func(n int64) int64 { return 0 })
	p.Add(&auth.Auth{UID: uid, AccessToken: "tok", RefreshToken: "rt", ExpiresAt: 9999999999})
	return p
}

// TestReviveDisabledReturnsAccountToPool 缺陷主路径：禁用 → 复活 → 能重新被选中。
func TestReviveDisabledReturnsAccountToPool(t *testing.T) {
	p := newRevivePool(t, "u1")
	p.Disable("u1", "12153 session dead")

	// 前置：禁用账号绝不参与选号（含全冷却兜底）。
	if got := p.Pick(); got != nil {
		t.Fatalf("前置：禁用账号不该被选中，实际 %+v", got)
	}

	if !p.ReviveDisabled("u1") {
		t.Fatal("复活一个已禁用账号应返回 true")
	}

	st, ok := p.Status("u1")
	if !ok {
		t.Fatal("账号不该从池中消失（复活不是删除）")
	}
	if st.Disabled {
		t.Errorf("复活后 disabled 应为 false: %+v", st)
	}
	if st.Reason != "" {
		t.Errorf("复活后 reason 应为空（否则界面上仍显示旧的死号原因）: %q", st.Reason)
	}

	got := p.Pick()
	if got == nil || got.UID != "u1" {
		t.Fatalf("复活后账号应重新被选中，实际 %+v", got)
	}
}

// TestReviveDisabledIsIdempotent 复活一个本来就启用的账号返回 false，且是空操作。
//
// 「返回是否真的改了状态」是端点层区分「复活成功」与「本来就没禁用」的唯一依据，
// 不能只靠读 Status 反推（那样两次请求的响应会完全一样，用户无法判断哪次生效了）。
func TestReviveDisabledIsIdempotent(t *testing.T) {
	p := newRevivePool(t, "u1")
	p.SetCredits("u1", 42)

	if p.ReviveDisabled("u1") {
		t.Fatal("账号本就启用，复活应返回 false")
	}

	// 状态逐字段不变：不能顺手把 credits / 统计清掉。
	st, _ := p.Status("u1")
	if st.Disabled || st.Reason != "" || st.Credits != 42 {
		t.Errorf("空操作复活改动了状态: %+v", st)
	}
}

// TestReviveDisabledUnknownUID 未知 uid 是空操作，返回 false 且不 panic。
func TestReviveDisabledUnknownUID(t *testing.T) {
	p := newRevivePool(t, "u1")
	if p.ReviveDisabled("nope") {
		t.Error("未知 uid 应返回 false")
	}
}

// TestReviveDisabledResetsSessionDeadCounter 复活同时复位连续 12153 计数。
//
// 为什么必须清：计数是「连续」语义。复活后若残留计数，账号只需再撞一次 12153
// 就被立刻禁用 —— 用户会觉得「刚复活就又死了」，而它其实是被**复活前的**
// 抖动计数杀掉的。
func TestReviveDisabledResetsSessionDeadCounter(t *testing.T) {
	p := newRevivePool(t, "u1")
	n := SessionDeadThreshold()

	// 制造「计了 n-1 次 + 被禁用」的状态：先计满禁用，再手工计一次。
	for i := 0; i < n; i++ {
		p.NoteSessionDead("u1")
	}
	if st, _ := p.Status("u1"); !st.Disabled {
		t.Fatal("前置：连续 12153 达阈值应已禁用")
	}
	p.NoteSessionDead("u1") // 禁用后再计一次（残留计数）
	if p.SessionDeadFails("u1") == 0 {
		t.Fatal("前置：应有残留计数")
	}

	p.ReviveDisabled("u1")
	if got := p.SessionDeadFails("u1"); got != 0 {
		t.Fatalf("复活后计数应清零，实际 %d", got)
	}

	// 复活后必须重新累计满 n 次才禁用（证明确实是从新计数）。
	for i := 1; i < n; i++ {
		if p.NoteSessionDead("u1") {
			t.Fatalf("复活后第 %d 次（阈值 %d）不该禁用", i, n)
		}
	}
	if !p.NoteSessionDead("u1") {
		t.Fatalf("复活后重新累计满 %d 次应禁用", n)
	}
}

// TestReviveDisabledKeepsNoRoute 复活**不动**用户的手工轴（NoRoute）。
//
// 这是本方法与「无条件清禁用位」的关键区别：NoRoute 是用户在界面上拨的
// 「停止接流量」开关，由宿主写进凭证文件，属用户意图而非池运行态。
// 若复活顺手清了它，一次「复活」就会把用户明确摘除的号悄悄放回流量池 ——
// 而这个错误在界面上完全看不出来（账号状态显示正常，只是开始偷偷接流量）。
func TestReviveDisabledKeepsNoRoute(t *testing.T) {
	p := New("")
	p.SetRandomSource(func(n int64) int64 { return 0 })
	p.Add(&auth.Auth{
		UID: "u1", AccessToken: "tok", ExpiresAt: 9999999999,
		NoRoute: true, // 用户手动「停止接流量」
	})
	p.Disable("u1", "12153 session dead") // 同时被系统判死

	if !p.ReviveDisabled("u1") {
		t.Fatal("前置：应复活成功（系统判定位确实被清了）")
	}

	st, _ := p.Status("u1")
	if st.Disabled {
		t.Errorf("系统禁用位应被清除: %+v", st)
	}
	if !st.NoRoute {
		t.Error("复活不得清除用户手动轴 NoRoute")
	}
	// 用户轴仍在 → 账号依旧不接流量，这正是用户要的。
	if got := p.Pick(); got != nil {
		t.Fatalf("仍被标 no_route 的账号不该接流量，实际 %+v", got)
	}
}

// TestReviveDisabledKeepsCoolingAndBreaker 复活不清冷却与熔断。
//
// 三者是正交维度：disabled 是「网关判定这个号已死」，until 是「余额/限流冷却」，
// breakerUntil 是「连续失败熔断」。复活只负责把死号放回候选，不假装它健康 ——
// 否则一次复活就等于绕过所有惩罚，用户会觉得「复活能刷掉限流」。
func TestReviveDisabledKeepsCoolingAndBreaker(t *testing.T) {
	p := newRevivePool(t, "u1")
	p.Cooldown("u1", CoolHard, time.Hour, "余额不足")
	p.SetBreaker(2, time.Hour, 2*time.Hour)
	p.NoteError("u1")
	p.NoteError("u1") // 触发熔断
	p.Disable("u1", "12153 session dead")

	if !p.ReviveDisabled("u1") {
		t.Fatal("前置：应复活成功")
	}

	st, _ := p.Status("u1")
	if st.Disabled {
		t.Errorf("disabled 应已清除: %+v", st)
	}
	if !st.Cooling {
		t.Error("复活不得清除冷却/熔断（它们各有自己的到期路径）")
	}
	if got := p.Pick(); got != nil {
		t.Fatalf("仍在冷却/熔断的账号不该立刻接流量，实际 %+v", got)
	}
}

// TestReviveDisabledPersists 复活结果落盘（重启后不回退）。
//
// 这条最关键：disabled 是**持久化**状态。若复活只改内存而不置 dirty，
// 进程重启后账号会被 state.json 里的旧值重新判死 —— 表现为
//「复活了、能用了，一重启又死了」，用户完全无从理解。
func TestReviveDisabledPersists(t *testing.T) {
	dir := t.TempDir()
	fp := filepath.Join(dir, "state.json")

	p := New(fp)
	p.Add(&auth.Auth{UID: "u1", AccessToken: "tok"})
	p.Disable("u1", "12153 session dead")
	p.ReviveDisabled("u1")
	p.Flush() // 状态变更走 dirty 标志，落盘由 Flush / 后台 flusher 负责

	// 直接从磁盘核对：state.json 里该账号的 disabled 必须已是 false。
	raw, err := os.ReadFile(fp)
	if err != nil {
		t.Fatalf("state.json 未写出: %v", err)
	}
	if got := string(raw); !strings.Contains(got, `"disabled": false`) {
		t.Errorf("复活后 state.json 里应无 disabled=true，实际内容：\n%s", got)
	}

	// 重启（新建池加载同一份 state.json）后账号仍可用。
	p2 := New(fp)
	p2.Add(&auth.Auth{UID: "u1", AccessToken: "tok"})
	st, ok := p2.Status("u1")
	if !ok || st.Disabled {
		t.Fatalf("复活应跨重启保持，实际 disabled=%v ok=%v", st.Disabled, ok)
	}
	if got := p2.Pick(); got == nil || got.UID != "u1" {
		t.Fatalf("重启后账号应仍可被选中，实际 %+v", got)
	}
}

// TestDisableIsNotClearedByReimport 重新导入凭证**不解开**禁用。
//
// 这条锁住的是缺陷的另一半，也是本修复存在的理由：用户遇到死号时最自然的
// 反应是「重新导入一次凭证」。若 upsertLocked 顺手清了 disabled，本修复就
// 没有意义了；而它**确实不清**，所以必须有一个显式复活入口（ReviveDisabled）。
// 用例把「当前行为」钉住：改这条语义会立刻红，提醒改动人同时更新复活路径。
func TestDisableIsNotClearedByReimport(t *testing.T) {
	p := newRevivePool(t, "u1")
	p.Disable("u1", "12153 session dead")

	// 重新导入同一个账号的新凭证（模拟宿主重新导出）。
	p.Add(&auth.Auth{UID: "u1", AccessToken: "tok2", RefreshToken: "rt2", ExpiresAt: 9999999999})

	st, _ := p.Status("u1")
	if !st.Disabled {
		t.Error("重新导入凭证不应解开系统禁用（禁用只有 ReviveDisabled 一个出口）")
	}
	if got := p.Pick(); got != nil {
		t.Fatalf("仍被禁用的账号不该被选中，实际 %+v", got)
	}

	// 唯一的恢复路径仍然是显式复活。
	if !p.ReviveDisabled("u1") {
		t.Fatal("显式复活应返回 true")
	}
	if got := p.Pick(); got == nil || got.UID != "u1" {
		t.Fatalf("复活后应可选中，实际 %+v", got)
	}
}
