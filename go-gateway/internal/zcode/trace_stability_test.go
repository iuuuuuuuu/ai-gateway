package zcode

// trace_stability_test.go 追踪头**生命周期**的单测。
//
// # 为什么单独一个文件
//
// 2026-09-20 抓包对比官方客户端**相隔 58 分钟**的两次成功请求，发现：
//
//	x-request-id      每请求**不同**   ✓
//	x-query-id        每条消息**不同** ✓
//	x-session-id      会话期**相同**   ← 我们此前每请求随机（错）
//	x-zcode-trace-id  会话期**相同**   ← 我们此前每请求随机（错）
//
// 一个"每发一条消息就换 trace-id / session-id"的客户端，在服务端看来
// 与脚本无异。这是**行为指纹**层面的差异，不影响认证，但影响风控。
//
// 故这里把"哪些该变、哪些该定"钉成断言 —— 否则后人看到"每次请求新 id"
// 的直觉，很容易把它改回随机（那个直觉对 request-id 是对的，对 trace-id 是错的）。
import (
	"encoding/json"
	"strings"
	"testing"
)

// TestTraceHeadersLifecycle 追踪头的三类生命周期必须分清。
func TestTraceHeadersLifecycle(t *testing.T) {
	id := Identity{AccountID: "zcode-1b2941c020ef"}
	a := id.TraceHeaders()
	b := id.TraceHeaders()

	// ---- 每请求新 ----
	for _, k := range []string{"x-request-id", "x-query-id"} {
		if a[k] == "" {
			t.Errorf("%q 不该为空", k)
		}
		if a[k] == b[k] {
			t.Errorf("%q 应**每次变化**（官方实测每请求/每条消息都换），"+
				"两次调用却是同一个值 %q", k, a[k])
		}
		// 必须仍是 UUID 形态（上游对非 UUID 回 429/3001）
		if !looksLikeUUID(a[k]) {
			t.Errorf("%q 应是 UUID 形态，实际 %q", k, a[k])
		}
	}

	// ---- 每会话稳定 ----
	for _, k := range []string{"x-session-id", "x-zcode-trace-id"} {
		if a[k] == "" {
			t.Errorf("%q 不该为空", k)
		}
		if a[k] != b[k] {
			t.Errorf("%q 应**会话期稳定**（官方相隔 58 分钟的两次请求该值完全相同），"+
				"两次调用却不同：%q vs %q", k, a[k], b[k])
		}
		if !looksLikeUUID(a[k]) {
			t.Errorf("%q 应是 UUID 形态，实际 %q", k, a[k])
		}
	}

	// session-type 固定值
	if a["x-zcode-session-type"] != "main" {
		t.Errorf("x-zcode-session-type 应为 main，实际 %q", a["x-zcode-session-type"])
	}
}

// TestTraceHeadersDifferPerAccount 不同账号必须得到**不同**的稳定 id。
//
// 若所有账号共用一个 session-id，上游会看到"一个会话在给所有账号发请求"
// —— 那比随机更可疑。这是把账号标识接进 Identity 的原因。
func TestTraceHeadersDifferPerAccount(t *testing.T) {
	a := Identity{AccountID: "account-a"}.TraceHeaders()
	b := Identity{AccountID: "account-b"}.TraceHeaders()

	if a["x-session-id"] == b["x-session-id"] {
		t.Errorf("不同账号的 x-session-id 不该相同（都=%q）", a["x-session-id"])
	}
	if a["x-zcode-trace-id"] == b["x-zcode-trace-id"] {
		t.Errorf("不同账号的 x-zcode-trace-id 不该相同（都=%q）", a["x-zcode-trace-id"])
	}
}

// TestTraceHeadersStableAcrossCalls 同账号跨"重启"也必须稳定。
//
// stableUUID 用哈希派生而不是内存随机，故只要账号标识相同，
// 网关重启后 **依旧** 得到同一个会话 id —— 这点很重要：
// 重启即换 session-id 同样会暴露"非真实会话"。
func TestTraceHeadersStableAcrossCalls(t *testing.T) {
	// 模拟"两次独立的进程"：各自构造 Identity，值必须一致
	x := Identity{AccountID: "same-account"}.TraceHeaders()["x-session-id"]
	y := Identity{AccountID: "same-account"}.TraceHeaders()["x-session-id"]
	if x != y {
		t.Errorf("同一账号标识必须派生出同一个会话 id（重启后也要一致），实际 %q vs %q", x, y)
	}
}

// TestTraceHeadersFallbackWithoutAccount 无账号信息时**退回随机**而不是固定串。
//
// 固定串会让所有无账号信息的请求共用一个 session-id（比随机更糟）；
// 随机至少不比旧行为差。
func TestTraceHeadersFallbackWithoutAccount(t *testing.T) {
	h := Identity{}.TraceHeaders()
	if h["x-session-id"] == "" || !looksLikeUUID(h["x-session-id"]) {
		t.Errorf("无账号信息时应回退到随机 UUID，实际 %q", h["x-session-id"])
	}
	// 两次应不同（随机）
	//
	// ⚠ 复合字面量后直接取下标在 Go 里需要括号包裹 —— 否则
	// `Identity{}.TraceHeaders()[...]` 会被解析成复合字面量的键访问而报语法错。
	if (Identity{}).TraceHeaders()["x-session-id"] == h["x-session-id"] {
		t.Error("无账号信息时两次调用应得到不同的随机 id")
	}
}

