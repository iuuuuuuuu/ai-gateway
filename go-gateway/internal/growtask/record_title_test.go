package growtask

// record_title_test.go —— 成长任务记录的**粒度**回归测试（2026-09-22）。
//
// # 所有者现场
//
//	「任务执行记录我看也没有,这些任务执行都是需要记录的」
//
// # 查证结论：记录**一直在写**，是粒度太粗
//
// 18 项成长任务此前**共用同一个标题**：
//
//	const recordsTitle = "成长任务"   // ← 旧实现
//
// 而 `records.TaskDaily` 的去重键是「**账号 + 标题 + 结果**，每天最多一条」。
// 于是同一天跑 5 项，记录里**只剩 1 条「成长任务」**，
// 用户完全看不出哪几项做了什么 —— 他把这读成了"没有记录"。
//
// 功能没坏，是"多项被压成一条"。故修复 = 标题按项区分。
//
// # 这组测试钉住什么
//
// 直接测 `growthRecordTitle`：它是**唯一的标题来源**，
// 而且不依赖 Runner / 上游 / 网络，是最小可测单元。
//
// ⚠ 为什么不去测 `RunOne` 的端到端写盘：那条路要造完整的
// fake upstream（列表 + 报名 + 动作 + 回读），测试会很长很脆，
// 而它真正要保护的**只是标题这一件事**。测在最小单元上更稳。

import (
	"strings"
	"testing"

	"workbuddy2api/internal/upstream"
)

// TestGrowthRecordTitleUsesUpstreamTitle 有上游标题时用它。
func TestGrowthRecordTitleUsesUpstreamTitle(t *testing.T) {
	got := growthRecordTitle(&upstream.GrowthTask{Title: "夜猫子任务"}, "black_cat")
	if got != "夜猫子任务" {
		t.Errorf("有上游标题时应优先用它，实际 %q", got)
	}
}

// TestGrowthRecordTitleFallsBackToCode 没有标题时回落**任务码**。
//
// ⚠ 这条是本次修复的核心：回落值必须是**能区分项**的东西。
// 若回落成笼统的"成长任务"，去重又会把多项合成一条 —— 正是本 bug。
func TestGrowthRecordTitleFallsBackToCode(t *testing.T) {
	for _, code := range []string{"black_cat", "RichMeow_Chat", "expert_5"} {
		got := growthRecordTitle(&upstream.GrowthTask{Title: ""}, code)
		if got != code {
			t.Errorf("无标题时应回落任务码 %q，实际 %q", code, got)
		}
		if strings.TrimSpace(got) == "" {
			t.Errorf("任务码 %q 回落出了空标题 —— 空标题会让记录无法区分项", code)
		}
	}
}

// TestGrowthRecordTitleNeverCollapsesToSharedTitle **不同任务必须得到不同标题**。
//
// 这是对"记录看不见"这个缺陷最直接的断言：把多项的标题收集起来，
// 去重后数量必须与项数一致。若有人把实现改回共用一个常量，这条立刻红。
func TestGrowthRecordTitleNeverCollapsesToSharedTitle(t *testing.T) {
	// 取几个真实的、语义完全不同的任务码
	tasks := []struct {
		code string
		task *upstream.GrowthTask
	}{
		{"black_cat", &upstream.GrowthTask{Title: ""}},
		{"RichMeow_Chat", &upstream.GrowthTask{Title: ""}},
		{"expert_5", &upstream.GrowthTask{Title: ""}},
		{"skill_1", &upstream.GrowthTask{Title: ""}},
		{"template_5", &upstream.GrowthTask{Title: ""}},
	}

	seen := map[string]string{} // 标题 → 首个用了它的任务码
	for _, tc := range tasks {
		title := growthRecordTitle(tc.task, tc.code)
		if prev, dup := seen[title]; dup {
			t.Errorf("任务 %q 与 %q 得到**同一个标题** %q ——\n"+
				"TaskDaily 的去重键含标题，同一天里这两项只会留下**一条**记录，\n"+
				"用户看不到另一项做了什么（这正是所有者反馈的「记录没有」）。",
				prev, tc.code, title)
		}
		seen[title] = tc.code
	}

	if len(seen) != len(tasks) {
		t.Errorf("%d 项任务只产生了 %d 个不同标题 —— 记录会被去重压掉",
			len(tasks), len(seen))
	}
}

// TestGrowthRecordTitleHandlesNilTask nil 任务不能 panic。
//
// `RunOne` 在某些分支上可能拿不到任务对象（如 `refetch` 失败后回落到
// 只有 code 的路径）。此处防御的是**崩溃**，不是业务语义。
func TestGrowthRecordTitleHandlesNilTask(t *testing.T) {
	defer func() {
		if r := recover(); r != nil {
			t.Errorf("growthRecordTitle(nil, code) 不应 panic：%v", r)
		}
	}()
	if got := growthRecordTitle(nil, "black_cat"); got != "black_cat" {
		t.Errorf("nil 任务应回落任务码，实际 %q", got)
	}
}
