package upstream

import (
	"io"
	"strings"
	"testing"
	"time"
)

// ---------------------------------------------------------------------------
// 探测口径必须**真的**与 Stream 一致（这是所有者现场报的缺陷）
//
// # 缺陷经过（2026-09-20，装新版后仍报 `empty upstream stream`）
//
// `upstream/sse.go` 里早就有 `ValidSSEFrame`，注释也写明"与 Stream 的计数
// 口径一致，否则会自相矛盾"。**但 `ProbeFirstFrame` 从来没用它** ——
// goroutine 里写的是 `strings.HasPrefix(t, "data:")`，只看前缀。
//
// 于是判据分叉：
//
//	probe  : `data:` 前缀即算"有帧"    → 放行
//	Stream : 要求 JSON 能解析成对象     → 0 帧 → 写 `empty upstream stream`
//
// 关键后果是**不可挽回**：probe 放行后 `forward.go` 已经把 HTTP 200
// 写了出去，此后即使发现流是空的也**再也无法换号重试** ——
// 用户直接看到"本轮运行失败 empty upstream stream"。
//
// # 已有的测试为什么没抓到
//
// `TestValidSSEFrameMatchesStreamCounting` 确实在测"两边口径一致"，
// 但它测的是 **`ValidSSEFrame` 这个函数**与 Stream 是否一致 ——
// 而缺陷在于 **probe 没调用那个函数**。
// 那是一个典型的「测试测了辅助函数，没测真正使用的路径」。
//
// 故本文件直接测 **`ProbeFirstFrame` 本身**：喂进各种流，断言它返回的
// `first` 是否为空 —— 那正是 `forward.go` 用来决定"要不要换号"的判据。
// ---------------------------------------------------------------------------

// TestProbeFirstFrameRejectsDoneOnlyStream 只有 `[DONE]` 的流必须被判成空。
//
// 这是本次缺陷的**最小复现**：上游返回 200 + 只有一帧 `[DONE]`。
// 旧代码返回 first=`data: [DONE]`（非空）→ probe 放行 → Stream 报空流。
// 修后必须返回 first==""，让 forward.go 走换号路径。
func TestProbeFirstFrameRejectsDoneOnlyStream(t *testing.T) {
	rc := io.NopCloser(strings.NewReader("data: [DONE]\n\n"))
	got, first, _ := ProbeFirstFrame(rc, 2*time.Second)
	_ = got.Close()

	if first != "" {
		t.Errorf("`[DONE]` 不是有效数据帧，first 必须为空（否则 probe 放行、"+
			"Stream 仍会报 empty upstream stream），实际 %q", first)
	}
}

// TestProbeFirstFrameRejectsNonJSONStream 非 JSON 的 data 行不算有效帧。
func TestProbeFirstFrameRejectsNonJSONStream(t *testing.T) {
	for _, in := range []string{
		"data: not-json\n\n",
		"data: \n\n",
		"data: <html>upstream error page</html>\n\n",
	} {
		t.Run(strings.TrimSpace(in), func(t *testing.T) {
			rc := io.NopCloser(strings.NewReader(in))
			got, first, _ := ProbeFirstFrame(rc, 2*time.Second)
			_ = got.Close()
			if first != "" {
				t.Errorf("输入 %q 不是有效帧，first 应为空，实际 %q", in, first)
			}
		})
	}
}

// TestProbeFirstFrameAcceptsRealFrame 真实的 JSON 数据帧必须被接受（不能修过头）。
func TestProbeFirstFrameAcceptsRealFrame(t *testing.T) {
	in := `data: {"choices":[{"index":0,"delta":{"content":"你"}}]}` + "\n\n"
	rc := io.NopCloser(strings.NewReader(in))
	got, first, err := ProbeFirstFrame(rc, 2*time.Second)
	if err != nil {
		t.Fatalf("有效帧不该报错: %v", err)
	}
	if first == "" {
		t.Fatal("有效 JSON 帧必须被接受 —— 否则所有请求都会被判成空流")
	}
	// ⚠ 首帧已从流里消费，必须能**原样读回**（否则回答会少开头几个字）
	rest, _ := io.ReadAll(got)
	got.Close()
	if !strings.Contains(string(rest), `"content":"你"`) {
		t.Errorf("首帧必须能从返回的流里读回（否则丢首帧），实际读到 %q", string(rest))
	}
}