// TestTraceHeadersSessionIDWins 显式给的会话 id 优先于账号派生。
func TestTraceHeadersSessionIDWins(t *testing.T) {
	// UUID 形态的会话 id → 直接用作 x-session-id
	real := "8fc6b5b0-fb13-4801-b1de-988f41d14eed"
	h := Identity{AccountID: "acct", SessionID: real}.TraceHeaders()
	if h["x-session-id"] != real {
		t.Errorf("UUID 形态的 SessionID 应直接用作 x-session-id（官方就是这样），"+
			"实际 %q", h["x-session-id"])
	}
	// 非 UUID 的会话 id → 作为派生种子（仍要稳定且形态合法）
	h2 := Identity{SessionID: "sess-abc"}.TraceHeaders()
	if h2["x-session-id"] == "sess-abc" {
		t.Error("非 UUID 的会话 id 不该原样发出（上游对非 UUID 回 429/3001）")
	}
	if !looksLikeUUID(h2["x-session-id"]) {
		t.Errorf("派生的会话 id 应是 UUID 形态，实际 %q", h2["x-session-id"])
	}
}

// TestLooksLikeUUID 形态判定本身。
func TestLooksLikeUUID(t *testing.T) {
	yes := []string{
		"8fc6b5b0-fb13-4801-b1de-988f41d14eed",
		"5F323AC1-E8F9-4EC3-A29A-14F34CEF75A7",
	}
	no := []string{
		"",
		"zcode-1b2941c020ef",
		"19331730795565300",
		"8fc6b5b0fb134801b1de988f41d14eed",   // 无连字符
		"8fc6b5b0-fb13-4801-b1de-988f41d14ee", // 少一位
		"8fc6b5b0-fb13-4801-b1de-988f41d14eedd",
		"8fc6b5b0-fb13-4801-b1de-988f41d14eez", // 非十六进制
	}
	for _, s := range yes {
		if !looksLikeUUID(s) {
			t.Errorf("%q 应判为 UUID 形态", s)
		}
	}
	for _, s := range no {
		if looksLikeUUID(s) {
			t.Errorf("%q 不该判为 UUID 形态", s)
		}
	}
}

// TestStableUUIDIsDeterministic 派生函数的确定性与区分度。
func TestStableUUIDIsDeterministic(t *testing.T) {
	a := stableUUID("seed-1")
	if a != stableUUID("seed-1") {
		t.Error("同一 seed 必须得到同一个结果")
	}
	if a == stableUUID("seed-2") {
		t.Error("不同 seed 应得到不同结果")
	}
	if !looksLikeUUID(a) {
		t.Errorf("产物必须是 UUID 形态（上游校验），实际 %q", a)
	}
	// 版本位与变体位（v4 形态）—— 与 NewDeviceMid 一致
	if a[14] != '4' {
		t.Errorf("第 13 个十六进制位应是版本号 4（v4 形态），实际 %q（%s）", a[14], a)
	}
	if !strings.ContainsRune("89ab", rune(a[19])) {
		t.Errorf("变体位应是 8/9/a/b，实际 %q（%s）", a[19], a)
	}
}

// TestBuildAnthropicBodyCarriesMetadata metadata 必须按官方的**双重编码**形状发。
//
// 官方抓包：
//
//	"metadata":{"user_id":"{\"device_id\":\"…\",\"account_uuid\":\"\",\"session_id\":\"…\"}"}
//
// 注意 user_id 的值是**序列化后的 JSON 字符串**，不是对象。
// 写成对象上游虽可能容忍，但那与官方形状不同 —— 而我们要的就是"不可区分"。
func TestBuildAnthropicBodyCarriesMetadata(t *testing.T) {
	out, err := BuildAnthropicBodyWithMeta(
		[]byte(`{"model":"m","messages":[{"role":"user","content":"hi"}]}`),
		"GLM-5.3-Flash",
		AnthropicMeta{DeviceID: "dev-1", SessionID: "sess-1"},
	)
	if err != nil {
		t.Fatal(err)
	}
	m := decodeOut2(t, out)
	md, ok := m["metadata"].(map[string]any)
	if !ok {
		t.Fatalf("应发 metadata（官方每次对话都带），实际 %v", m["metadata"])
	}
	uid, ok := md["user_id"].(string)
	if !ok {
		t.Fatalf("metadata.user_id 必须是**字符串**（官方是双重编码的 JSON），实际 %T", md["user_id"])
	}
	for _, want := range []string{`"device_id":"dev-1"`, `"session_id":"sess-1"`, `"account_uuid":""`} {
		if !strings.Contains(uid, want) {
			t.Errorf("user_id 里应含 %s，实际 %s", want, uid)
		}
	}
}

// TestBuildAnthropicBodyOmitsMetadataWhenEmpty 取不到元数据时**不发** metadata。
//
// 发一个全空的 metadata 比不发更可疑（真实客户端不会发空设备号）。
func TestBuildAnthropicBodyOmitsMetadataWhenEmpty(t *testing.T) {
	out, err := BuildAnthropicBodyWithMeta(
		[]byte(`{"model":"m","messages":[{"role":"user","content":"hi"}]}`),
		"GLM-5.3-Flash",
		AnthropicMeta{},
	)
	if err != nil {
		t.Fatal(err)
	}
	m := decodeOut2(t, out)
	if _, has := m["metadata"]; has {
		t.Error("元数据全空时不该发 metadata")
	}
}

// decodeOut2 把翻译产物解成 map（供本文件用）。
func decodeOut2(t *testing.T, b []byte) map[string]any {
	t.Helper()
	var m map[string]any
	if err := json.Unmarshal(b, &m); err != nil {
		t.Fatalf("产物不是合法 JSON: %v", err)
	}
	return m
}
