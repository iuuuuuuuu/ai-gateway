package upstream

// queue_frame_probe_test.go 用**官方日志里抄来的真实排队帧**验证现存探测逻辑。
//
// # 这组测试要回答的问题（而不是我猜）
//
// 所有者要求「需要排队就直接报错，不要等待对话」。
// 而排队时上游在 **HTTP 200** 的流首帧下发：
//
//	{"code":"403","message":"{\"code\":\"10605\",\"message\":
//	  \"{\\\"isQueued\\\":true,\\\"queueCount\\\":7309,\\\"waitTime\\\":206}\"}"}
//
// 问题是：`ProbeFirstFrame` + `IsUpstreamErrorFrame` 这套**现有**机制
// 认不认它？—— 认，就会冷却换号并回 503（即"直接报错"）；
// 不认，就会照常把流交给客户端（即"进入对话才发现错误"）。
//
// # 为什么用测试而不是真机实测
//
// 排队是**概率性**的（队列长度随时变），靠"多打几次"去凑复现
// 正是把账号打进风控的做法。而把日志原文喂进纯函数是**零风险**的，
// 且结论同样确定 —— 因为判据全在这两个函数里。
import (
	"strings"
	"testing"
	"time"
)

// officialQueueFrame 官方 Qoder CN 客户端日志里的排队帧（逐字抄）。
//
// 来源：~/.qoder-cn/logs/runs/<ts>/qodercli.log
//
//	[PayloadParser] SSE event #1: Qoder API error: statusCode=FORBIDDEN, statusCodeValue=403,
//	body(first 3000)={"code":"403","message":"{\"code\":\"10605\",\"message\":
//	  \"{\\\"isQueued\\\":true,\\\"modelKey\\\":\\\"qfmodel\\\",\\\"queueCount\\\":7309,
//	     \\\"queueType\\\":\\\"p3\\\",\\\"retryAfterSeconds\\\":30,
//	     \\\"serviceAvailable\\\":true,\\\"waitTime\\\":206}\"}"}
const officialQueueFrame = `data: {"code":"403","message":"{\"code\":\"10605\",\"message\":\"{\\\"isQueued\\\":true,\\\"modelKey\\\":\\\"qfmodel\\\",\\\"queueCount\\\":7309,\\\"queueType\\\":\\\"p3\\\",\\\"retryAfterSeconds\\\":30,\\\"serviceAvailable\\\":true,\\\"waitTime\\\":206}\"}"}` + "\n"

// TestQueueFrameRecognizedByIsUpstreamErrorFrame 排队帧必须被判成"上游错误帧"。
//
// 判成错误 ⇒ forward.go 会冷却换号 + 最终回 503（= 直接报错，不进入对话）
// 判不成   ⇒ 照常回 200，客户端进入对话后才发现（= 所有者要避免的"等待对话"）
func TestQueueFrameRecognizedByIsUpstreamErrorFrame(t *testing.T) {
	bad, reason := IsUpstreamErrorFrame(officialQueueFrame)
	if !bad {
		t.Fatalf("排队帧应被判成「上游错误帧」（这样才会直接报错而不是进入对话）；"+
			"当前判成正常帧 reason=%q\n  帧=%s", reason, officialQueueFrame)
	}
	t.Logf("✓ 判成错误帧，reason=%q", reason)
	// 原因应能带出线索（至少是 code=403）
	if reason == "" {
		t.Error("应给出可读原因（便于日志与用户提示）")
	}
}