// TestProbeFirstFrameSkipsInvalidThenFindsValid 跳过无效帧后能找到有效帧。
//
// 上游常见形态：先发注释/心跳，甚至夹杂非 JSON 行。
// 旧代码会把第一个 `data:` 前缀行当成"首帧"返回（哪怕它是非 JSON），
// 修后应继续读到**真正**的有效帧。
func TestProbeFirstFrameSkipsInvalidThenFindsValid(t *testing.T) {
	in := ": ping\n\n" +
		"data: not-json\n\n" +
		`data: {"choices":[{"index":0,"delta":{"content":"真"}}]}` + "\n\n"
	rc := io.NopCloser(strings.NewReader(in))
	got, first, err := ProbeFirstFrame(rc, 2*time.Second)
	if err != nil {
		t.Fatalf("应能找到有效帧，不该报错: %v", err)
	}
	if !strings.Contains(first, `"content":"真"`) {
		t.Errorf("应跳过注释与非 JSON 行、返回**有效**帧，实际返回 %q", first)
	}
	got.Close()
}

// TestProbeFirstFrameConsistentWithStreamOnTrickyInputs 端到端口径一致。
//
// 对每种输入同时跑 probe 与 Stream，断言：
//
//	probe 认（first != ""） ⇔ Stream 数出的有效帧 > 0
//
// 这比 `TestValidSSEFrameMatchesStreamCounting` 更贴缺陷 ——
// 它走的是 **probe 的真实实现**，而不是辅助函数。
func TestProbeFirstFrameConsistentWithStreamOnTrickyInputs(t *testing.T) {
	inputs := []string{
		`data: {"choices":[{"delta":{"content":"a"}}]}`,
		`data: {}`,
		`data: []`,
		`data: [DONE]`,
		`data: `,
		`data: not-json`,
		`: comment`,
	}
	for _, in := range inputs {
		t.Run(in, func(t *testing.T) {
			// probe 侧
			rc := io.NopCloser(strings.NewReader(in + "\n\n"))
			probed, first, _ := ProbeFirstFrame(rc, 2*time.Second)
			probeOK := first != ""

			// Stream 侧：用 probe 返回的流（模拟真实调用链）
			h := newFakeRW()
			_ = Stream(h, probed)
			probed.Close()
			streamEmpty := strings.Contains(h.body.String(), "empty upstream stream")

			if probeOK == streamEmpty {
				t.Errorf("口径分叉：probe 认=%v 但 Stream 判空=%v（输入 %q）——"+
					"这正是 `empty upstream stream` 不可挽回的成因",
					probeOK, streamEmpty, in)
			}
		})
	}
}

// TestProbeFirstFrameTimeoutIsCallerControlled 超时由调用方决定。
//
// 所有者配置 `header_timeout_seconds = 120`（思考型模型首帧可以很晚），
// 而旧代码在 forward.go 里**硬编码 30 秒** —— 40 秒才开口的模型会被
// 判成"空流"并换号，用户看到 `empty upstream stream` 而**上游其实正常**。
// 故 probe 必须接受调用方传入的超时。
func TestProbeFirstFrameTimeoutIsCallerControlled(t *testing.T) {
	pr, pw := io.Pipe()
	defer pw.Close()
	start := time.Now()
	got, first, err := ProbeFirstFrame(pr, 300*time.Millisecond)
	elapsed := time.Since(start)
	got.Close()

	if elapsed > 3*time.Second {
		t.Errorf("应尊重调用方传入的超时（300ms），实际等了 %v", elapsed)
	}
	if err == nil && first == "" {
		t.Error("超时应返回错误或空帧，让上层能换号")
	}
}
