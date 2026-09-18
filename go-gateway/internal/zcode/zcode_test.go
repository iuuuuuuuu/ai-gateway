package zcode

// Z2 单元测试：凭证、服务商、错误分类、额度解析、身份头。
//
// ## 测试重点：**形状守卫**
//
// 参考实现的注释反复强调一件事（`resolver.ts`）：
//
//	Shape guard: silently returning undefined used to store a bogus
//	credential that surfaced only as cryptic upstream 401s.
//
// 也就是说：上游字段改名时，**静默**存下 "undefined" 会让问题推迟到
// 很久以后、以"401 认证失败"的形式出现 —— 而真正原因是我们的解析错了。
//
// 故本文件的重点是"上游形状变化时必须报错"，而不是"正常情况能解析"。

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// ---------------------------------------------------------------------------
// 凭证解析
// ---------------------------------------------------------------------------

// TestParseTwoPartCredential 两段式（Z.AI 的必需形态）。
func TestParseTwoPartCredential(t *testing.T) {
	c, err := Parse("abc123.def456")
	if err != nil {
		t.Fatal(err)
	}
	if c.Credential != "abc123.def456" {
		t.Errorf("凭证应原样保留，实际 %q", c.Credential)
	}
	if c.UID == "" {
		t.Error("应派生 UID")
	}
}

// TestParseSinglePartCredential 单段式（智谱的 secret 可选）。
//
// 若把单段判为非法，智谱用户就无法导入 —— 而单段是合法的。
func TestParseSinglePartCredential(t *testing.T) {
	if _, err := Parse("abc123"); err != nil {
		t.Errorf("单段凭证应合法（智谱的 secret 可选），实际报错: %v", err)
	}
}

// TestParseTrimsWhitespaceAndNewlines 粘贴常带空白与换行。
//
// 不清理会拼出坏请求头（值里含 \n 会让 Go 的 http 包直接报错）。
func TestParseTrimsWhitespaceAndNewlines(t *testing.T) {
	cases := []string{
		"  abc.def  ",
		"abc.def\n",
		"\nabc.def\r\n",
		"\t abc.def \t",
	}
	for _, in := range cases {
		c, err := Parse(in)
		if err != nil {
			t.Errorf("Parse(%q) 应成功，实际 %v", in, err)
			continue
		}
		if c.Credential != "abc.def" {
			t.Errorf("Parse(%q) 应清理成 abc.def，实际 %q", in, c.Credential)
		}
	}
}

// TestParseRejectsEmpty 空凭证必须报错。
func TestParseRejectsEmpty(t *testing.T) {
	for _, in := range []string{"", "   ", "\n\n", "\t"} {
		if _, err := Parse(in); err == nil {
			t.Errorf("Parse(%q) 应报错", in)
		}
	}
}

// TestParseRejectsSpacesInside 凭证内部含空格时必须报错。
//
// 常见错误：用户粘贴了 "apiKey: xxx" 或整行配置，而空格是识别信号。
// 放行会拼出坏请求头（HTTP 头值不能含未转义空格会被上游拒）。
func TestParseRejectsSpacesInside(t *testing.T) {
	if _, err := Parse("apiKey: abc.def"); err == nil {
		t.Error("含空格的输入应报错（多半是粘贴了多余内容）")
	}
	if _, err := Parse("abc def"); err == nil {
		t.Error("含空格的输入应报错")
	}
}

// TestCredKeyIsStableAndIdempotent 同一凭证必须派生同一 UID。
//
// 这是"重复导入幂等"的基础：不稳定的话，每次导入都会新增一个账号，
// 而用户会看到账号列表里出现一堆重复项。
func TestCredKeyIsStableAndIdempotent(t *testing.T) {
	a := CredKey("abc.def")
	b := CredKey("abc.def")
	if a != b {
		t.Errorf("同一凭证应派生同一 UID：%q vs %q", a, b)
	}
	// 不同凭证应派生不同 UID
	if CredKey("abc.def") == CredKey("xyz.uvw") {
		t.Error("不同凭证不应派生相同 UID")
	}
	// 前后空白不应影响（与 Parse 的清理口径一致）
	if CredKey("abc.def") != CredKey("  abc.def  ") {
		t.Error("前后空白不应影响 UID（否则同一凭证会被当成两个账号）")
	}
}

