package qoder

// userstatus_test.go —— `/user/status` 端点调用的契约测试。
//
// 这些用例钉的是**离线可验证**的部分：请求体形状、头集合、签名算法、
// 解析判据、优先级决议。**上游是否接受这条通道**是另一回事 —— 那件事
// 只能靠真机验证（见 live_test.go 的 TestLiveUserStatusReachable），
// 单元测试证明不了（自己签自己验必然通过）。

import (
	"context"
	"crypto/md5"
	"encoding/hex"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"strings"
	"testing"
	"time"
)

// readAllForTest 读干请求体（测试桩用；被测代码里的读法不在这里断言）。
func readAllForTest(r *http.Request) string {
	raw, _ := io.ReadAll(r.Body)
	return string(raw)
}

// decodeUserStatusBody 把请求体按 `{payload, encodeVersion}` 外壳解回来。
//
// 线缆上的体是 `QoderEncode(marshalNoEscape(outer))`（参考实现
// postEncoded:635-639：先 marshal 再 Encode），所以第一步必须 QoderDecode，
// 直接把线上字节当 JSON 解只会得到一堆 `Q` 开头的乱码。
//
// ★ `payload` 字段里装的是**明文 inner JSON**，不是再编码一次的结果 ——
// 参考实现 FetchUserStatus:820-827 只把 innerJSON 塞进外壳，编码由
// postEncoded 对整个外壳做一次。所以 payload 直接 json.Unmarshal 即可；
// 对它再 QoderDecode 会报 `非法字符 '{'（位置 0）`。
//
// 顺带断言外壳形状 —— 少一层 `QoderEncode` 或把 encodeVersion 写成空串
// 都会让上游按错误的方式解包，而那种失败在真机上只表现为一个含糊的错误码。
func decodeUserStatusBody(t *testing.T, body string) map[string]any {
	t.Helper()
	outerJSON, err := QoderDecode(body)
	if err != nil {
		t.Fatalf("请求体不是合法的 QoderEncode 载荷: %v（原文 %q）", err, truncate(body, 200))
	}
	var outer map[string]any
	if err := json.Unmarshal(outerJSON, &outer); err != nil {
		t.Fatalf("解码后不是合法 JSON 外壳: %v（原文 %q）", err, truncate(string(outerJSON), 200))
	}
	if outer["encodeVersion"] != "1" {
		t.Errorf("encodeVersion = %v, 期望 \"1\"", outer["encodeVersion"])
	}
	payload, _ := outer["payload"].(string)
	if payload == "" {
		t.Fatalf("外壳里没有 payload: %q", truncate(string(outerJSON), 200))
	}
	var fields map[string]any
	if err := json.Unmarshal([]byte(payload), &fields); err != nil {
		t.Fatalf("payload 不是合法 JSON: %v（原文 %q）", err, truncate(payload, 200))
	}
	return fields
}

// TestUserStatusRequestBodyContract 请求体的**逐字段**契约。
//
// 参考实现 auth.go:725-736 的注释要求字段顺序与官方客户端一致，而签名
// 覆盖的是序列化字节 —— 但更关键的是**内容**：token 字段必须发空串
// （这条通道只认机器身份签名，带上会话令牌反而会让上游按错误通道校验）。
func TestUserStatusRequestBodyContract(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		fields := decodeUserStatusBody(t, readAllForTest(r))

		// 六个字段必须**全部存在**（不能因为 omitempty 被省掉）。
		want := []string{"userId", "personalToken", "securityOauthToken", "refreshToken", "needRefresh", "authInfo"}
		for _, k := range want {
			if _, ok := fields[k]; !ok {
				t.Errorf("请求体缺少字段 %q（参考实现要求六个字段都发出去）", k)
			}
		}
		if got := fields["userId"]; got != "uid-contract-1" {
			t.Errorf("userId = %v, 期望 uid-contract-1", got)
		}
		// 三个令牌字段必须是**空串**。
		for _, k := range []string{"personalToken", "securityOauthToken", "refreshToken"} {
			if got, _ := fields[k].(string); got != "" {
				t.Errorf("%s = %q, 期望空串（本端点靠机器身份签名，不发会话令牌）", k, got)
			}
		}
		if got := fields["needRefresh"]; got != false {
			t.Errorf("needRefresh = %v, 期望 false", got)
		}
		// authInfo 必须是 `{}` 而不是 null —— 参考实现特意发空对象。
		ai, ok := fields["authInfo"].(map[string]any)
		if !ok {
			t.Errorf("authInfo = %#v, 期望一个对象（不能是 null）", fields["authInfo"])
		} else if len(ai) != 0 {
			t.Errorf("authInfo = %#v, 期望空对象", ai)
		}

		w.WriteHeader(200)
		_, _ = w.Write([]byte(`{"userType":"personal_standard","plan":"PLAN_TIER_FREE","nextResetAt":1784014393945}`))
	}))
	defer srv.Close()

	c := New()
	c.userStatusBase = srv.URL
	st, err := c.FetchUserStatus(context.Background(), &Cred{UID: "uid-contract-1"})
	if err != nil {
		t.Fatalf("FetchUserStatus 失败: %v", err)
	}
	if st.NextResetAtMs != 1784014393945 {
		t.Errorf("NextResetAtMs = %d, 期望 1784014393945", st.NextResetAtMs)
	}
}

