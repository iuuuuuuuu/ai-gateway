package server

// wafip_test.go 「WAF 403 是出口 IP 问题而非账号问题」的判定断言。
//
// 本模块的存在理由：此前 403 只有账号级软冷却，所有号轮一遍全 403 时
// 网关还在继续换号，把请求放大 MaxRotate 倍 —— 恰好是 WAF 最想惩罚的行为。
// 因此下面最关键的三个用例是：同账号不触发 / 不同账号触发 / 窗口过期不触发。

import (
	"strings"
	"testing"
	"time"
)

// newTestWafIP 构造一个时钟可控的窗口状态，避免用例依赖真实时间流逝
// （那会让「窗口过期」只能用 time.Sleep 来验，既慢又不稳）。
func newTestWafIP(now *time.Time) *wafIPState {
	s := newWafIPState(wafIPWindow, wafIPThreshold)
	s.now = func() time.Time { return *now }
	return s
}

// TestWafSameAccountRepeatedDoesNotTrigger 同一账号反复 403 **不算** IP 问题。
//
// 这是本模块最容易被写错的一条：按「403 次数」计数的话，一个坏账号
// 一分钟内就能轻易打出几十次 403，从而误判「整个出口 IP 被拦」，
// 把好账号的请求也一起中止掉。判定必须按**不同账号**计数。
func TestWafSameAccountRepeatedDoesNotTrigger(t *testing.T) {
	now := time.Now()
	s := newTestWafIP(&now)

	for i := 0; i < 10; i++ {
		if s.noteWaf("uid-a") {
			t.Fatalf("第 %d 次同一账号 403 就判定 IP 被拦 —— 这是账号问题，不该升级成 IP 问题", i+1)
		}
	}
	if got := s.distinct(); got != 1 {
		t.Errorf("窗口内不同账号数 = %d，期望 1（重复 403 不该累加）", got)
	}
}

// TestWafDistinctAccountsTrigger 不同账号 403 达到阈值即判定 IP 被拦。
//
// 阈值取 2：一个账号的 403 无法区分「账号问题」与「IP 问题」，
// 两个不同账号同时 403 才能排除账号自身因素。
func TestWafDistinctAccountsTrigger(t *testing.T) {
	now := time.Now()
	s := newTestWafIP(&now)

	if s.noteWaf("uid-a") {
		t.Error("第 1 个账号 403 不该立刻判定 IP 被拦（阈值是 2 个不同账号）")
	}
	if !s.noteWaf("uid-b") {
		t.Error("第 2 个**不同**账号 403 应判定 IP 被拦")
	}
	if got := s.distinct(); got != 2 {
		t.Errorf("窗口内不同账号数 = %d，期望 2", got)
	}
}

// TestWafVerdictPersistsWithinWindow 跨过阈值后，窗口内的后续调用仍返回 true。
//
// 这条防止的放大是：判定出来了，但**下一个请求**又从 0 开始轮转
// （那时窗口里只有一个新账号的 403，计数 < 阈值）—— 于是每个请求都
// 至少多打一个账号。判定结果必须在窗口内持续有效。
func TestWafVerdictPersistsWithinWindow(t *testing.T) {
	now := time.Now()
	s := newTestWafIP(&now)

	s.noteWaf("uid-a")
	if !s.noteWaf("uid-b") {
		t.Fatal("第 2 个不同账号 403 应判定 IP 被拦")
	}
	// 窗口内继续：一个新账号的 403 也应立即判 true。
	if !s.noteWaf("uid-c") {
		t.Error("已判定 IP 被拦后，窗口内新账号 403 应继续返回 true")
	}
	// 连第一个账号再来一次，也应 true（判定在窗口内持续有效）。
	if !s.noteWaf("uid-a") {
		t.Error("已判定 IP 被拦后，窗口内重复账号 403 也应返回 true")
	}
}

// TestWafWindowExpiryResetsVerdict 窗口过期后不再判定 IP 被拦。
//
// 出口 IP 被拦是**临时**状态。若做成累计计数，一旦过线就永久成立 ——
// 之后哪怕 IP 早已解封，网关也会一直拒绝换号（表现为「账号明明恢复了
// 却怎么都不好用」）。滑动窗口让判定自动过期。
func TestWafWindowExpiryResetsVerdict(t *testing.T) {
	now := time.Now()
	s := newTestWafIP(&now)

	if s.noteWaf("uid-a") {
		t.Fatal("第 1 个账号不该判定")
	}
	// 时间推进到窗口之外：uid-a 的 403 应被清掉。
	now = now.Add(wafIPWindow + time.Second)

	if got := s.distinct(); got != 0 {
		t.Errorf("窗口过期后不同账号数 = %d，期望 0（过期条目必须被清理）", got)
	}
	// 此时只有 1 个「不同账号」在窗口内，不该判定。
	if s.noteWaf("uid-b") {
		t.Error("窗口过期后单个账号 403 不该判定 IP 被拦")
	}
	// 再凑一个不同账号才应判定 —— 证明窗口是**滑动**的而不是被永久重置。
	if !s.noteWaf("uid-c") {
		t.Error("窗口内凑够 2 个不同账号后应判定 IP 被拦")
	}
}