// TestCredKeyDoesNotLeakCredential UID 里**不得**含凭证原文。
//
// 安全项：UID 会出现在日志、状态文件、界面 URL 里。
// 若它就是凭证，等于把秘密到处散布。
func TestCredKeyDoesNotLeakCredential(t *testing.T) {
	secret := "supersecretapikey.verysecretpart"
	uid := CredKey(secret)
	if strings.Contains(uid, "supersecret") || strings.Contains(uid, "verysecret") {
		t.Errorf("UID 泄漏了凭证内容: %q", uid)
	}
	if !strings.HasPrefix(uid, "zcode-") {
		t.Errorf("UID 应有 zcode- 前缀便于识别，实际 %q", uid)
	}
}

// TestMaskSecret 脱敏。
func TestMaskSecret(t *testing.T) {
	// 长凭证：只留前 6 后 4
	got := MaskSecret("abcdef1234567890xyz")
	if strings.Contains(got, "1234567890") {
		t.Errorf("脱敏后不应含中段内容: %q", got)
	}
	// 短凭证：全遮
	if MaskSecret("short") != "****" {
		t.Errorf("短凭证应全遮，实际 %q", MaskSecret("short"))
	}
}

// TestLoadFileNestedForm 嵌套形（宿主落盘）。
func TestLoadFileNestedForm(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "zcode-x.json")
	doc := `{"uid":"zcode-abc","nickname":"我的账号","provider":"zai","credential":"k1.s1","jwt":"jwt-token"}`
	if err := os.WriteFile(p, []byte(doc), 0o600); err != nil {
		t.Fatal(err)
	}
	c, err := LoadFile(p)
	if err != nil {
		t.Fatal(err)
	}
	if c.Credential != "k1.s1" || c.Provider != ProviderZAI || c.Nickname != "我的账号" || c.JWT != "jwt-token" {
		t.Errorf("解析结果不对: %+v", c)
	}
	if c.FilePath != p {
		t.Error("应回填 FilePath")
	}
}

// TestLoadFileFlatForm 扁平形（手写）。
func TestLoadFileFlatForm(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "zcode-y.json")
	// 分开的 api_key + secret
	doc := `{"api_key":"k2","secret":"s2","provider":"bigmodel"}`
	if err := os.WriteFile(p, []byte(doc), 0o600); err != nil {
		t.Fatal(err)
	}
	c, err := LoadFile(p)
	if err != nil {
		t.Fatal(err)
	}
	if c.Credential != "k2.s2" {
		t.Errorf("分开的 api_key+secret 应拼成 k2.s2，实际 %q", c.Credential)
	}
	if c.Provider != ProviderBigmodel {
		t.Errorf("服务商应为 bigmodel，实际 %q", c.Provider)
	}
}

// TestLoadFileBareString 整个文件是一个 JSON 字符串。
func TestLoadFileBareString(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "zcode-z.json")
	if err := os.WriteFile(p, []byte(`"bare-key.secret"`), 0o600); err != nil {
		t.Fatal(err)
	}
	c, err := LoadFile(p)
	if err != nil {
		t.Fatal(err)
	}
	if c.Credential != "bare-key.secret" {
		t.Errorf("裸字符串形态应能解析，实际 %q", c.Credential)
	}
}

// TestLoadFileMissingCredentialField 找不到凭证字段时必须报错。
//
// 形状守卫：静默返回空凭证会存下一个"看起来正常但永远 401"的账号。
func TestLoadFileMissingCredentialField(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "zcode-bad.json")
	if err := os.WriteFile(p, []byte(`{"foo":"bar"}`), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, err := LoadFile(p); err == nil {
		t.Error("找不到凭证字段时应报错（形状守卫）")
	}
}

// TestLoadFileRejectsBadJSON 非法 JSON 必须报错（不能静默跳过）。
func TestLoadFileRejectsBadJSON(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "zcode-bad2.json")
	if err := os.WriteFile(p, []byte(`{坏`), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, err := LoadFile(p); err == nil {
		t.Error("非法 JSON 应报错")
	}
	// 空文件
	p2 := filepath.Join(dir, "zcode-empty.json")
	_ = os.WriteFile(p2, []byte(""), 0o600)
	if _, err := LoadFile(p2); err == nil {
		t.Error("空文件应报错")
	}
}

