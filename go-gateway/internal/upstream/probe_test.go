package upstream

// 「空流导致永远失败」的回归测试（2026-09-19 现场）。
//
// # 缺陷原貌
//
// 流式请求在 `status < 400` 时**直接交给调用方转写**，从不检查流里有没有
// 内容。而上游有一种失败形态是 **HTTP 200 + 空流**（实测 ZCode 的
// code=3012 "unusual activity" 就是这样返回的）。
//
// 后果有两层，第二层更严重：
//
//  1. 用户收到 `empty upstream stream`，而 200 已经写出去了，
//     **再也无法换账号重试**；
//  2. 调用方在拿到流之前已调用 `Pool.NoteSuccess` → 坏账号被记成
//     "成功"、**永不冷却** → 每次请求都选中它 → 对话**持续失败**。
//
// # 本测试盯住的能力
//
//	ProbeFirstFrame  在**写响应头之前**判断这条流值不值得交出去
//	IsUpstreamErrorFrame  识别"上游用 200 表达失败"
//	ValidSSEFrame    与 Stream 内部计数口径**一致**（否则自相矛盾）
//
// # 为什么必须有最后那条
//
// 如果 probe 说"有效"而 Stream 说"空"，用户看到的现象就是
// 「探测通过了却还是报 empty upstream stream」—— 比原缺陷更难查。

import (
	"io"
	"net/http"
	"strings"
	"testing"
	"time"
)

// closer 把字符串包成 io.ReadCloser（探针要求 ReadCloser）。
type closer struct{ io.Reader }

func (closer) Close() error { return nil }

func rc(s string) io.ReadCloser { return closer{strings.NewReader(s)} }

// fakeRW 是一个最小的 http.ResponseWriter + Flusher。
//
// # 为什么必须实现 Flusher
//
// `Stream` 里 `fl, _ := w.(http.Flusher)` —— 不实现它 fl 为 nil，
// 代码会走"不 flush"的分支。那样测试虽然能过，却**没有覆盖真实的
// flush 路径**（而 flush 失败正是流式最常见的线上问题来源）。
type fakeRW struct {
	// header 直接就是 http.Header（它底层是 map[string][]string），
	// 这样 Stream 里的 h.Set(...) 会**写进同一个 map** —— 无需 shim。
	header  http.Header
	body    *strings.Builder
	flushes int
	status  int
}

func newFakeRW() *fakeRW {
	return &fakeRW{header: http.Header{}, body: &strings.Builder{}}
}

func (f *fakeRW) Header() http.Header { return f.header }

func (f *fakeRW) Write(b []byte) (int, error) {
	if f.status == 0 {
		f.status = http.StatusOK
	}
	return f.body.Write(b)
}

func (f *fakeRW) WriteHeader(code int) { f.status = code }
func (f *fakeRW) Flush()               { f.flushes++ }

// TestProbeReturnsFirstDataFrame 正常流：拿到首帧，且**内容不丢**。
//
// ⚠ "内容不丢"是关键断言：探针从 r 里读走了一部分，若没接回去，
// 用户会发现回答**少了开头几个字** —— 一个非常难查的静默缺陷。
func TestProbeReturnsFirstDataFrame(t *testing.T) {
	body := "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n" +
		"data: {\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}\n\n" +
		"data: [DONE]\n\n"

	r, first, err := ProbeFirstFrame(rc(body), 3*time.Second)
	if err != nil {
		t.Fatalf("正常流不该报错: %v", err)
	}
	if !strings.Contains(first, "assistant") {
		t.Errorf("首帧应是第一条 data 帧，实际 %q", first)
	}

	// 最关键的断言：继续读必须能拿到**完整**内容（含首帧）
	rest, _ := io.ReadAll(r)
	_ = r.Close()
	full := first + string(rest)
	if !strings.Contains(full, "assistant") {
		t.Error("**首帧丢了** —— 探针消费后没有接回去，用户会少看到开头内容")
	}
	if !strings.Contains(full, "你好") {
		t.Error("后续帧丢了 —— 流被截断")
	}
	if !strings.Contains(full, "[DONE]") {
		t.Error("DONE 帧丢了 —— 客户端无法正常收尾")
	}
}

// TestProbeDetectsEmptyStream 空流必须被识别（这是原缺陷的核心场景）。
func TestProbeDetectsEmptyStream(t *testing.T) {
	// 上游 200 但只回了心跳/空行，没有任何 data 帧
	for _, body := range []string{"", "\n\n", ": keep-alive\n\n", "\n\n\n"} {
		r, first, err := ProbeFirstFrame(rc(body), 2*time.Second)
		_ = r.Close()
		if first != "" {
			t.Errorf("空流不该返回首帧，body=%q 得到 %q", body, first)
		}
		if err == nil {
			t.Errorf("空流应返回错误（上层据此换号），body=%q", body)
		}
	}
}

