package zcode

// F-07：把验证码求解器回传的 verifyCode（F001 / T001 / T002）翻成**可操作的提示**。
//
// # 这条链路真实的样子（写测试前先对齐）
//
//	assets/zcode-captcha/solver.js   handleCaptchaResult
//	  → stderr:  verify rejected: {"verifyCode":"F001","certifyId":"…"}
//	宿主 src-tauri/src/captcha_webview.rs   fail / onError
//	  → HTTP 502 {"error":"验证失败: {\"verifyCode\":\"F001\",…}"}
//	captcha.go  solveOnce → solverDetail(stderr)        ← 本次改动（旧实现只取 firstLine）
//	client.go   StreamChat → appendCaptchaNote(reason)  ← 唯一生产接线点（client.go:342）
//	errors.go   AnnotateRiskCode(reason)                ← 本次改动
//	          → 用户看到的 error.message
//
// # ⚠ 为什么端到端（真起 Node 子进程 / 真起宿主窗口）不在本文件里
//
// 求解器是**进程级单例**（`SharedCaptchaSolver`），而 `SetExternalSolver`
// 会连带创建 param 池并 `go refillAsync()`，且**没有反注册接口**
//（`SetExternalSolver("","")` 只清 URL，池仍非 nil）。一旦在本包测试里碰它，
// 后续所有 `Solve()` 都会走 `pool.takeOrWait(…, captchaWaitForRefill = 25s)`，
// 把整包测试拖慢、并制造测试间的顺序依赖 —— `logincli_warm_test.go:110-154`
// 记着这个坑是怎么被踩到的。
//
// 故这里**直接钉住真实接线点** `appendCaptchaNote`，并用 `solverDetail` 的
// **真实产物**构造 reason：除「起子进程」那一步外，整条链路都在。

import (
	"encoding/json"
	"fmt"
	"strings"
	"testing"
)

// mockSolverStderr 求解器失败时 stderr 的**实测形态**（`solver.js` 的
// `handleCaptchaResult`：第一行是启动日志，**风控码在第二行**）。
//
// certifyId 用合成值：本仓库是公开的，不放真实设备/会话标识。
const mockSolverStderr = `[pe-init] aliyun captcha sdk booted
verify rejected: {"verifyCode":"F001","certifyId":"mock-certify-id"}`

// TestRiskCodeHintIsActionableForEveryKnownCode 收录的每个码都必须给出**动作**。
//
// 判据不是「文案非空」—— 那太弱（「未知风控码」也能过）。这里要求文案里出现
// 至少一个**可执行动作**的关键词，否则用户看完还是不知道该干什么。
func TestRiskCodeHintIsActionableForEveryKnownCode(t *testing.T) {
	cases := []struct {
		code     string
		wantWord []string
	}{
		// F001 的出路是换出口 IP ⇒ 必须提到「代理」「住宅」。
		{RiskEnvRejected, []string{"代理", "住宅"}},
		// T001/T002 的出路是「再发一次请求」⇒ 必须提到「下一次请求」。
		//
		// ⚠ 这里刻意**不**接受「自动刷新」这类说法：本项目没有
		// 「检测到 param 失效就主动作废缓存」的动作（全文件 grep `invalidate`
		// 零命中），真实语义就是「下一次请求会重新求解一次」。
		// 参考实现（zcode-yourhoneypomelo）的原话是「网关会自动刷新重试」，
		// 照抄就是对用户说满了 —— 他会以为不用管，而实际需要再发一次请求。
		{RiskVerifyExpired, []string{"下一次请求"}},
		{RiskVerifyUsed, []string{"下一次请求"}},
	}
	for _, tc := range cases {
		hint := RiskCodeHint(tc.code)
		if hint == "" {
			t.Errorf("%s 应有提示文案，实际为空（用户会只看到一串 verifyCode）", tc.code)
			continue
		}
		for _, w := range tc.wantWord {
			if !strings.Contains(hint, w) {
				t.Errorf("%s 的提示必须提到 %q（否则不可操作）：%s", tc.code, w, hint)
			}
		}
	}
}