// TestLoadDirReportsFailures 失败清单必须报出来。
func TestLoadDirReportsFailures(t *testing.T) {
	dir := t.TempDir()
	_ = os.WriteFile(filepath.Join(dir, "zcode-ok.json"),
		[]byte(`{"credential":"k.s","provider":"zai"}`), 0o600)
	_ = os.WriteFile(filepath.Join(dir, "zcode-bad.json"), []byte(`{坏`), 0o600)
	// 无关文件不应被扫到
	_ = os.WriteFile(filepath.Join(dir, "qoder-x.json"), []byte(`{}`), 0o600)

	creds, failed, err := LoadDir(dir)
	if err != nil {
		t.Fatal(err)
	}
	if len(creds) != 1 {
		t.Errorf("应解析出 1 个凭证，实际 %d", len(creds))
	}
	if len(failed) != 1 {
		t.Errorf("应报告 1 个失败，实际 %d: %v", len(failed), failed)
	}
}

// TestSaveAtomicRoundTrip 落盘后能读回（往返一致）。
func TestSaveAtomicRoundTrip(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "zcode-rt.json")
	c := &Cred{
		UID: "zcode-rt", Nickname: "测试", Credential: "k.s",
		Provider: ProviderZAI, JWT: "jwt-x", JWTIssuedAt: 1700000000,
		FilePath: p,
	}
	if err := c.SaveAtomic(); err != nil {
		t.Fatal(err)
	}
	back, err := LoadFile(p)
	if err != nil {
		t.Fatal(err)
	}
	if back.Credential != "k.s" || back.Provider != ProviderZAI || back.JWT != "jwt-x" {
		t.Errorf("往返不一致: %+v", back)
	}
	if back.JWTIssuedAt != 1700000000 {
		t.Errorf("JWT 签发时间应保留，实际 %d", back.JWTIssuedAt)
	}
	// 落盘内容里凭证应可见（供再次导入），但**不该**有额外的敏感字段
	raw, _ := os.ReadFile(p)
	var doc map[string]any
	_ = json.Unmarshal(raw, &doc)
	if doc["credential"] != "k.s" {
		t.Error("落盘应含 credential 字段")
	}
}

// ---------------------------------------------------------------------------
// 服务商
// ---------------------------------------------------------------------------

// TestParseProvider 服务商解析（容错常见写法）。
func TestParseProvider(t *testing.T) {
	cases := map[string]Provider{
		"zai":        ProviderZAI,
		"Z.AI":       ProviderZAI,
		"z-ai":       ProviderZAI,
		"bigmodel":   ProviderBigmodel,
		"zhipu":      ProviderBigmodel,
		"智谱":         ProviderBigmodel,
		"glm":        ProviderBigmodel,
		"":           ProviderUnknown,
		"unknown":    ProviderUnknown,
		"something":  ProviderUnknown,
	}
	for in, want := range cases {
		if got := ParseProvider(in); got != want {
			t.Errorf("ParseProvider(%q) = %q，期望 %q", in, got, want)
		}
	}
}

// TestProviderEndpointsMatchReference 端点表必须与参考实现一致。
func TestProviderEndpointsMatchReference(t *testing.T) {
	zai := ProviderZAI
	if zai.OpenAIBase() != "https://api.z.ai/api/coding/paas/v4" {
		t.Errorf("Z.AI OpenAI 端点错误: %s", zai.OpenAIBase())
	}
	if zai.AnthropicBase() != "https://api.z.ai/api/anthropic" {
		t.Errorf("Z.AI Anthropic 端点错误: %s", zai.AnthropicBase())
	}
	if zai.BizHost() != "https://api.z.ai" {
		t.Errorf("Z.AI 业务 host 错误: %s", zai.BizHost())
	}

	bm := ProviderBigmodel
	if bm.OpenAIBase() != "https://open.bigmodel.cn/api/coding/paas/v4" {
		t.Errorf("智谱 OpenAI 端点错误: %s", bm.OpenAIBase())
	}
	if bm.BizHost() != "https://open.bigmodel.cn" {
		t.Errorf("智谱业务 host 错误: %s", bm.BizHost())
	}

	// 两个服务商的端点不能相同
	if zai.OpenAIBase() == bm.OpenAIBase() {
		t.Error("两个服务商的端点不应相同")
	}
}

