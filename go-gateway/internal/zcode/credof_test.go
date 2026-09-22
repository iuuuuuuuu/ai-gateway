package zcode

// credof_test.go 钉住 credOf 的字段完整性。
//
// # 为什么必须有这组测试（2026-09-21 实测缺陷）
//
// 症状：同一份凭证、同一个模型，
//
//	独立探针（LoadDir/LoadFile 拿到完整 Cred）  → HTTP 200 ✓
//	走网关（server → dispatch → credOf）        → 恒 503「无法连接上游」
//
// 根因：`auth.Auth` 只承载选号需要的字段，而 `credOf` 此前只填了
// UID/Nickname/Credential/FilePath/Provider —— **丢了 JWT**。
// 而 `applyHeaders` 的鉴权顺序是
//
//	token := cr.JWT; if token == "" { token = cr.Credential }
//
// 于是网关永远拿 `{apiKey}.{secret}` 形态的 Credential 去打 start-plan
// 通道（Anthropic Messages）—— 那是**另一条通道**的凭证。实测上游回
// `429 限流`，而真正的表现（网关侧）是恒 503，把排查方向引向"网络/代理"。
//
// 同时 `isStartPlan()` 只看 JWT，丢了它还会**选错端点**。
//
// # 为什么用假凭证文件而不是 mock
//
// 本测试要钉住的正是"从文件回读"这个动作本身。若用内存里的假 Cred，
// 就绕过了 `LoadFile`，而缺陷恰好就出在"以为 auth.Auth 够用、没去读文件"。

import (
	"os"
	"path/filepath"
	"testing"

	"workbuddy2api/internal/auth"
)

// writeCredFile 在临时目录写一份凭证文件，返回路径。
func writeCredFile(t *testing.T, name, body string) string {
	t.Helper()
	dir := t.TempDir()
	p := filepath.Join(dir, name)
	if err := os.WriteFile(p, []byte(body), 0o600); err != nil {
		t.Fatalf("写凭证文件失败: %v", err)
	}
	return p
}

// TestCredOfKeepsJWT 核心回归：credOf 必须把 JWT 带出来。
//
// 这是本次缺陷的直接断言 —— 若哪天有人"简化"掉回读逻辑，这条会红。
func TestCredOfKeepsJWT(t *testing.T) {
	const jwt = "eyJhbGciOiJIUzI1NiJ9.fake-payload.fake-signature"
	const cred = "ak-abcdefghijklmnop.secretsecretsecret"
	const devMid = "11111111-2222-3333-4444-555555555555"
	const acctID = "12345678901234567"

	p := writeCredFile(t, "zcode-test.json", `{
  "uid": "zcode-test",
  "nickname": "测试",
  "provider": "zai",
  "credential": "`+cred+`",
  "jwt": "`+jwt+`",
  "jwt_issued_at": 1789780969,
  "device_mid": "`+devMid+`",
  "account_id": "`+acctID+`"
}`)

	// 模拟 main.go 的 zcodeAuthOf：只把选号需要的字段放进 auth.Auth
	a := &auth.Auth{
		UID:         "zcode-test",
		Nickname:    "测试",
		AccessToken: cred,
		Domain:      DomainOfProvider(ProviderZAI),
		FilePath:    p,
		Product:     auth.ProductZcode,
	}

	got := credOf(a)
	if got == nil {
		t.Fatal("credOf 返回 nil")
	}

	// ① JWT：鉴权头与端点选择都靠它 —— 缺了必失败
	if got.JWT != jwt {
		t.Errorf("JWT 丢了：got=%q want=%q\n"+
			"  这是 2026-09-21 那个「网关恒 503」缺陷的直接原因：\n"+
			"  applyHeaders 会用 Credential（按量通道凭证）去打 start-plan 通道",
			got.JWT, jwt)
	}
	if !got.isStartPlan() {
		t.Error("isStartPlan() 为 false —— 端点会被选成按量计费通道（错误的那条）")
	}
	// ② DeviceMid：metadata.user_id 的 device_id
	if got.DeviceMid != devMid {
		t.Errorf("DeviceMid 丢了：got=%q want=%q", got.DeviceMid, devMid)
	}
	// ③ AccountID：x-session-id / trace-id 的稳定标识
	if got.AccountID != acctID {
		t.Errorf("AccountID 丢了：got=%q want=%q", got.AccountID, acctID)
	}
	// ④ 选号字段仍要原样保留（回读不能覆盖它们）
	if got.UID != "zcode-test" || got.Credential != cred || got.FilePath != p {
		t.Errorf("回读覆盖了选号字段：UID=%q Credential=%q FilePath=%q",
			got.UID, got.Credential, got.FilePath)
	}
	if got.Provider != ProviderZAI {
		t.Errorf("Provider=%q want=%q", got.Provider, ProviderZAI)
	}
}

// TestCredOfSurvivesMissingFile 文件不存在时必须**优雅降级**而不是 panic。
//
// 凭证可能在网关运行期被删掉（用户在界面上移除账号），而池里可能还留着
// 该账号的引用。此时 credOf 应当返回只带 Credential 的对象 ——
// 行为与修复前一致（由 applyHeaders 的回落兜底），不 panic。
func TestCredOfSurvivesMissingFile(t *testing.T) {
	a := &auth.Auth{
		UID:         "zcode-gone",
		AccessToken: "ak-x.secret",
		Domain:      DomainOfProvider(ProviderZAI),
		FilePath:    filepath.Join(t.TempDir(), "does-not-exist.json"),
		Product:     auth.ProductZcode,
	}

	got := credOf(a)
	if got == nil {
		t.Fatal("credOf 在文件缺失时返回 nil —— 会让请求直接失败而不是回落")
	}
	if got.Credential != "ak-x.secret" {
		t.Errorf("Credential 应保留（回落用）：got=%q", got.Credential)
	}
	if got.JWT != "" {
		t.Errorf("文件缺失时 JWT 应为空，got=%q", got.JWT)
	}
}

// TestCredOfNilSafe nil 输入返回 nil（调用方据此报「账号缺凭证」）。
func TestCredOfNilSafe(t *testing.T) {
	if got := credOf(nil); got != nil {
		t.Errorf("credOf(nil) 应返回 nil，got=%+v", got)
	}
}

// TestCredOfUnknownProviderFallsBackZAI 域名认不出服务商时回落 Z.AI。
//
// 与 endpointsOf 的默认行为一致；若这里变了，老凭证（没有 provider 字段的）
// 会突然全部走错端点。
func TestCredOfUnknownProviderFallsBackZAI(t *testing.T) {
	a := &auth.Auth{
		UID:         "zcode-legacy",
		AccessToken: "ak-y.secret",
		Domain:      "", // 老凭证可能没有域名
		FilePath:    filepath.Join(t.TempDir(), "missing.json"),
		Product:     auth.ProductZcode,
	}
	if got := credOf(a); got.Provider != ProviderZAI {
		t.Errorf("未知服务商应回落 ZAI，got=%q", got.Provider)
	}
}