// TestQueueFrameBlocksProbe 端到端：ProbeFirstFrame 取回首帧后，
// forward.go 那段判定逻辑会得到"该报错"的结论。
//
// 这里把 forward.go 的判定复刻成一个小函数（那三行逻辑），
// 用**同一份输入**验证结论 —— 避免"函数对但接线错"。
func TestQueueFrameBlocksProbe(t *testing.T) {
	rc := rc(officialQueueFrame + "data: [DONE]\n\n")
	probed, first, err := ProbeFirstFrame(rc, 2*time.Second)
	defer probed.Close()
	if err != nil {
		t.Fatalf("探测不该超时/报错：%v", err)
	}
	if first == "" {
		t.Fatal("应取回首帧")
	}

	// 复刻 forward.go:614-618 的判定
	var probeMsg string
	if err != nil || first == "" {
		probeMsg = "上游返回空流（无有效数据帧）"
	} else if bad, reason := IsUpstreamErrorFrame(first); bad {
		probeMsg = "上游首帧即错误：" + reason
	}
	if probeMsg == "" {
		t.Error("现有探测逻辑**没能拦住**排队帧 —— 会照常回 200 让客户端进入对话。\n" +
			"这正是所有者要求避免的「等待对话」。")
	} else {
		t.Logf("✓ 会被拦住：%s（→ 冷却换号 → 最终 503，即直接报错）", probeMsg)
	}
}

// TestNormalFramesNotJudgedAsError 正常帧**一个都不能**被判成错误帧。
//
// 这是防回归的核心：判宽了会让好账号被冷却换号，比原问题更糟。
func TestNormalFramesNotJudgedAsError(t *testing.T) {
	normal := []string{
		`data: {"choices":[{"index":0,"delta":{"role":"assistant"}}]}`,
		`data: {"choices":[{"index":0,"delta":{"content":"你好"}}]}`,
		`data: {"choices":[{"index":0,"delta":{"reasoning_content":"想"}}]}`,
		`data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}`,
		`data: {"usage":{"prompt_tokens":1,"completion_tokens":1}}`,
		`data: {"body":"{\"choices\":[{\"delta\":{\"content\":\"x\"}}]}"}`,
		`data: {"id":"x","model":"m","choices":[]}`,
	}
	for _, l := range normal {
		if bad, reason := IsUpstreamErrorFrame(l); bad {
			t.Errorf("正常帧被误判成错误（会导致好账号被冷却）：\n  帧=%s\n  reason=%s", l, reason)
		}
	}
}

// TestSuccessCodeNotJudgedAsError 成功语义的 code 不算错误。
func TestSuccessCodeNotJudgedAsError(t *testing.T) {
	for _, l := range []string{
		`data: {"code":"0","message":"ok"}`,
		`data: {"code":0,"message":"ok"}`,
		`data: {"code":"200","message":"ok"}`,
	} {
		if bad, reason := IsUpstreamErrorFrame(l); bad {
			t.Errorf("成功码不该判成错误：%s → %s", l, reason)
		}
	}
}

// TestErrorFrameWithBodyOrChoicesNotJudged 带 body/choices/usage 的帧不判坏。
//
// 即使它们顶层有 code 字段（上游加字段是常事），也不该判坏 ——
// 因为那说明这帧是内容帧，账号是好的。
func TestErrorFrameWithBodyOrChoicesNotJudged(t *testing.T) {
	for _, l := range []string{
		`data: {"code":"403","body":"{\"choices\":[{\"delta\":{\"content\":\"x\"}}]}"}`,
		`data: {"code":"403","choices":[{"delta":{"content":"x"}}]}`,
		`data: {"code":"403","usage":{"prompt_tokens":1}}`,
	} {
		if bad, reason := IsUpstreamErrorFrame(l); bad {
			t.Errorf("带内容/用量的帧不该判坏（账号是好的）：%s → %s", l, reason)
		}
	}
}

// TestExistingErrorEnvelopeStillJudged 既有的 {"error":{…}} 形状不能被破坏。
func TestExistingErrorEnvelopeStillJudged(t *testing.T) {
	bad, reason := IsUpstreamErrorFrame(`data: {"error":{"code":"3012","message":"blocked"}}`)
	if !bad {
		t.Fatal("error 包装形状仍应判坏")
	}
	if !strings.Contains(reason, "blocked") {
		t.Errorf("应带出原因，实际 %q", reason)
	}
}