// TestProviderLabel 面向用户的服务商名。
func TestProviderLabel(t *testing.T) {
	if ProviderZAI.Label() != "Z.AI" {
		t.Errorf("ProviderZAI.Label() = %q", ProviderZAI.Label())
	}
	if ProviderBigmodel.Label() != "智谱" {
		t.Errorf("ProviderBigmodel.Label() = %q", ProviderBigmodel.Label())
	}
	// 未知必须给可读标签（界面会显示成空白）
	if ProviderUnknown.Label() == "" {
		t.Error("未知服务商应有可读标签")
	}
}

// TestAllProvidersCoversBoth 界面选项要覆盖两个服务商。
func TestAllProvidersCoversBoth(t *testing.T) {
	all := AllProviders()
	if len(all) != 2 {
		t.Fatalf("应有 2 个服务商，实际 %d", len(all))
	}
	seen := map[Provider]bool{}
	for _, p := range all {
		seen[p] = true
	}
	if !seen[ProviderZAI] || !seen[ProviderBigmodel] {
		t.Error("应同时包含 Z.AI 与智谱")
	}
}

// ---------------------------------------------------------------------------
// 错误分类
// ---------------------------------------------------------------------------

// TestClassifyAuthMissingVsFailed 1001 与 1000 必须区分。
//
// 这是实测确认的区分（见 signing.go 的对照实验）：
//   · 1001 = 认证参数未收到 → **我们的 bug**（没带 Authorization）
//   · 1000 = 认证失败       → 用户要换凭证
//
// 不区分的话，1001 会被显示成"凭证无效"，用户反复换凭证也无用。
func TestClassifyAuthMissingVsFailed(t *testing.T) {
	cases := []struct {
		name string
		body string
		want ErrKind
	}{
		{
			"1001 认证参数未收到",
			`{"error":{"code":"1001","message":"Authentication parameter not received in Header, unable to authenticate"}}`,
			ErrAuthMissing,
		},
		{
			"1000 认证失败",
			`{"error":{"code":"1000","message":"Authentication Failed"}}`,
			ErrAuthFailed,
		},
		{
			"1000 中文（智谱）",
			`{"error":{"code":"1000","message":"身份验证失败。"}}`,
			ErrAuthFailed,
		},
		{
			"Anthropic 形状（码在 type）",
			`{"error":{"message":"Authentication Failed","type":"1000"}}`,
			ErrAuthFailed,
		},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if got := Classify(401, tc.body); got != tc.want {
				t.Errorf("Classify = %q，期望 %q", got, tc.want)
			}
		})
	}
}

// TestClassifySigningRequired 签名错误必须单独分类。
//
// 我们**刻意没实现签名**（见 signing.go）。若上游真要求了，
// 报错必须点明"要补签名实现"，而不是笼统的"认证失败" ——
// 否则排查方向会跑到"换凭证"上去，白费功夫。
func TestClassifySigningRequired(t *testing.T) {
	cases := []string{
		`{"error":{"reason":"VERIFY_SIGNATURE_INVALID"}}`,
		`{"data":{"reason":"VERIFY_SIGNATURE_INVALID"}}`,
		`{"error":{"message":"VERIFY_APIKEY_EXPIRED"}}`,
		`{"msg":"verify_signature_invalid"}`, // 大小写不敏感
	}
	for _, body := range cases {
		if got := Classify(401, body); got != ErrSigningRequired {
			t.Errorf("Classify(%s) = %q，期望 %q", body, got, ErrSigningRequired)
		}
	}
}

// TestClassifyRateLimitAndServerError HTTP 状态码兜底。
func TestClassifyRateLimitAndServerError(t *testing.T) {
	if got := Classify(429, ""); got != ErrRateLimited {
		t.Errorf("429 应分类为限流，实际 %q", got)
	}
	if got := Classify(503, ""); got != ErrProviderDown {
		t.Errorf("503 应分类为上游不可用，实际 %q", got)
	}
	if got := Classify(402, ""); got != ErrQuotaExhausted {
		t.Errorf("402 应分类为额度耗尽，实际 %q", got)
	}
}

// TestClassifyUnknownIsNone 认不出的错误**不猜**。
//
// 猜错比不猜更糟：会把用户引向错误的排查方向。
func TestClassifyUnknownIsNone(t *testing.T) {
	if got := Classify(400, `{"weird":"shape"}`); got != ErrNone {
		t.Errorf("无法识别时应返回 ErrNone（不猜），实际 %q", got)
	}
}