// TestWafWindowEvictionKeepsMemoryBounded 窗口过期后条目被真正删除。
//
// 这是内存只增不减的防线：hits 的 key 是 uid，若不清理，长时间运行的
// 网关会为每个曾经 403 过的账号永久保留一条记录。
func TestWafWindowEvictionKeepsMemoryBounded(t *testing.T) {
	now := time.Now()
	s := newTestWafIP(&now)

	for _, uid := range []string{"u1", "u2", "u3", "u4", "u5"} {
		s.noteWaf(uid)
	}
	if got := len(s.hits); got != 5 {
		t.Fatalf("清理前应有 5 条记录，实际 %d", got)
	}
	now = now.Add(wafIPWindow + time.Second)
	s.noteWaf("fresh") // 任何一次写入都会触发清理
	if got := len(s.hits); got != 1 {
		t.Errorf("窗口过期后应只剩 1 条记录（fresh），实际 %d —— 内存只增不减", got)
	}
}

// TestWafIPStateConcurrency 并发写入不出错（-race 下能真正抓到问题）。
//
// 同一进程内多个请求会同时轮转、各自独立地 403，正是它们的**并集**
// 才能说明 IP 被拦 —— 故这份状态必须并发安全。
func TestWafIPStateConcurrency(t *testing.T) {
	now := time.Now()
	s := newTestWafIP(&now)

	done := make(chan struct{})
	for i := 0; i < 8; i++ {
		go func(k int) {
			defer func() { done <- struct{}{} }()
			for j := 0; j < 50; j++ {
				s.noteWaf("uid-" + string(rune('a'+k)))
				s.distinct()
			}
		}(i)
	}
	for i := 0; i < 8; i++ {
		<-done
	}
	if got := s.distinct(); got != 8 {
		t.Errorf("并发写入后不同账号数 = %d，期望 8", got)
	}
}

// TestIsWafForbidden 判定「这次 403 是不是边缘 WAF 的拦截」。
//
// 主判据是「响应体不是上游业务 JSON 信封」：业务拒绝能解析出业务码与文案，
// 说明请求**已经到达应用层**，被拦的与该账号/该请求有关，与出口 IP 无关；
// 拿不到业务信封的 403 说明请求在应用层之前就被挡下了。
func TestIsWafForbidden(t *testing.T) {
	cases := []struct {
		name   string
		status int
		body   string
		want   bool
	}{
		// —— 业务拒绝：必须判 false，否则会把账号/套餐问题误升级成 IP 问题 ——
		{
			name:   "上游业务信封（ZCode 缺套餐）",
			status: 403,
			body:   `{"code":3101,"msg":"coding plan is required"}`,
			want:   false,
		},
		{
			name:   "上游业务信封（中文）",
			status: 403,
			body:   `{"code":403,"msg":"账号已被禁用","data":null}`,
			want:   false,
		},
		{
			name:   "OpenAI 形状业务错误",
			status: 403,
			body:   `{"error":{"message":"no permission","type":"invalid_request_error","code":"403"}}`,
			want:   false,
		},
		{
			name:   "账号被 block 的业务文案不该误判（markers 刻意不收敛的泛词）",
			status: 403,
			body:   `{"code":1001,"msg":"your account has been blocked"}`,
			want:   false,
		},
		{
			name:   "业务信封正文恰好含 wafer 子串（单词边界判定）",
			status: 403,
			body:   `{"code":1,"msg":"wafer quota exceeded"}`,
			want:   false,
		},
		{
			name:   "业务信封正文含 dwarf 子串",
			status: 403,
			body:   `{"code":1,"msg":"dwarf plan not available"}`,
			want:   false,
		},
		{
			name:   "业务信封含「账号已被拦截」（泛词刻意不收录）",
			status: 403,
			body:   `{"code":1,"msg":"账号已被拦截，请联系客服"}`,
			want:   false,
		},

		// —— 边缘拦截：判 true ——
		{
			name:   "HTML 拦截页",
			status: 403,
			body:   `<!DOCTYPE html><html><head><title>403 Forbidden</title></head></html>`,
			want:   true,
		},
		{
			name:   "Cloudflare 拦截页",
			status: 403,
			body:   `<html><body>Attention Required! | Cloudflare</body></html>`,
			want:   true,
		},
		{
			name:   "带 WAF 标识的 JSON（非信封）",
			status: 403,
			body:   `{"waf":"blocked","reason":"malicious traffic"}`,
			want:   true,
		},
		{
			name:   "信封形状但文案说 WAF（靠特征列表识别）",
			status: 403,
			body:   `{"code":403,"msg":"Request blocked by WAF"}`,
			want:   true,
		},
		{
			name:   "信封形状 + WAF 紧邻汉字（单词边界含非 ASCII）",
			status: 403,
			body:   `{"code":403,"msg":"WAF拦截了本次请求"}`,
			want:   true,
		},
		{
			name:   "中文安全策略页",
			status: 403,
			body:   `您的请求触发安全策略，访问被拦截`,
			want:   true,
		},
		{
			name:   "空体 403（应用层拒绝从不空体）",
			status: 403,
			body:   ``,
			want:   true,
		},
		{
			name:   "纯文本 403",
			status: 403,
			body:   `Forbidden`,
			want:   true,
		},
		{
			name:   "JSON 数组（非信封）",
			status: 403,
			body:   `["forbidden"]`,
			want:   true,
		},

		// —— 非 403 一律不判 ——
		{name: "401 不判", status: 401, body: `<html>unauthorized</html>`, want: false},
		{name: "429 不判", status: 429, body: `<html>too many requests</html>`, want: false},
		{name: "500 不判", status: 500, body: `<html>oops</html>`, want: false},
		{name: "200 不判", status: 200, body: ``, want: false},
	}
	for _, c := range cases {
		if got := isWafForbidden(c.status, c.body); got != c.want {
			t.Errorf("%s：isWafForbidden(%d, %q) = %v，期望 %v", c.name, c.status, c.body, got, c.want)
		}
	}
}