// TestUserStatusHeadersAndSignature 头集合与签名算法。
//
// 签名只覆盖 date，所以这里**独立重算**一遍 md5(appcode&secret&date)，
// 而不是复用被测代码的 userStatusSign —— 否则签名写错也测不出来。
func TestUserStatusHeadersAndSignature(t *testing.T) {
	var got *http.Request
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		got = r.Clone(context.Background())
		_ = readAllForTest(r)
		w.WriteHeader(200)
		_, _ = w.Write([]byte(`{"userType":"personal","nextResetAt":1}`))
	}))
	defer srv.Close()

	c := New()
	c.userStatusBase = srv.URL
	if _, err := c.FetchUserStatus(context.Background(), &Cred{UID: "u1"}); err != nil {
		t.Fatalf("FetchUserStatus 失败: %v", err)
	}
	if got == nil {
		t.Fatal("上游没有收到请求")
	}

	if got.Method != http.MethodPost {
		t.Errorf("method = %s, 期望 POST", got.Method)
	}
	if !strings.HasSuffix(got.URL.Path, "/algo/api/v3/user/status") {
		t.Errorf("path = %s, 期望 /algo/api/v3/user/status", got.URL.Path)
	}
	if got.URL.Query().Get("Encode") != "1" {
		t.Errorf("Encode = %q, 期望 \"1\"", got.URL.Query().Get("Encode"))
	}

	// 通道钥匙三件套。
	if v := got.Header.Get("appcode"); v != "cosy" {
		t.Errorf("appcode = %q, 期望 cosy", v)
	}
	date := got.Header.Get("date")
	if date == "" {
		t.Fatal("缺少 date 头（签名覆盖它，缺了必然 101）")
	}
	if _, err := time.Parse(userStatusDateLayout, date); err != nil {
		t.Errorf("date = %q 不符合 %q: %v", date, userStatusDateLayout, err)
	}
	// 独立重算签名。
	sum := md5.Sum([]byte("cosy&" + userStatusSecret() + "&" + date))
	wantSign := hex.EncodeToString(sum[:])
	if v := got.Header.Get("signature"); v != wantSign {
		t.Errorf("signature = %q, 期望 %q（md5(appcode&secret&date)）", v, wantSign)
	}
	// 其余固定头。
	for k, want := range map[string]string{
		"login-version":   "v2",
		"cosy-clienttype": "5",
		"accept":          "application/json",
		"content-type":    "application/json",
	} {
		if v := got.Header.Get(k); v != want {
			t.Errorf("%s = %q, 期望 %q", k, v, want)
		}
	}
}