// TestFriendlyMessageIsActionable 错误文案必须**可指导动作**。
//
// 复述上游原文（如"身份验证失败。"）不告诉用户该做什么。
func TestFriendlyMessageIsActionable(t *testing.T) {
	cases := []struct {
		kind    ErrKind
		mustHas string
	}{
		{ErrAuthMissing, "网关"},
		{ErrAuthFailed, "重新"},
		{ErrSigningRequired, "签名"},
		{ErrRateLimited, "限流"},
		{ErrQuotaExhausted, "额度"},
		{ErrProviderDown, "不可用"},
	}
	for _, tc := range cases {
		e := &Error{Kind: tc.kind, Status: 401, Msg: "raw upstream text"}
		msg := e.FriendlyMessage()
		if !strings.Contains(msg, tc.mustHas) {
			t.Errorf("%s 的文案应含 %q，实际 %q", tc.kind, tc.mustHas, msg)
		}
		// 文案不能只是复述上游原文
		if msg == "raw upstream text" {
			t.Errorf("%s 的文案不应只是复述上游原文", tc.kind)
		}
	}
}

// TestNewErrorExtractsFields 错误对象要带码与文案。
func TestNewErrorExtractsFields(t *testing.T) {
	e := NewError(401, `{"error":{"code":"1000","message":"Authentication Failed"}}`)
	if e.Code != "1000" {
		t.Errorf("应提取业务码，实际 %q", e.Code)
	}
	if e.Msg != "Authentication Failed" {
		t.Errorf("应提取文案，实际 %q", e.Msg)
	}
	if e.Kind != ErrAuthFailed {
		t.Errorf("应分类为认证失败，实际 %q", e.Kind)
	}
	if !strings.Contains(e.Error(), "1000") {
		t.Error("Error() 应含业务码（便于排障）")
	}
}

// ---------------------------------------------------------------------------
// 身份头
// ---------------------------------------------------------------------------

// TestIdentityHeadersShape 身份头的清单与取值。
func TestIdentityHeadersShape(t *testing.T) {
	id := Identity{
		AppVersion: "3.12.3", SourceTitle: "zcode", RefererOrigin: "https://zcode.z.ai",
		ReleaseChannel: "production", ClientLanguage: "zh-CN", ClientTimezone: "Asia/Shanghai",
		Platform: "windows", Arch: "amd64", OSVersion: "10.0.22631",
	}
	h := id.Headers()

	must := map[string]string{
		"HTTP-Referer":        "https://zcode.z.ai",
		"User-Agent":          "ZCode/3.12.3",
		"X-ZCode-App-Version": "3.12.3",
		"X-Title":             "Z Code@zcode",
		"X-Release-Channel":   "production",
		"X-Client-Language":   "zh-CN",
		"X-Client-Timezone":   "Asia/Shanghai",
		"X-ZCode-Agent":       "glm",
		// ⚠ 必须是 **Node.js 命名**（win32-x64），不是 Go 的 windows-amd64。
		// 实测取值错误会被上游 400 {"code":3001} 拒绝，且错误信息不提示是哪个参数。
		"X-Platform":    "win32-x64",
		"X-Os-Category": "windows",
		"X-Os-Version":  "10.0.22631",
	}
	for k, want := range must {
		if h[k] != want {
			t.Errorf("%s 应为 %q，实际 %q", k, want, h[k])
		}
	}

	// **不发** X-Device-Mid：参考实现注释说 LLM 请求路径不再发它，
	// 多带一个真实客户端不发的头反而成为区分特征。
	if _, ok := h["X-Device-Mid"]; ok {
		t.Error("LLM 请求不应发 X-Device-Mid（会成为区分特征）")
	}
}

// TestIdentityOmitsUnknownValues 取不到的值应**省略**而不是填 "unknown"。
//
// 填假值比不发更容易被识别 —— 真实客户端不会发 platform="unknown"。
func TestIdentityOmitsUnknownValues(t *testing.T) {
	id := Identity{
		AppVersion: "3.12.3", SourceTitle: "zcode", RefererOrigin: "https://zcode.z.ai",
		// Platform / Arch / OSVersion 故意留空
	}
	h := id.Headers()
	if _, ok := h["X-Platform"]; ok {
		t.Error("platform/arch 缺失时不应发 X-Platform（拼出假值更易被识别）")
	}
	if _, ok := h["X-Os-Category"]; ok {
		t.Error("platform 缺失时不应发 X-Os-Category")
	}
	if _, ok := h["X-Os-Version"]; ok {
		t.Error("OS 版本缺失时不应发 X-Os-Version")
	}
	// 但语言/时区有"unknown"回退（官方行为），故仍应发出
	if h["X-Client-Language"] == "" || h["X-Client-Timezone"] == "" {
		t.Error("语言与时区应有回退值（官方行为）")
	}
}