// TestRiskCodeHintIsCaseAndSpaceTolerant 求解器/宿主的大小写不保证一致。
func TestRiskCodeHintIsCaseAndSpaceTolerant(t *testing.T) {
	for _, in := range []string{"f001", " F001 ", "F001\n", "t001"} {
		if RiskCodeHint(in) == "" {
			t.Errorf("RiskCodeHint(%q) 不该为空（大小写/空白应被容忍）", in)
		}
	}
}

// TestRiskCodeHintUnknownIsEmpty 未收录的码返回空串是**刻意的**契约。
//
// 调用方（`AnnotateRiskCode`）据此回退到原文。若这里改成返回一句
// 「未知风控码」，用户既没得到提示、又丢掉了原文里的唯一线索。
func TestRiskCodeHintUnknownIsEmpty(t *testing.T) {
	for _, in := range []string{"F008", "T100", "", "3012", "F0012", "hello"} {
		if got := RiskCodeHint(in); got != "" {
			t.Errorf("RiskCodeHint(%q) 应为空串（未收录），实际 %q", in, got)
		}
	}
}

// TestRiskCodeOfExtractsOnlyKnownCodes 提取规则：**只认已收录的码**。
func TestRiskCodeOfExtractsOnlyKnownCodes(t *testing.T) {
	cases := []struct {
		name string
		in   string
		want string
	}{
		{"空输入", "", ""},
		{"带键名的 JSON 形状", `verify rejected: {"verifyCode":"F001","certifyId":"x"}`, RiskEnvRejected},
		{"带键名但有空格", `{"verifyCode" : "T001"}`, RiskVerifyExpired},
		{"裸码", `solve failed: T002 something`, RiskVerifyUsed},
		{"裸码小写", `solve failed: f001`, RiskEnvRejected},
		// 宿主 502 的 error 字段是被 JSON.stringify 过的，码带着反斜杠转义。
		{"宿主 502 的转义形状", `宿主求解失败（HTTP 502）：验证失败: {\"verifyCode\":\"T001\"}`, RiskVerifyExpired},
		// ⚠ 未收录的码不能当提示（「风控拒绝(F008)」对用户零信息量）。
		{"未收录的 F 码", `{"verifyCode":"F008"}`, ""},
		{"未收录的 T 码", `solve failed: T100`, ""},
		// ⚠ 边界：`F0012` 不是 `F001`（与 server 层 hasBareCode 防 30120 同一教训）。
		{"四位数字不是三位码", `solve failed: F0012`, ""},
		// 上游 3012 是数字业务码，不是本表的 verifyCode。
		{"上游数字业务码不参与", `{"code":3012,"msg":"unusual activity"}`, ""},
		// 混在一起时取**已收录**的那个，而不是第一个出现的。
		{"跳过未收录取已收录", `F008 then T001`, RiskVerifyExpired},
		{"JSON 形状优先于裸码", `{"verifyCode":"T002"} 后面还有 F001`, RiskVerifyUsed},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if got := RiskCodeOf(tc.in); got != tc.want {
				t.Errorf("RiskCodeOf(%q) = %q，期望 %q", tc.in, got, tc.want)
			}
		})
	}
}

// TestAnnotateRiskCodeRecognizesSolverFailure 认得出码 ⇒ 前置可操作提示。
func TestAnnotateRiskCodeRecognizesSolverFailure(t *testing.T) {
	out := AnnotateRiskCode(mockSolverStderr)

	if !strings.HasPrefix(out, "风控拒绝(F001)：") {
		t.Errorf("应以「风控拒绝(F001)：」开头，实际 %q", out)
	}
	if !strings.Contains(out, RiskCodeHint(RiskEnvRejected)) {
		t.Error("应带上 F001 的提示全文（用户要的是「该干什么」，不是码本身）")
	}
	if !strings.Contains(out, "原始信息：") {
		t.Error("应保留「原始信息：」段（排障要靠它）")
	}
	// 原文必须还在 —— 提示是**附加**，不是替换。
	for _, keep := range []string{"verify rejected", "mock-certify-id"} {
		if !strings.Contains(out, keep) {
			t.Errorf("原文片段 %q 必须保留在「原始信息：」里，实际 %q", keep, out)
		}
	}
}

