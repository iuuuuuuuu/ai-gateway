package qoder

// finish_once_test.go —— 钉住「finish_reason 只能出现一次」这个修复。
//
// # 所有者现场（2026-09-22）
//
//	「zcode没问题,qoder这不还是不行吗」
//
// 截图里内容**已经出来了**（`已思考 > 2`，模型答对），
// 但界面还在转圈，然后报：
//
//	503 {"code":"no_healthy_account",
//	     "message":"上游服务异常（HTTP 503），已切换到其他账号"}
//
// 重试 (2/5)、重试延迟 1095 毫秒。
//
// # 根因：同一条流里 finish_reason 出现两次
//
// 修复前 `fill` 的 io.EOF 分支**无条件**补一个 `finish_reason: "stop"`
// 的收尾帧；而 qoder 上游**自己也会**在最后一个内容分片里给
// `finish_reason`（第 143 行原样转发）。
//
// 于是实测：
//
//	qoder:Qwen3.8-Flash → finish_reason:stop 出现 **2 次**
//	zcode:GLM-5.3-Flash → finish_reason:stop 出现 **1 次**   ← 正常
//
// 这正好解释了所有者说的「**zcode 没问题、qoder 有问题**」——
// 它是 qoder 这条流特有的形状。
//
// # 为什么必须只出现一次
//
// OpenAI 规范里 `finish_reason` 是「本轮结束」的信号，只该出现在
// **最后一个** chunk。重复会让客户端状态机提前判定流已结束，
// 而其后还有帧（usage / 收尾）→ 客户端认为流异常。
//
// # 修复后的行为
//
//   - 上游给过 finish_reason ⇒ 收尾**只发** `[DONE]`（不再补 finish 帧）
//   - 上游**没给过** ⇒ 仍然补（原注释的教训：不补客户端会一直等）

import (
	"io"
	"strings"
	"testing"
)

// countFinishReasons 统计流里 `"finish_reason":"stop"` 出现几次。
func countFinishReasons(out string) int {
	return strings.Count(out, `"finish_reason":"stop"`)
}

// drainStream 读完整个翻译后的流。
func drainStream(t *testing.T, src string) string {
	t.Helper()
	r := NewOpenAIStream(io.NopCloser(strings.NewReader(src)), "Qwen3.8-Flash")
	defer r.Close()
	got, err := io.ReadAll(r)
	if err != nil {
		t.Fatalf("读取流失败: %v", err)
	}
	return string(got)
}

// TestFinishReasonAppearsOnceWhenUpstreamProvidesIt ★ 上游给了 finish_reason 时不再补。
//
// 这是本修复的核心断言，也是所有者「qoder 不行」的直接原因。
func TestFinishReasonAppearsOnceWhenUpstreamProvidesIt(t *testing.T) {
	// 上游形态与实测一致：最后一个内容分片带 finish_reason:stop
	upstream := strings.Join([]string{
		`data: {"body":"{\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"思考\"}}]}"}`,
		`data: {"body":"{\"choices\":[{\"index\":0,\"delta\":{\"content\":\"2\"}}]}"}`,
		`data: {"body":"{\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}"}`,
		"data: [DONE]",
	}, "\n")

	out := drainStream(t, upstream)
	if n := countFinishReasons(out); n != 1 {
		t.Fatalf(
			"`finish_reason:stop` 应只出现 **1 次**，实际 %d 次。\n\n"+
				"重复会让客户端提前判定流已结束，而其后还有帧 → 报错重试。\n"+
				"所有者现场：qoder 报 503 且界面一直转圈，而 zcode 正常 ——\n"+
				"实测正是 qoder 2 次 / zcode 1 次的差别。\n\n输出：\n%s", n, out)
	}
	if !strings.Contains(out, "[DONE]") {
		t.Error("流必须以 [DONE] 收尾（客户端靠它判断结束）")
	}
	// 内容不能因为不再补收尾帧而丢失
	if !strings.Contains(out, `"content":"2"`) {
		t.Errorf("正文应保留在流里：\n%s", out)
	}
}

// TestFinishReasonStillSynthesizedWhenUpstreamOmitsIt 上游没给时必须补。
//
// 回归保护：`finishFrame` 的原始理由是「不补的话部分客户端会一直等」
// （见其注释）。修复不能把那个场景一起弄坏。
func TestFinishReasonStillSynthesizedWhenUpstreamOmitsIt(t *testing.T) {
	// 上游只给内容，**始终没有** finish_reason
	upstream := strings.Join([]string{
		`data: {"body":"{\"choices\":[{\"index\":0,\"delta\":{\"content\":\"第一段\"}}]}"}`,
		`data: {"body":"{\"choices\":[{\"index\":0,\"delta\":{\"content\":\"第二段\"}}]}"}`,
		"data: [DONE]",
	}, "\n")

	out := drainStream(t, upstream)
	if n := countFinishReasons(out); n != 1 {
		t.Fatalf(
			"上游**没给** finish_reason 时必须补一个（否则客户端会一直等），"+
				"实际 %d 次。\n输出：\n%s", n, out)
	}
	if !strings.Contains(out, "[DONE]") {
		t.Error("流必须以 [DONE] 收尾")
	}
}

// TestEmptyFinishReasonStillTriggersSynthesis 空字符串的 finish_reason 不算「给过」。
//
// ⚠ 这个边界很重要：上游偶尔发 `"finish_reason":""` 的空帧，
// 那不是结束信号。若把它当成"给过了"而跳过补帧，
// 客户端就会**一直等**（正是 finishFrame 注释里那个老缺陷）。
func TestEmptyFinishReasonStillTriggersSynthesis(t *testing.T) {
	upstream := strings.Join([]string{
		`data: {"body":"{\"choices\":[{\"index\":0,\"delta\":{\"content\":\"内容\"}}]}"}`,
		// 空 finish_reason：不是结束信号
		`data: {"body":"{\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"\"}]}"}`,
		"data: [DONE]",
	}, "\n")

	out := drainStream(t, upstream)
	if !strings.Contains(out, `"finish_reason":"stop"`) {
		t.Fatalf(
			"上游只给了**空** finish_reason（不是结束信号）⇒ 收尾必须补一个 stop，"+
				"否则客户端会一直等。\n输出：\n%s", out)
	}
}

// TestUsageFrameStillEmittedAfterFinish usage 帧不能因为不补收尾帧而丢。
//
// 顺序是「内容帧（含 finish_reason）→ 独立 usage 帧 → [DONE]」，
// 见 stream.go 的注释：usage 必须在 finish 之后但仍要发出来，
// 否则客户端的用量面板恒显示 0。
func TestUsageFrameStillEmittedAfterFinish(t *testing.T) {
	upstream := strings.Join([]string{
		`data: {"body":"{\"choices\":[{\"index\":0,\"delta\":{\"content\":\"答\"}}]}"}`,
		`data: {"body":"{\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}"}`,
		"data: [DONE]",
	}, "\n")

	out := drainStream(t, upstream)
	if !strings.Contains(out, `"usage"`) {
		t.Errorf("usage 帧不能丢（客户端的用量面板依赖它）：\n%s", out)
	}
	if n := countFinishReasons(out); n != 1 {
		t.Errorf("finish_reason 应只 1 次，实际 %d 次：\n%s", n, out)
	}
	if !strings.Contains(out, "[DONE]") {
		t.Error("流必须以 [DONE] 收尾")
	}
}