// TestIdentityPlatformArchFallback 空值必须回退到真实值。
//
// 参考实现：空 override 会拼出 "-x64" / "linux-" —— 真实客户端不会发这种形状。
func TestIdentityPlatformArchFallback(t *testing.T) {
	id := Identity{} // 全空
	pa := id.PlatformArch()
	if strings.HasPrefix(pa, "-") || strings.HasSuffix(pa, "-") {
		t.Errorf("PlatformArch 不应以分隔符开头/结尾（真实客户端不会这么发）: %q", pa)
	}
	if !strings.Contains(pa, "-") {
		t.Errorf("PlatformArch 应形如 platform-arch，实际 %q", pa)
	}
}

// TestPlatformUsesNodeNaming 平台名必须用 **Node.js 的命名**，不是 Go 的。
//
// ## 为什么这条测试很重要
//
// 实测（uitest/diag-zcode-platform-value.cjs）：`client/configs` 端点
// **严格校验** X-Platform 的取值，错误的取值返回
//
//	400 {"code":3001,"msg":"parameter error"}
//
// 而这个错误信息**完全不提示是哪个参数错** —— 我最初就是卡在这里，
// 逐个二分 9 个身份头才定位到它。
//
// 上游是照 Node.js 的 `process.platform`/`process.arch` 校验的
//（官方客户端是 Electron 应用），所以：
//
//	Go 的 windows/amd64  →  必须映射成  win32/x64
//
// 这条映射一旦回归，所有请求都会 400 而看不出原因。
func TestPlatformUsesNodeNaming(t *testing.T) {
	cases := []struct {
		goos, goarch string
		want         string
	}{
		{"windows", "amd64", "win32-x64"}, // 最常见，也是最容易写错的
		{"windows", "arm64", "win32-arm64"},
		{"windows", "386", "win32-ia32"},
		{"darwin", "arm64", "darwin-arm64"},
		{"linux", "amd64", "linux-x64"},
		// 已经是 Node 命名时不应二次转换
		{"win32", "x64", "win32-x64"},
	}
	for _, tc := range cases {
		id := Identity{Platform: tc.goos, Arch: tc.goarch}
		if got := id.PlatformArch(); got != tc.want {
			t.Errorf("PlatformArch(%s/%s) = %q，期望 %q", tc.goos, tc.goarch, got, tc.want)
		}
	}

	// 单独断言最容易错的那一条（Go 的 amd64 ≠ Node 的 amd64）
	if got := nodePlatform("windows"); got != "win32" {
		t.Errorf("nodePlatform(windows) = %q，期望 win32（不是 windows！）", got)
	}
	if got := nodeArch("amd64"); got != "x64" {
		t.Errorf("nodeArch(amd64) = %q，期望 x64（不是 amd64！）", got)
	}
}

// TestOSCategoryMapping 平台名归一。
func TestOSCategoryMapping(t *testing.T) {
	cases := map[string]string{
		"darwin":  "macos",
		"windows": "windows",
		"linux":   "linux",
		"freebsd": "linux", // 其它一律 linux（与参考实现一致）
	}
	for in, want := range cases {
		if got := osCategory(in); got != want {
			t.Errorf("osCategory(%q) = %q，期望 %q", in, got, want)
		}
	}
}

// ---------------------------------------------------------------------------
// 额度解析
// ---------------------------------------------------------------------------

// TestToInt64AcceptsMultipleShapes 上游可能返回数字或字符串。
//
// 只认数字会让额度静默变成 0 —— 而 0 会被用户误读成"额度耗尽"。
func TestToInt64AcceptsMultipleShapes(t *testing.T) {
	cases := []struct {
		in   any
		want int64
	}{
		{float64(123), 123},
		{int64(456), 456},
		{int(789), 789},
		{"1234", 1234},
		{"  1234  ", 1234},
		{json.Number("5678"), 5678},
		{nil, 0},
		{"not-a-number", 0},
	}
	for _, tc := range cases {
		if got := toInt64(tc.in); got != tc.want {
			t.Errorf("toInt64(%v) = %d，期望 %d", tc.in, got, tc.want)
		}
	}
}