// TestAnnotateRiskCodeTruncatesOnlyWhenCodeRecognized 认得出码时才截断。
func TestAnnotateRiskCodeTruncatesOnlyWhenCodeRecognized(t *testing.T) {
	long := mockSolverStderr + strings.Repeat(" 填充", 400)
	out := AnnotateRiskCode(long)

	if !strings.HasSuffix(out, "…") {
		t.Error("认出码后原文退居次要，应被截断（避免把整页 SDK 日志塞进响应）")
	}
	i := strings.Index(out, "原始信息：")
	if i < 0 {
		t.Fatalf("缺「原始信息：」段：%q", out)
	}
	tail := out[i+len("原始信息："):]
	// 截断按 **rune** 数（`truncateStr`），不是按字节 —— 按字节会把中文切碎。
	if n := len([]rune(tail)); n > riskRawLimit+1 {
		t.Errorf("截断后应不超过 %d 个 rune（含省略号），实际 %d", riskRawLimit+1, n)
	}
}

// TestAnnotateRiskCodeFallsBackVerbatim 回退契约：认不出码 ⇒ **逐字**返回。
//
// 这是本次改动的验收点之一。回退路径上的原文往往正是唯一有用的线索
// （如「求解器组件未安装」）—— 截断它等于把可诊断的信息砍掉。
func TestAnnotateRiskCodeFallsBackVerbatim(t *testing.T) {
	// ① 生产上最常见的那句：求解器组件未安装。
	const productionReason = "求解器不可用：求解器组件未安装（发行包应包含 assets/zcode-captcha）"
	if got := AnnotateRiskCode(productionReason); got != productionReason {
		t.Errorf("认不出码时必须逐字返回：\n  期望 %q\n  实际 %q", productionReason, got)
	}
	// ② 超长且无码的原文：**不得**被截断（与上一条相反的方向）。
	long := strings.Repeat("求解器组件未安装（发行包应包含 assets/zcode-captcha）；", 20)
	if len([]rune(long)) <= riskRawLimit {
		t.Fatalf("测试数据太短（%d 个 rune），测不到「不截断」这条契约", len([]rune(long)))
	}
	if got := AnnotateRiskCode(long); got != long {
		t.Errorf("无码时不该截断：原文 %d 个 rune，返回 %d 个 rune",
			len([]rune(long)), len([]rune(got)))
	}
	// ③ 空白与空串的边界。
	if got := AnnotateRiskCode("   "); got != "" {
		t.Errorf("纯空白应返回空串，实际 %q", got)
	}
	if got := AnnotateRiskCode(""); got != "" {
		t.Errorf("空串应返回空串，实际 %q", got)
	}
}

// TestSolverDetailJoinsStderrLinesAndTruncates 本次改动：不再只取第一行。
//
// 旧实现取 `firstLine(stderr)` ⇒ 失败原因（在后面的行里）**全被丢掉**，
// 用户只看到一句 `exit status 4`，而可操作的线索就在被丢掉的那段里。
func TestSolverDetailJoinsStderrLinesAndTruncates(t *testing.T) {
	cases := []struct {
		name string
		in   string
		want string
	}{
		{"跳过空行", "a\n\n  b  \nc", "a b c"},
		{"只有空行", "\n \n\t\n", ""},
		{"空输入", "", ""},
		{"单行原样", "verify rejected: F001", "verify rejected: F001"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if got := solverDetail(tc.in); got != tc.want {
				t.Errorf("solverDetail(%q) = %q，期望 %q", tc.in, got, tc.want)
			}
		})
	}

	// 实测形态：第一行是启动日志，**风控码在第二行**。
	got := solverDetail(mockSolverStderr)
	if !strings.Contains(got, "F001") {
		t.Fatalf("求解器报的风控码必须被保留（旧实现取 firstLine 会丢掉它）：%q", got)
	}
	if !strings.Contains(got, "[pe-init]") {
		t.Error("第一行也该保留（诊断上下文）")
	}

	// 超长 stderr 必须被截断（不能把整页 SDK 调试输出塞进 HTTP 响应）。
	huge := solverDetail(strings.Repeat("sdk debug line\n", 200))
	if n := len([]rune(huge)); n > riskRawLimit+1 {
		t.Errorf("应截断到 %d 个 rune 以内（含省略号），实际 %d", riskRawLimit+1, n)
	}
}

