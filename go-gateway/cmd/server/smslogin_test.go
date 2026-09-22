package main

// smslogin_test.go —— 短信登录登记逻辑的回归测试（2026-09-22 新增）。
//
// # 这组测试保护什么
//
// 短信登录的**登记**环节有三处极易出错、且错了之后症状与原因相距很远：
//
//  1. **文件名前缀**：`auth.LoadDir` 只 glob `workbuddy*.json`。
//     写成 `<uid>.json` 会让凭证**静静地不被加载** ——
//     表现为「登录成功、界面也提示成功，但账号列表里没有它」。
//  2. **uid 解析**：短信登录的响应**只给 token 不给 uid**。
//     解析失败时若编一个随机值，同一手机号每次登录都会多出一个账号。
//  3. **路径穿越**：uid 来自上游返回的 JWT，**不能假设**它是 UUID。
//     含 `/` 或 `..` 的 uid 会把凭证写到目录之外。
//
// 前两条的症状都不会报错，只会让人看到"少了个账号"，很难联想到文件名。

import (
	"encoding/base64"
	"encoding/json"
	"os"
	"strings"
	"testing"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/pool"
	"workbuddy2api/internal/server"
)

// makeJWT 造一个只有 payload 有意义的 JWT（签名随便填 —— 我们只读负载）。
func makeJWT(t *testing.T, claims map[string]any) string {
	t.Helper()
	raw, err := json.Marshal(claims)
	if err != nil {
		t.Fatal(err)
	}
	enc := base64.RawURLEncoding.EncodeToString(raw)
	return "header." + enc + ".sig"
}

// TestUIDFromAccessTokenReadsKnownClaims 认得几个已知的声明名。
//
// ⚠ 多个候选名不是"过度设计"：上游改过一次字段名，
// 只认一个会在改版后**静默失效**（登录报"无法解析账号标识"，
// 而 token 本身完全有效）。
func TestUIDFromAccessTokenReadsKnownClaims(t *testing.T) {
	want := "{uuid}"
	for _, key := range []string{"uid", "userId", "user_id", "sub"} {
		tok := makeJWT(t, map[string]any{key: want})
		got, err := uidFromAccessToken(tok)
		if err != nil {
			t.Errorf("声明 %q 应能解析出 uid，实际报错：%v", key, err)
			continue
		}
		if got != want {
			t.Errorf("声明 %q 解析出 %q，期望 %q", key, got, want)
		}
	}
}

// TestUIDFromAccessTokenRejectsOpaqueToken 非 JWT 令牌必须**报错**而不是猜。
//
// 猜一个 uid 会产生"每次登录多一个账号"的坏结果（见文件头）。
func TestUIDFromAccessTokenRejectsOpaqueToken(t *testing.T) {
	for _, tok := range []string{"", "not-a-jwt", "onlyonepart", "a.b"} {
		if _, err := uidFromAccessToken(tok); err == nil {
			t.Errorf("令牌 %q 不是可解析的 JWT，应报错而不是返回一个猜的 uid", tok)
		}
	}
}

// TestUIDFromAccessTokenRejectsJWTWithoutKnownClaims payload 合法但没有已知声明。
func TestUIDFromAccessTokenRejectsJWTWithoutKnownClaims(t *testing.T) {
	tok := makeJWT(t, map[string]any{"something_else": "x"})
	if _, err := uidFromAccessToken(tok); err == nil {
		t.Error("JWT 里没有 uid/userId/user_id/sub 时应报错，" +
			"否则会给同一个账号编出不同的标识")
	}
}