// TestUserStatusSignatureUsesEnvSecret 环境变量能覆盖签名密钥。
//
// 上游换密钥时不应要求改代码重发版。
func TestUserStatusSignatureUsesEnvSecret(t *testing.T) {
	const custom = "custom-secret-for-test"
	t.Setenv(userStatusSecretEnv, custom)

	var got *http.Request
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		got = r.Clone(context.Background())
		_ = readAllForTest(r)
		w.WriteHeader(200)
		_, _ = w.Write([]byte(`{"userType":"personal","nextResetAt":1}`))
	}))
	defer srv.Close()

	c := New()
	c.userStatusBase = srv.URL
	if _, err := c.FetchUserStatus(context.Background(), &Cred{UID: "u1"}); err != nil {
		t.Fatalf("FetchUserStatus 失败: %v", err)
	}
	date := got.Header.Get("date")
	sum := md5.Sum([]byte("cosy&" + custom + "&" + date))
	if want := hex.EncodeToString(sum[:]); got.Header.Get("signature") != want {
		t.Errorf("signature 没有用环境变量里的密钥（got %q want %q）",
			got.Header.Get("signature"), want)
	}
}

// TestUserStatusNeverFabricatesMachineHeaders 源码级断言：指纹不得来自 Cred。
//
// 理由与 campaign_test.go:178-194 相同 —— `qoderclient.MachineIdentityOf`
// 是进程级 `sync.Once` 缓存，测试里没法重置；而这条约束（**不得**读
// `cr.MachineToken`）是"不伪造指纹"这个安全属性的关键，必须钉住。
//
// ★ 先剥掉注释再匹配：本文件在注释里**明写了**「这里刻意不读
// `cr.MachineToken`」来解释为什么不读它 —— 不剥注释的话，那句解释本身
// 会把断言打红（假阳性）。剥注释只影响这个断言，不影响任何生产路径。
func TestUserStatusNeverFabricatesMachineHeaders(t *testing.T) {
	src, err := os.ReadFile("userstatus.go")
	if err != nil {
		t.Fatalf("读取 userstatus.go 失败: %v", err)
	}
	code := stripGoComments(string(src))
	if strings.Contains(code, "cr.MachineToken") || strings.Contains(code, "cr.MachineType") {
		t.Error("userstatus.go 的代码里读了 cr.MachineToken / cr.MachineType —— " +
			"那两个值由 cred.go 的 EnsureFingerprint 本地编造，本端点不得使用")
	}
	if !strings.Contains(code, "qoderclient.MachineIdentityOf") {
		t.Error("userstatus.go 没有用 qoderclient.MachineIdentityOf 取真实机器指纹")
	}
}

// stripGoComments 去掉 Go 源码里的行注释与块注释，只留代码。
//
// 这是**为源码级断言服务的粗糙实现**，不追求完整的词法分析：它不区分
// 字符串字面量里的 `//`（本文件里没有那种情况）。之所以自己写而不引
// `go/parser`，是因为断言要检查的正是"这段源码文本里有没有某个标识符"，
// 走 AST 反而会把注释天然过滤掉、失去对本文件注释的可见性。
func stripGoComments(src string) string {
	var b strings.Builder
	for _, line := range strings.Split(src, "\n") {
		if i := strings.Index(line, "//"); i >= 0 {
			line = line[:i]
		}
		b.WriteString(line)
		b.WriteByte('\n')
	}
	out := b.String()
	for {
		i := strings.Index(out, "/*")
		if i < 0 {
			break
		}
		j := strings.Index(out[i:], "*/")
		if j < 0 {
			out = out[:i]
			break
		}
		out = out[:i] + out[i+j+2:]
	}
	return out
}

// TestUserStatusOmitsMachineHeadersWhenIdentityUnusable 拿不到指纹时**一个都不发**。
//
// 发空串或编造值会让上游静默降级（实测国际版 host 回 200 + 结构合法 JSON，
// 只是少一条数据）—— 那种失败极难排查，故宁可少发头。
func TestUserStatusOmitsMachineHeadersWhenIdentityUnusable(t *testing.T) {
	var got *http.Request
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		got = r.Clone(context.Background())
		_ = readAllForTest(r)
		w.WriteHeader(200)
		_, _ = w.Write([]byte(`{"userType":"personal","nextResetAt":1}`))
	}))
	defer srv.Close()

	c := New()
	c.userStatusBase = srv.URL
	if _, err := c.FetchUserStatus(context.Background(), &Cred{UID: "u1"}); err != nil {
		t.Fatalf("FetchUserStatus 失败: %v", err)
	}

	// 本机有 runtime-info.exe 时这些头会出现（且必须非空）；
	// 没有时它们必须**整体缺席**，不允许出现空串。
	tok := got.Header.Get("cosy-machinetoken")
	typ := got.Header.Get("cosy-machinetype")
	code := got.Header.Get("cosy-machinecode")
	if tok == "" || typ == "" || code == "" {
		if tok != "" || typ != "" || code != "" {
			t.Errorf("机器指纹头只发了一部分（token=%q type=%q code=%q）—— "+
				"要么三个都发，要么一个都不发", tok, typ, code)
		}
	}
}