// TestProbeDetectsErrorFrameFirst 首帧就是错误帧 —— 也要被识别为失败。
//
// 上游常用 HTTP 200 + {"error":{...}} 表达失败。若不识别，
// 用户会看到「截断的回答 + 正常 [DONE]」并当成成功。
func TestProbeDetectsErrorFrameFirst(t *testing.T) {
	body := "data: {\"error\":{\"code\":3012,\"message\":\"request has been blocked due to unusual activity.\"}}\n\n"
	r, first, err := ProbeFirstFrame(rc(body), 2*time.Second)
	_ = r.Close()
	if err != nil {
		t.Fatalf("错误帧本身是可读到的首帧，不该报 IO 错: %v", err)
	}
	bad, reason := IsUpstreamErrorFrame(first)
	if !bad {
		t.Fatalf("应识别为上游错误帧，实际 first=%q", first)
	}
	if !strings.Contains(reason, "unusual activity") {
		t.Errorf("应提取出可读原因，实际 %q", reason)
	}
}

// TestIsUpstreamErrorFrame 错误帧判据的边界。
func TestIsUpstreamErrorFrame(t *testing.T) {
	cases := []struct {
		name string
		line string
		want bool
	}{
		{"错误帧", `data: {"error":{"code":3007,"message":"captcha verify failed"}}`, true},
		{"错误帧-字符串形态", `data: {"error":"boom"}`, true},
		{"正常首帧", `data: {"choices":[{"delta":{"role":"assistant"}}]}`, false},
		{"有内容就算正常（即使带 error 字段）",
			`data: {"error":{"code":1},"choices":[{"delta":{"content":"有产出"}}]}`, false},
		{"有 finish_reason 算正常",
			`data: {"error":{"code":1},"choices":[{"finish_reason":"stop"}]}`, false},
		{"DONE 不是错误", `data: [DONE]`, false},
		{"非 data 行", `: comment`, false},
		{"空 data", `data: `, false},
		{"非 JSON 不算错误帧（可能是上游噪声）", `data: not-json`, false},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			got, _ := IsUpstreamErrorFrame(c.line)
			if got != c.want {
				t.Errorf("IsUpstreamErrorFrame(%q) = %v，期望 %v", c.line, got, c.want)
			}
		})
	}
}

// TestValidSSEFrameMatchesStreamCounting **口径一致性**（防止自相矛盾）。
//
// `ProbeFirstFrame` 用 `ValidSSEFrame` 判"要不要换号"，
// `Stream` 用内部 writeFrame 的计数判"要不要报空流"。
// 两者口径若不一致，会出现「probe 说有效、Stream 说空」——
// 用户看到"探测通过却仍报 empty upstream stream"，比原缺陷更难查。
//
// 故这里把两边**同时**跑在同一个输入上，断言结论一致。
func TestValidSSEFrameMatchesStreamCounting(t *testing.T) {
	inputs := []string{
		`data: {"choices":[{"delta":{"content":"a"}}]}`,
		`data: {"choices":[]}`,
		`data: []`,
		`data: {}`,
		`data: [DONE]`,
		`data: `,
		`data: not-json`,
		`: comment`,
		`event: ping`,
	}
	for _, in := range inputs {
		t.Run(in, func(t *testing.T) {
			probeSaysValid := ValidSSEFrame(in)

			// 用 Stream 实际跑一遍，看它数了几个有效帧
			h := newFakeRW()
			_ = Stream(h, strings.NewReader(in+"\n\n"))
			// Stream 在 validFrames==0 时会写 empty upstream stream
			streamSaysEmpty := strings.Contains(h.body.String(), "empty upstream stream")

			if probeSaysValid == streamSaysEmpty {
				t.Errorf("口径不一致：ValidSSEFrame=%v 但 Stream 判空=%v（输入 %q，输出 %q）",
					probeSaysValid, streamSaysEmpty, in, h.body.String())
			}
		})
	}
}

// TestProbeFirstFrameDoesNotHangOnStall 上游"挂住不发帧"时应超时返回，
// 而不是永久阻塞（那会让用户一直转圈）。
func TestProbeFirstFrameDoesNotHangOnStall(t *testing.T) {
	// 一个永不返回数据的 reader
	pr, pw := io.Pipe()
	defer pw.Close()
	r, first, err := ProbeFirstFrame(pr, 400*time.Millisecond)
	_ = r.Close()
	if err == nil {
		t.Error("上游不发帧时应超时返回错误，而不是永久阻塞")
	}
	if first != "" {
		t.Errorf("超时不该有首帧，实际 %q", first)
	}
}

// TestProbeFirstFrameSkipsHeartbeats 心跳/注释行要被跳过，找到真正的 data 帧。
//
// 上游常先发 `: ping` 或空行保活。若把这些当成"首帧"，
// 就会把一条正常流误判为"有内容"，从而漏掉后面的错误帧。
func TestProbeFirstFrameSkipsHeartbeats(t *testing.T) {
	body := ": ping\n\n" + "\n" + "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n"
	r, first, err := ProbeFirstFrame(rc(body), 3*time.Second)
	_ = r.Close()
	if err != nil {
		t.Fatalf("不该报错: %v", err)
	}
	if !strings.Contains(first, "content") {
		t.Errorf("应跳过心跳拿到真正的 data 帧，实际 %q", first)
	}
}