// TestCaptchaNoteCarriesHintFromSolverStderr 端到端的**接线**验证。
//
// 复刻 `client.go:334-343` 的真实顺序：
//
//	stderr → solverDetail → fmt.Errorf("验证码求解失败：%s") → appendCaptchaNote
//
// 与生产唯一的差别是「谁来起 Node 子进程」。断言最终给用户看的
// `error.message` 里出现了可操作提示，而不仅仅是那串 verifyCode。
func TestCaptchaNoteCarriesHintFromSolverStderr(t *testing.T) {
	upstream := NormalizeErrorBody([]byte(`{"code":3007,"msg":"captcha verify failed"}`), 400)
	reason := fmt.Errorf("验证码求解失败：%s", solverDetail(mockSolverStderr)).Error()

	body := appendCaptchaNote(upstream, reason)

	var m map[string]any
	if err := json.Unmarshal(body, &m); err != nil {
		t.Fatalf("附注后必须是合法 JSON（旧实现往 JSON 后拼文本 ⇒ 用户只看到「未知错误」）：%s", body)
	}
	e, _ := m["error"].(map[string]any)
	msg, _ := e["message"].(string)

	if !strings.Contains(msg, "风控拒绝(F001)") {
		t.Errorf("用户看到的文案里应出现可操作提示，实际 %q", msg)
	}
	if !strings.Contains(msg, RiskCodeHint(RiskEnvRejected)) {
		t.Error("提示全文应透出（只说「风控拒绝(F001)」用户仍然不知道该干什么）")
	}
	if !strings.Contains(msg, "certifyId") {
		t.Error("原始信息应保留（排障需要）")
	}
	// 既有契约不能破：上游原文 + 附注标记。
	if !strings.Contains(msg, "captcha verify failed") {
		t.Errorf("上游原文必须保留，实际 %q", msg)
	}
	if !strings.Contains(msg, "网关附注") || !strings.Contains(msg, "人机验证") {
		t.Errorf("应保留「网关附注」「人机验证」，实际 %q", msg)
	}
	// 数字业务码 3007 必须还在（server 层判据的唯一硬依托）。
	if code, _ := e["code"].(float64); int(code) != 3007 {
		t.Errorf("error.code 应保留 3007，实际 %#v", e["code"])
	}
}

// TestCaptchaNoteKeepsUnavailableReasonVerbatim 认不出码时**一个字节都不改**。
//
// 生产上最常见的就是这一条：求解器组件未安装。此时 reason 里没有任何
// verifyCode，提示表**不该**插手 —— 用户需要的是「去装组件」这条信息。
func TestCaptchaNoteKeepsUnavailableReasonVerbatim(t *testing.T) {
	const reason = "求解器不可用：求解器组件未安装（发行包应包含 assets/zcode-captcha）"

	// 形状 ①：上游是 OpenAI 形状（`NormalizeErrorBody` 的产物）。
	body := appendCaptchaNote(
		NormalizeErrorBody([]byte(`{"code":3007,"msg":"captcha verify failed"}`), 400), reason)
	var m map[string]any
	if err := json.Unmarshal(body, &m); err != nil {
		t.Fatalf("必须是合法 JSON：%s", body)
	}
	e, _ := m["error"].(map[string]any)
	msg, _ := e["message"].(string)
	if !strings.Contains(msg, reason) {
		t.Errorf("认不出码时原因必须逐字保留，实际 %q", msg)
	}
	if strings.Contains(msg, "风控拒绝") {
		t.Error("认不出码时不该硬加「风控拒绝」前缀（那是无信息量的噪音）")
	}

	// 形状 ③：上游不是 JSON（HTML 错误页）⇒ 包成 JSON 再附注，原文仍要保留。
	body = appendCaptchaNote([]byte("<html><body>502 Bad Gateway</body></html>"), reason)
	if err := json.Unmarshal(body, &m); err != nil {
		t.Fatalf("非 JSON 上游体也必须被包成合法 JSON：%s", body)
	}
	e, _ = m["error"].(map[string]any)
	msg, _ = e["message"].(string)
	if !strings.Contains(msg, "上游原始响应：") || !strings.Contains(msg, "502 Bad Gateway") {
		t.Errorf("非 JSON 上游体的原文必须保留，实际 %q", msg)
	}
	if !strings.Contains(msg, reason) {
		t.Errorf("原因必须保留，实际 %q", msg)
	}
}