// TestParseAccountStatusReadsFlatShape 真上游的**扁平**形状（实测过）。
func TestParseAccountStatusReadsFlatShape(t *testing.T) {
	// 字段值脱敏自 2026-07 国服实测响应（只留形状，数值是合成的）。
	raw := map[string]any{
		"userType":             "personal_standard",
		"plan":                 "PLAN_TIER_FREE",
		"userTag":              "Free",
		"orgName":              "",
		"nextResetAt":          float64(1784014393945),
		"isQuotaExceeded":      false,
		"totalUsagePercentage": 0.0,
	}
	st, ok := ParseAccountStatus(raw)
	if !ok {
		t.Fatal("ParseAccountStatus 没认出实测过的扁平形状")
	}
	if st.UserType != "personal_standard" || st.Plan != "PLAN_TIER_FREE" || st.UserTag != "Free" {
		t.Errorf("字段解析错了: %+v", st)
	}
	if st.NextResetAtMs != 1784014393945 {
		t.Errorf("NextResetAtMs = %d, 期望 1784014393945", st.NextResetAtMs)
	}
}

// TestParseAccountStatusReadsEnvelopeShape 信封形状（`{"code":0,"data":{…}}`）。
//
// ⚠ 这一层**未在真上游验证过**（参考实现只读顶层）。保留它是因为漏读的
// 代价是"永远拿不到周期却看不出原因"，而多读一层没有副作用。
func TestParseAccountStatusReadsEnvelopeShape(t *testing.T) {
	raw := map[string]any{
		"code": float64(0),
		"data": map[string]any{
			"userType":    "teams",
			"plan":        "PLAN_TIER_TEAM",
			"nextResetAt": float64(1790000000000),
		},
	}
	st, ok := ParseAccountStatus(raw)
	if !ok {
		t.Fatal("ParseAccountStatus 没认出信封形状")
	}
	if st.Plan != "PLAN_TIER_TEAM" || st.NextResetAtMs != 1790000000000 {
		t.Errorf("信封形状解析错了: %+v", st)
	}
}

// TestParseAccountStatusRejectsUnknownShape 三个特征键全空 ⇒ 不认。
//
// 返回 ok=false 让调用方**保留上一次的缓存**，而不是把周期清零。
func TestParseAccountStatusRejectsUnknownShape(t *testing.T) {
	for name, raw := range map[string]map[string]any{
		"nil":          nil,
		"empty":        {},
		"unrelated":    {"foo": "bar", "baz": float64(1)},
		"empty-envelo": {"code": float64(0), "data": map[string]any{"foo": "bar"}},
	} {
		t.Run(name, func(t *testing.T) {
			if _, ok := ParseAccountStatus(raw); ok {
				t.Error("认出了一个不认识的形状（应返回 ok=false 让调用方保留旧值）")
			}
		})
	}
}

// TestParseAccountStatusDoesNotGuessStringMillis 数字必须是数字。
//
// 上游若把 nextResetAt 发成字符串（`"1784014393945"`），我方**不猜** ——
// 猜错会把"未知"变成一个看起来很确定的错误时刻，而错误的周期比没有周期
// 更糟（界面会显示一个假的倒计时）。
func TestParseAccountStatusDoesNotGuessStringMillis(t *testing.T) {
	st, ok := ParseAccountStatus(map[string]any{
		"userType":    "personal",
		"nextResetAt": "1784014393945", // 字符串形态
	})
	if !ok {
		t.Fatal("形状本身是认识的（userType 非空），不该判为未知")
	}
	if st.NextResetAtMs != 0 {
		t.Errorf("NextResetAtMs = %d, 期望 0（字符串不猜）", st.NextResetAtMs)
	}
}