// TestQuotaSoonestExpiry 最早到期时刻（跨产品路由要用）。
func TestQuotaSoonestExpiry(t *testing.T) {
	q := &Quota{Entries: []QuotaEntry{
		{ExpiresAt: 2000},
		{ExpiresAt: 1000}, // 最早
		{ExpiresAt: 0},    // 未知应忽略
		{ExpiresAt: 3000},
	}}
	if got := q.SoonestExpiry(); got != 1000 {
		t.Errorf("最早到期应为 1000，实际 %d", got)
	}
	// 全部未知 → 0（而不是某个假值）
	q2 := &Quota{Entries: []QuotaEntry{{ExpiresAt: 0}, {ExpiresAt: 0}}}
	if got := q2.SoonestExpiry(); got != 0 {
		t.Errorf("全部未知时应返回 0，实际 %d", got)
	}
}

// TestFetchQuotaWithoutJWT 没有 JWT 时必须返回 ErrNoJWT（而不是空结果）。
//
// 界面据此显示"额度未知（需要 OAuth 登录）"，而不是 0 ——
// 0 会被误读成"额度耗尽"，让用户白折腾。
func TestFetchQuotaWithoutJWT(t *testing.T) {
	c := New()
	cr := &Cred{Credential: "k.s", Provider: ProviderZAI} // 无 JWT
	_, err := c.FetchQuota(nil, cr)
	if err == nil {
		t.Fatal("没有 JWT 时应报错")
	}
	if err != ErrNoJWT {
		t.Errorf("应返回 ErrNoJWT（供界面区分），实际 %v", err)
	}
}

// ---------------------------------------------------------------------------
// 模型清单
// ---------------------------------------------------------------------------

// TestBuiltinModelsShape 内置清单的形状（兜底用）。
func TestBuiltinModelsShape(t *testing.T) {
	ms := BuiltinModels()
	if len(ms) != 11 {
		t.Errorf("内置清单应有 11 个模型（与参考实现一致），实际 %d", len(ms))
	}
	for _, m := range ms {
		if m.ID == "" {
			t.Error("每个模型都应有 id")
		}
		// Source 必须标为 builtin —— 让界面能区分"上游真值"与"内置兜底"
		if m.Source != "builtin" {
			t.Errorf("内置清单的 Source 应为 builtin，实际 %q（模型 %s）", m.Source, m.ID)
		}
	}
	// 抽查几个已知值
	idx := builtinModelIndex()
	if m, ok := idx["glm-4.6"]; !ok || m.ContextWindow != 200000 {
		t.Error("glm-4.6 的上下文窗口应为 200000")
	}
	if m, ok := idx["glm-5.3"]; !ok || m.ContextWindow != 1000000 {
		t.Error("glm-5.3 的上下文窗口应为 1000000")
	}
	if m, ok := idx["glm-4.6v"]; !ok || m.Reasoning {
		t.Error("glm-4.6v 不应标为推理模型")
	}
}

// ---------------------------------------------------------------------------
// 签名（刻意不实现）
// ---------------------------------------------------------------------------

// TestSigningIsDeliberatelyNotImplemented 记录"我们没实现签名"这件事。
//
// 这条测试的价值：让这个**刻意的决定**在代码里可见、可追溯，
// 而不是靠"搜不到相关代码"来推断。
func TestSigningIsDeliberatelyNotImplemented(t *testing.T) {
	if !SigningNotImplemented {
		t.Error("SigningNotImplemented 应为 true —— 若改成 false，说明补了签名实现，" +
			"此时应同时更新 signing.go 的说明与 errors.go 的分类")
	}
}

// TestIsVerifyFailure 签名失败识别（大小写不敏感）。
func TestIsVerifyFailure(t *testing.T) {
	yes := []string{
		"VERIFY_SIGNATURE_INVALID",
		`{"error":{"reason":"VERIFY_SIGNATURE_INVALID"}}`,
		"verify_apikey_expired",
		"prefix VERIFY_SIGNATURE_INVALID suffix",
	}
	for _, s := range yes {
		if !IsVerifyFailure(s) {
			t.Errorf("IsVerifyFailure(%q) 应为 true", s)
		}
	}
	no := []string{"", "1000", "Authentication Failed", "some other error"}
	for _, s := range no {
		if IsVerifyFailure(s) {
			t.Errorf("IsVerifyFailure(%q) 应为 false", s)
		}
	}
}