// TestSanitizeFilePartBlocksPathTraversal uid 里的路径字符必须被中和。
//
// ⚠ uid 来自上游，**不是**我们能信任的输入。`../../evil` 这类值
// 会让凭证写到数据目录之外。
func TestSanitizeFilePartBlocksPathTraversal(t *testing.T) {
	cases := map[string]string{
		"../../evil":      "______evil",
		"a/b":             "a_b",
		`a\b`:             "a_b",
		"..":              "__",
		"":                "unknown",
		"normal-uid_123":  "normal-uid_123",
		"with space":      "with_space",
		"colon:and*star?": "colon_and_star_",
	}
	for in, want := range cases {
		if got := sanitizeFilePart(in); got != want {
			t.Errorf("sanitizeFilePart(%q) = %q，期望 %q", in, got, want)
		}
	}
	// 结果里绝不能出现路径分隔符 —— 那是穿越的必要条件
	for _, in := range []string{"../../evil", `a\b`, "a/b"} {
		got := sanitizeFilePart(in)
		if strings.ContainsAny(got, `/\`) {
			t.Errorf("sanitizeFilePart(%q) = %q 仍含路径分隔符", in, got)
		}
	}
}

// TestSMSLoginWritesLoadableCredential 端到端：登记后**能被 auth.LoadDir 扫到**。
//
// 这是本组最重要的一条：它同时覆盖文件名前缀、落盘格式、uid 写入
// 三件事 —— 任何一件错了，这里都会红。
func TestSMSLoginWritesLoadableCredential(t *testing.T) {
	dir := t.TempDir()
	p := pool.New("")
	fn := newSMSLoginFunc(p, dir)

	uid := "{uuid}"
	tok := makeJWT(t, map[string]any{"uid": uid})

	out, err := fn(server.SMSLoginInput{
		Phone: "13800000000", Region: "cn", AccessToken: tok, RefreshToken: "rt-value",
	})
	if err != nil {
		t.Fatalf("登记失败: %v", err)
	}
	_ = out

	// 1) 文件真的写出来了
	entries, err := os.ReadDir(dir)
	if err != nil {
		t.Fatal(err)
	}
	if len(entries) != 1 {
		t.Fatalf("应写出 1 个凭证文件，实际 %d 个", len(entries))
	}
	name := entries[0].Name()
	if !strings.HasPrefix(name, "workbuddy") {
		t.Errorf("凭证文件名 %q 必须以 `workbuddy` 开头 ——\n"+
			"`auth.LoadDir` 只 glob `workbuddy*.json`，前缀不对会被**静默忽略**：\n"+
			"表现为「登录成功但账号列表里没有它」。", name)
	}
	if !strings.HasSuffix(name, ".json") {
		t.Errorf("凭证文件名 %q 应以 .json 结尾", name)
	}

	// 2) 能被 LoadDir 真正扫到（文件名前缀的终极判据）
	auths, err := auth.LoadDir(dir)
	if err != nil {
		t.Fatal(err)
	}
	if len(auths) != 1 {
		t.Fatalf("auth.LoadDir 只扫到 %d 个账号 —— 文件名或格式不对", len(auths))
	}
	got := auths[0]
	if got.UID != uid {
		t.Errorf("落盘的 uid = %q，期望 %q", got.UID, uid)
	}
	if got.AccessToken != tok {
		t.Error("落盘的 accessToken 与登录返回的不一致")
	}
	if got.RefreshToken != "rt-value" {
		t.Error("落盘的 refreshToken 与登录返回的不一致")
	}
	if got.ProductOf() != "workbuddy" {
		t.Errorf("新账号的产品应为 workbuddy，实际 %q", got.ProductOf())
	}

	// 3) 进了池
	if len(p.List()) != 1 {
		t.Errorf("账号应已进入池，实际池大小 %d", len(p.List()))
	}
}

// TestSMSLoginRegionSetsDomain 区域决定 domain（国际版走 .ai）。
//
// domain 决定后续所有请求打哪个上游 —— 配错会让国际版账号
// 去打国服端点，表现为持续 401，而账号本身完全正常。
func TestSMSLoginRegionSetsDomain(t *testing.T) {
	for _, tc := range []struct{ region, wantDomain string }{
		{"cn", "www.workbuddy.cn"},
		{"intl", "www.workbuddy.ai"},
	} {
		dir := t.TempDir()
		fn := newSMSLoginFunc(pool.New(""), dir)
		tok := makeJWT(t, map[string]any{"uid": "u-" + tc.region})

		if _, err := fn(server.SMSLoginInput{
			Phone: "13800000000", Region: tc.region, AccessToken: tok, RefreshToken: "rt",
		}); err != nil {
			t.Fatalf("region=%s 登记失败: %v", tc.region, err)
		}

		auths, err := auth.LoadDir(dir)
		if err != nil || len(auths) != 1 {
			t.Fatalf("region=%s 读取凭证失败: %v (n=%d)", tc.region, err, len(auths))
		}
		if auths[0].Domain != tc.wantDomain {
			t.Errorf("region=%s 的 domain = %q，期望 %q",
				tc.region, auths[0].Domain, tc.wantDomain)
		}
	}
}

// TestSMSLoginRejectsUnparseableToken 令牌不可解析时**不落盘、不进池**。
//
// 否则会留下一个"文件在、但 uid 是编的"的坏账号，
// 而它指向的凭证其实是有效的 —— 那种账号最难排查。
func TestSMSLoginRejectsUnparseableToken(t *testing.T) {
	dir := t.TempDir()
	p := pool.New("")
	fn := newSMSLoginFunc(p, dir)

	_, err := fn(server.SMSLoginInput{
		Phone: "13800000000", Region: "cn", AccessToken: "not-a-jwt", RefreshToken: "rt",
	})
	if err == nil {
		t.Fatal("无法解析 uid 时应报错")
	}

	// 不能留下任何文件，也不能进池
	entries, _ := os.ReadDir(dir)
	if len(entries) != 0 {
		t.Errorf("解析失败时不应写任何凭证文件，实际写了 %d 个", len(entries))
	}
	if len(p.List()) != 0 {
		t.Errorf("解析失败时不应进池，实际池大小 %d", len(p.List()))
	}
}