// TestFetchUserStatusErrorsAreNotSilent 失败必须报错，不能降级成零值。
//
// 静默返回零值会让"查不到"看起来像"这个账号没有周期"，而调用方据此
// 就会把已有周期清掉。
func TestFetchUserStatusErrorsAreNotSilent(t *testing.T) {
	cases := map[string]struct {
		status int
		body   string
		want   string
	}{
		"http-500":      {500, `{"code":500,"msg":"boom"}`, "HTTP 500"},
		"http-101":      {200, ``, ""}, // 见下：200 + 空体走"不是合法 JSON"
		"signature-101": {200, `{"code":101,"msg":"Signature invalid"}`, "可识别字段"},
		"not-json":      {200, `<html>oops</html>`, "不是合法 JSON"},
	}
	for name, tc := range cases {
		t.Run(name, func(t *testing.T) {
			srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				_ = readAllForTest(r)
				w.WriteHeader(tc.status)
				_, _ = w.Write([]byte(tc.body))
			}))
			defer srv.Close()

			c := New()
			c.userStatusBase = srv.URL
			_, err := c.FetchUserStatus(context.Background(), &Cred{UID: "u1"})
			if err == nil {
				t.Fatal("上游异常却返回了 nil error（静默降级会让调用方清掉已有周期）")
			}
			if tc.want != "" && !strings.Contains(err.Error(), tc.want) {
				t.Errorf("错误信息 %q 里没有 %q（错误必须可诊断）", err.Error(), tc.want)
			}
		})
	}
}

// TestFetchUserStatusRejectsMissingUID 没有 uid 时早失败。
//
// userId 是这个端点唯一的有效载荷，空 uid 必然查不到东西 —— 别让上游
// 用一个语义不明的 200 把它盖过去。
func TestFetchUserStatusRejectsMissingUID(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		t.Error("空 uid 不应该发出请求")
		w.WriteHeader(200)
	}))
	defer srv.Close()

	c := New()
	c.userStatusBase = srv.URL
	if _, err := c.FetchUserStatus(context.Background(), &Cred{}); err == nil {
		t.Fatal("空 uid 却成功了")
	}
	if _, err := c.FetchUserStatus(context.Background(), nil); err == nil {
		t.Fatal("nil 账号却成功了")
	}
}

// TestResolveNextResetMsPrefersUserStatus 优先级：/user/status 权威。
//
// 参考实现 bridge\session.go:193-202 的原话：「the gateway's nextResetAt is
// authoritative, and the OpenAPI expiresAt is only a fallback for when it is
// missing」。
func TestResolveNextResetMsPrefersUserStatus(t *testing.T) {
	const (
		fromStatus = int64(1784014393945)
		fromQuota  = int64(1790000000000)
		sentinel   = int64(253402214400000) // 9999-12-31
	)
	cases := map[string]struct {
		st   *AccountStatus
		q    *Quota
		want int64
	}{
		"status-wins":            {&AccountStatus{NextResetAtMs: fromStatus}, &Quota{ExpiresAt: fromQuota}, fromStatus},
		"falls-back-to-quota":    {&AccountStatus{}, &Quota{ExpiresAt: fromQuota}, fromQuota},
		"sentinel-in-status":     {&AccountStatus{NextResetAtMs: sentinel}, &Quota{ExpiresAt: fromQuota}, fromQuota},
		"sentinel-in-both":       {&AccountStatus{NextResetAtMs: sentinel}, &Quota{ExpiresAt: sentinel}, 0},
		"zero-in-both":           {&AccountStatus{}, &Quota{}, 0},
		"nil-status":             {nil, &Quota{ExpiresAt: fromQuota}, fromQuota},
		"nil-quota":              {&AccountStatus{NextResetAtMs: fromStatus}, nil, fromStatus},
		"nil-both":               {nil, nil, 0},
		"negative-is-not-usable": {&AccountStatus{NextResetAtMs: -1}, &Quota{ExpiresAt: fromQuota}, fromQuota},
	}
	for name, tc := range cases {
		t.Run(name, func(t *testing.T) {
			if got := ResolveNextResetMs(tc.st, tc.q); got != tc.want {
				t.Errorf("ResolveNextResetMs = %d, 期望 %d", got, tc.want)
			}
		})
	}
}