// TestIsBusinessEnvelope 业务信封识别的边界。
//
// 这一层判错的方向性后果不对称：
//   - 把信封判成「不是信封」→ 账号/套餐问题被升级成 IP 问题 → 停止轮转、报错给用户；
//   - 把非信封判成「是信封」→ IP 拦截被当成账号问题 → 继续放大风控（本缺陷本身）。
//
// 两者都要避免，故边界值逐个钉住。
func TestIsBusinessEnvelope(t *testing.T) {
	cases := []struct {
		body string
		want bool
	}{
		{`{"code":0,"msg":"ok"}`, true},
		{`{"code":3101}`, true},             // msg 可缺
		{`  {"code":1,"msg":"x"}  `, true},  // 前后空白
		{`{"error":{"message":"x"}}`, true}, // OpenAI 形状
		{`{"error":{"code":"403"}}`, true},  //
		{`{"error":"Forbidden"}`, false},    // error 是字符串：边缘节点写法
		{`{"waf":"blocked"}`, false},        // 无 code / error 对象
		{`{"msg":"forbidden"}`, false},      // 只有 msg：不是统一信封
		{``, false},                         // 空体
		{`   `, false},                      // 全空白
		{`<html></html>`, false},            // HTML
		{`["a"]`, false},                    // 数组
		{`"str"`, false},                    // 标量
		{`{not json`, false},                // 坏 JSON
		{`{"code":0,"msg":"ok"`, false},     // 截断的 JSON
		{`null`, false},                     //
	}
	for _, c := range cases {
		if got := isBusinessEnvelope(c.body); got != c.want {
			t.Errorf("isBusinessEnvelope(%q) = %v，期望 %v", c.body, got, c.want)
		}
	}
}

// TestContainsWordWordBoundary 单词边界判定的边界值。
//
// 这一层是防误判的最后一道：isWafForbidden 在 403 路径上，
// 一次误判会让整个网关停止轮转，故裸 strings.Contains 不能用。
func TestContainsWordWordBoundary(t *testing.T) {
	cases := []struct {
		s    string
		word string
		want bool
	}{
		{"request blocked by waf", "waf", true},
		{"waf", "waf", true},
		{"waf blocked", "waf", true},
		{"blocked by waf.", "waf", true}, // 句号结尾 = 边界
		{"[waf]", "waf", true},           // 方括号 = 边界
		{"waf拦截了本次请求", "waf", true},      // 汉字算边界：中文不用空格分词，必须能认出
		{"由waf拦截", "waf", true},          // 同理
		{"wafer quota", "waf", false},    // 子串但不是单词
		{"dwarf plan", "waf", false},     //
		{"swift", "waf", false},          //
		{"", "waf", false},               //
		{"no marker here", "waf", false}, //
	}
	for _, c := range cases {
		if got := containsWord(c.s, c.word); got != c.want {
			t.Errorf("containsWord(%q, %q) = %v，期望 %v", c.s, c.word, got, c.want)
		}
	}
}

// TestEgressIPBlockedMessageReadable 错误文案必须说清「换号无用」与出路。
//
// 这是本缺陷修复的**用户可见**部分：此前用户只看到
// 「all accounts unavailable (cooling/disabled)」，会去查账号池 ——
// 而真正要改的是网络出口。
func TestEgressIPBlockedMessageReadable(t *testing.T) {
	msg := egressIPBlockedMessage(2)
	for _, want := range []string{"WAF", "不同账号", "出口 IP", "换号无用", "代理"} {
		if !strings.Contains(msg, want) {
			t.Errorf("文案应包含 %q，实际 %q", want, msg)
		}
	}
	if !strings.Contains(msg, "60 秒") {
		t.Errorf("文案应说明窗口长度（60 秒），实际 %q", msg)
	}
	// 数量要随实际值变化，不能写死。
	if !strings.Contains(egressIPBlockedMessage(3), "3 个") {
		t.Errorf("文案应带上实际的不同账号数，实际 %q", egressIPBlockedMessage(3))
	}
}
