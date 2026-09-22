package zcode

// `AccountIDFromJWT` 与 `Cred.AccountID` 的往返语义。
//
// # 为什么要测
//
// `AccountID` 用来判断"本地这几条账号是不是**同一个上游账号的多把 key**"
// —— 那正是所有者看到的"重复"。
//
// 这个判断**只许对、不许猜**：把两个不同账号标成"重复"会让用户
// 误删其中一个。故：
//   · 解不出时返回空串（"未知"），绝不用 uid 之类的东西顶替
//   · 值必须能**跨保存/加载往返**，否则每次加载都退化

import (
	"encoding/base64"
	"encoding/json"
	"path/filepath"
	"strings"
	"testing"
)

// makeJWT 拼一个未签名的 JWT（只为测试 payload 解析）。
func makeJWT(t *testing.T, claims map[string]any) string {
	t.Helper()
	header := base64.RawURLEncoding.EncodeToString([]byte(`{"alg":"HS256","typ":"JWT"}`))
	body, err := json.Marshal(claims)
	if err != nil {
		t.Fatal(err)
	}
	payload := base64.RawURLEncoding.EncodeToString(body)
	return header + "." + payload + ".sig"
}

func TestAccountIDFromJWT(t *testing.T) {
	cases := []struct {
		name   string
		token  string
		expect string
	}{
		{"正常 user_id", makeJWT(t, map[string]any{"user_id": "12345678901234567"}), "12345678901234567"},
		{"只有 sub", makeJWT(t, map[string]any{"sub": "only-sub"}), "only-sub"},
		{"user_id 优先于 sub", makeJWT(t, map[string]any{"user_id": "u", "sub": "s"}), "u"},
		{"UUID 形态", makeJWT(t, map[string]any{"user_id": "00000000-0000-4000-8000-000000000001"}), "00000000-0000-4000-8000-000000000001"},
		{"无相关字段", makeJWT(t, map[string]any{"iat": 123}), ""},
		{"空串", "", ""},
		{"不是 JWT（两段）", "aaa.bbb", ""},
		{"不是 JWT（四段）", "a.b.c.d", ""},
		{"payload 不是 base64", "aaa.!!!.ccc", ""},
		{"payload 不是 JSON", "aaa." + base64.RawURLEncoding.EncodeToString([]byte("not json")) + ".ccc", ""},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			if got := AccountIDFromJWT(c.token); got != c.expect {
				t.Errorf("AccountIDFromJWT = %q，期望 %q", got, c.expect)
			}
		})
	}
}

// TestAccountIDSurvivesSaveLoad AccountID 必须能跨保存/加载往返。
//
// 否则每次加载都退化成空，"同一账号多把 key"的识别就失效了 ——
// 而那正是所有者报的"重复"问题的判据。
func TestAccountIDSurvivesSaveLoad(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "zcode-accid.json")

	jwt := makeJWT(t, map[string]any{"user_id": "acct-777"})
	c := &Cred{
		UID:        "zcode-abc",
		Credential: "key.secret",
		Provider:   ProviderBigmodel,
		JWT:        jwt,
		AccountID:  AccountIDFromJWT(jwt),
		FilePath:   p,
	}
	if c.AccountID != "acct-777" {
		t.Fatalf("前置条件失败：AccountID = %q", c.AccountID)
	}
	if err := c.SaveAtomic(); err != nil {
		t.Fatal(err)
	}

	again, err := LoadFile(p)
	if err != nil {
		t.Fatal(err)
	}
	if again.AccountID != "acct-777" {
		t.Errorf("往返后 AccountID 变成 %q（应为 acct-777）", again.AccountID)
	}
}

// TestAccountIDFallsBackToJWT 文件里没有 account_id 时，从 JWT 现解。
//
// 老版本写出的凭证文件里没有这个字段；加载时应能从 JWT 补出来，
// 而不是显示"未知"。
func TestAccountIDFallsBackToJWT(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "zcode-legacy.json")

	jwt := makeJWT(t, map[string]any{"user_id": "legacy-acct"})
	// 手写一个**不含** account_id 的文件（模拟老版本）
	body, _ := json.Marshal(map[string]any{
		"uid":        "zcode-x",
		"credential": "k.s",
		"provider":   "bigmodel",
		"jwt":        jwt,
	})
	if err := writeTestFile(p, string(body)); err != nil {
		t.Fatal(err)
	}

	c, err := LoadFile(p)
	if err != nil {
		t.Fatal(err)
	}
	if c.AccountID != "legacy-acct" {
		t.Errorf("应从 JWT 补出 AccountID，实际 %q", c.AccountID)
	}
}

// TestAccountIDEmptyWhenNoJWT 没有 JWT 时保持空（不猜）。
//
// 只导入了对话凭证的账号本来就没有账号标识。此时**必须**留空 ——
// 若拿 uid 顶替，界面会把两个不同账号误判成同一个。
func TestAccountIDEmptyWhenNoJWT(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "zcode-nojwt.json")

	c := &Cred{UID: "zcode-y", Credential: "k.s", Provider: ProviderZAI, FilePath: p}
	if err := c.SaveAtomic(); err != nil {
		t.Fatal(err)
	}
	again, err := LoadFile(p)
	if err != nil {
		t.Fatal(err)
	}
	if again.AccountID != "" {
		t.Errorf("没有 JWT 时 AccountID 应为空，实际 %q", again.AccountID)
	}
}

// TestSameAccountDifferentKeysShareAccountID 同账号多把 key 共享 AccountID。
//
// 这正是"重复"的判据：uid 不同（凭证哈希不同），但 AccountID 相同。
func TestSameAccountDifferentKeysShareAccountID(t *testing.T) {
	jwtA := makeJWT(t, map[string]any{"user_id": "same-acct"})
	jwtB := makeJWT(t, map[string]any{"user_id": "same-acct"})
	jwtC := makeJWT(t, map[string]any{"user_id": "other-acct"})

	a := &Cred{Credential: "keyA.secretA", JWT: jwtA}
	b := &Cred{Credential: "keyB.secretB", JWT: jwtB}
	c := &Cred{Credential: "keyC.secretC", JWT: jwtC}
	for _, x := range []*Cred{a, b, c} {
		x.UID = CredKey(x.Credential)
		x.AccountID = AccountIDFromJWT(x.JWT)
	}

	if a.UID == b.UID {
		t.Fatal("前置条件失败：两把不同的 key 应得到不同的 uid")
	}
	if a.AccountID != b.AccountID {
		t.Errorf("同一账号的两把 key 应有相同 AccountID：%q vs %q", a.AccountID, b.AccountID)
	}
	if a.AccountID == c.AccountID {
		t.Error("不同账号不该有相同 AccountID")
	}

	// 且 AccountID 里不该含凭证内容（它会被展示在界面上）
	if strings.Contains(a.AccountID, "secret") || strings.Contains(a.AccountID, "keyA") {
		t.Errorf("AccountID 不该含凭证内容：%q", a.AccountID)
	}
}