// TestPlausibleResetMs 可信度判据（与 internal/usage 的同名函数同值）。
func TestPlausibleResetMs(t *testing.T) {
	const sentinel = int64(253402214400000) // 9999-12-31，上游对无到期套餐报的值
	cases := map[string]struct {
		ms   int64
		want bool
	}{
		"zero":            {0, false},
		"negative":        {-1, false},
		"one-ms":          {1, true},
		"real-2026":       {1784014393945, true},
		"just-below-max":  {MaxPlausibleResetMs - 1, true},
		"exactly-max":     {MaxPlausibleResetMs, false},
		"far-future-9999": {sentinel, false},
	}
	for name, tc := range cases {
		t.Run(name, func(t *testing.T) {
			if got := PlausibleResetMs(tc.ms); got != tc.want {
				t.Errorf("PlausibleResetMs(%d) = %v, 期望 %v", tc.ms, got, tc.want)
			}
		})
	}
}

// TestMaxPlausibleResetMsMatchesUsagePackage 两个包里的常量必须同值。
//
// internal/usage 是零依赖包，不能反向 import 本包，故各留一份 —— 但两份
// 一旦分叉，"可信"的判据就会在两个地方给出不同答案（同一时刻在一处可信、
// 另一处不可信）。这里用**源码文本**断言，因为常量无法跨包比较（除非 import，
// 而那会成环）。
func TestMaxPlausibleResetMsMatchesUsagePackage(t *testing.T) {
	src, err := os.ReadFile("../usage/cycle.go")
	if err != nil {
		t.Fatalf("读取 ../usage/cycle.go 失败: %v", err)
	}
	// 期望在 usage 侧出现同一个数值。
	want := "4102444800000"
	if !strings.Contains(string(src), want) {
		t.Errorf("../usage/cycle.go 里没有 %s —— 两个包的 MaxPlausibleResetMs 分叉了", want)
	}
	if MaxPlausibleResetMs != 4102444800000 {
		t.Errorf("本包 MaxPlausibleResetMs = %d, 期望 4102444800000", MaxPlausibleResetMs)
	}
}

// TestMarshalNoEscapeKeepsAngleBrackets SetEscapeHTML(false) 的必要性。
//
// 默认的 json.Marshal 会把 `<` `>` `&` 转成 \u003c 这类序列，字节一变
// 签名就对不上（参考实现 auth.go:271-279 同样处理）。
func TestMarshalNoEscapeKeepsAngleBrackets(t *testing.T) {
	got, err := marshalNoEscape(map[string]string{"k": "a<b>&c"})
	if err != nil {
		t.Fatalf("marshalNoEscape 失败: %v", err)
	}
	if strings.Contains(string(got), `\u003c`) || strings.Contains(string(got), `\u0026`) {
		t.Errorf("HTML 转义没被关掉: %s", got)
	}
	if strings.HasSuffix(string(got), "\n") {
		t.Errorf("尾部有多余换行: %q", got)
	}
	if !strings.Contains(string(got), "a<b>&c") {
		t.Errorf("内容被改动了: %s", got)
	}
}

// TestUserStatusDateIsGMT date 头必须是 GMT 格式（签名覆盖它）。
func TestUserStatusDateIsGMT(t *testing.T) {
	// 用一个**非 UTC** 的时刻，确认输出被转成了 UTC。
	loc := time.FixedZone("CST", 8*3600)
	at := time.Date(2026, time.July, 14, 23, 33, 13, 0, loc)
	got := userStatusDate(at)
	if !strings.HasSuffix(got, "GMT") {
		t.Errorf("date = %q, 期望以 GMT 结尾", got)
	}
	want := "Tue, 14 Jul 2026 15:33:13 GMT"
	if got != want {
		t.Errorf("date = %q, 期望 %q（UTC 转换 + GMT 布局）", got, want)
	}
}
