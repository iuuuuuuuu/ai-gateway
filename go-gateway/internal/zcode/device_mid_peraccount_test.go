package zcode

// device_mid_peraccount_test.go —— 钉住「**一个账号一个设备标识**」。
//
// # 所有者纠正的方向性错误（2026-09-21）
//
// 我上一版让所有账号**共用**官方客户端那一个 deviceMid，理由是
// "与官方共享同一设备身份"。所有者指出这是**反的**：
//
//	「我觉得一个账号一个机器码还是有必要的,可以视为在同一个局域网内的
//	  账号,但不能视为同一个设备上并发不同的账号」
//
// 他说的形状差异：
//
//	共用 deviceMid   设备 X → 账号 A,B,C…T   ← **一台设备并发一批账号**
//	一账号一个       X₁→A, X₂→B, …          ← 多台设备各用一个账号
//
// 前者是**号商 / 脚本**的典型特征；后者的每个账号看起来都像
// "某个人在他自己的机器上用官方客户端"。官方客户端就是后者
//（它只登录一个账号）。
//
// 他的类比很准：共享公网 IP（同一局域网）上游能理解 —— NAT 后面本来
// 就有很多人；但**共享设备标识**不能理解。
//
// # 本文件锁的契约
//
//	① 与官方**同账号** → 借用官方 deviceMid（同账号同设备，真实一致）
//	② 与官方**不同账号** → 各自独立的随机值（不共用）
//	③ 多个账号之间**互不相同**
//	④ 每个账号的值**稳定**（落盘后不变）

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
)

// writeOfficialCredentials 造官方 credentials.json（键名里带账号 uuid）。
func writeOfficialCredentials(t *testing.T, home, accountUUID string) {
	t.Helper()
	dir := filepath.Join(home, ".zcode", "v2")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	// 形状照本机实测：键名嵌 uuid，值是加密串（我们只读键名）
	doc := map[string]any{
		"oauth:active_provider": "enc:v1:xxx.yyy.zzz",
		"account-provider:coding-plan:account:zai-individual-coding-plan:account:" +
			accountUUID + ":api-key": "enc:v1:aaa.bbb.ccc",
	}
	blob, _ := json.Marshal(doc)
	if err := os.WriteFile(filepath.Join(dir, "credentials.json"), blob, 0o644); err != nil {
		t.Fatal(err)
	}
}

// writeCredWithAccount 造一个指定 account_id 的凭证。
func writeCredWithAccount(t *testing.T, dir, name, accountID string) string {
	t.Helper()
	p := filepath.Join(dir, name)
	doc, _ := json.Marshal(map[string]any{
		"jwt":        "header.payload.sig",
		"credential": "abc.def",
		"account_id": accountID,
	})
	if err := os.WriteFile(p, doc, 0o644); err != nil {
		t.Fatal(err)
	}
	return p
}

// TestOfficialAccountUUIDParsesRealShape 能从真实形状的键名里解出 uuid。
func TestOfficialAccountUUIDParsesRealShape(t *testing.T) {
	home := t.TempDir()
	t.Setenv("USERPROFILE", home)
	t.Setenv("HOME", home)
	want := "00000000-0000-4000-8000-000000000001"
	writeOfficialCredentials(t, home, want)

	if got := officialAccountUUID(); got != want {
		t.Errorf("应解出官方账号 uuid %q，实际 %q", want, got)
	}
}

// TestOfficialAccountUUIDMissingIsEmpty 文件缺失/无 uuid 时返回空。
func TestOfficialAccountUUIDMissingIsEmpty(t *testing.T) {
	home := t.TempDir()
	t.Setenv("USERPROFILE", home)
	t.Setenv("HOME", home)
	if got := officialAccountUUID(); got != "" {
		t.Errorf("无官方文件时应返回空串，实际 %q", got)
	}

	// 有文件但键名里没有 account uuid
	dir := filepath.Join(home, ".zcode", "v2")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	blob, _ := json.Marshal(map[string]any{"oauth:active_provider": "x"})
	if err := os.WriteFile(filepath.Join(dir, "credentials.json"), blob, 0o644); err != nil {
		t.Fatal(err)
	}
	if got := officialAccountUUID(); got != "" {
		t.Errorf("键名无 uuid 时应返回空串，实际 %q", got)
	}
}

// TestSameAccountBorrowsOfficialDeviceMid 与官方**同账号** ⇒ 借用官方值。
//
// 这是所有者本机的情形（官方登录 {uuid}…，他的凭证也是它）。
// 同账号同设备 ⇒ 共享标识是真实且一致的。
func TestSameAccountBorrowsOfficialDeviceMid(t *testing.T) {
	home := t.TempDir()
	t.Setenv("USERPROFILE", home)
	t.Setenv("HOME", home)
	acct := "00000000-0000-4000-8000-000000000001"
	officialDev := "00000000-0000-4000-8000-0000000000d1"
	writeOfficialCredentials(t, home, acct)
	writeOfficial(t, home, officialDev)

	credPath := writeCredWithAccount(t, t.TempDir(), "zcode-same.json", acct)
	c, err := LoadFile(credPath)
	if err != nil {
		t.Fatal(err)
	}
	if c.DeviceMid != officialDev {
		t.Errorf("与官方同账号时应借用官方 deviceMid（%q），实际 %q",
			officialDev, c.DeviceMid)
	}
}

// TestDifferentAccountGetsOwnDeviceMid ★ 与官方**不同账号** ⇒ 不共用官方值。
//
// # 这是所有者纠正的核心
//
// 若不同账号也共用官方那个 deviceMid，上游看到的就是
// 「一台设备并发一批账号」—— 号商特征，比"多设备各一账号"可疑得多。
func TestDifferentAccountGetsOwnDeviceMid(t *testing.T) {
	home := t.TempDir()
	t.Setenv("USERPROFILE", home)
	t.Setenv("HOME", home)
	officialAcct := "00000000-0000-4000-8000-000000000001"
	officialDev := "00000000-0000-4000-8000-0000000000d1"
	writeOfficialCredentials(t, home, officialAcct)
	writeOfficial(t, home, officialDev)

	// 一个**不同**的账号
	otherAcct := "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
	credPath := writeCredWithAccount(t, t.TempDir(), "zcode-other.json", otherAcct)
	c, err := LoadFile(credPath)
	if err != nil {
		t.Fatal(err)
	}
	if c.DeviceMid == officialDev {
		t.Errorf("不同账号**不该**共用官方 deviceMid（%q）—— "+
			"那会让上游看到「一台设备并发一批账号」", officialDev)
	}
	if !IsUUID(c.DeviceMid) {
		t.Errorf("应生成一个合法的随机 UUID，实际 %q", c.DeviceMid)
	}
}

// TestAccountsGetDistinctDeviceMids ★★ 多个账号之间**互不相同**。
//
// 这是本特性的核心断言：每个账号一个独立设备标识。
func TestAccountsGetDistinctDeviceMids(t *testing.T) {
	home := t.TempDir()
	t.Setenv("USERPROFILE", home)
	t.Setenv("HOME", home)
	officialAcct := "00000000-0000-4000-8000-000000000001"
	writeOfficialCredentials(t, home, officialAcct)
	writeOfficial(t, home, "00000000-0000-4000-8000-0000000000d1")

	dir := t.TempDir()
	accts := []string{
		officialAcct, // 含官方那个（它会借用官方值）
		"11111111-1111-1111-1111-111111111111",
		"22222222-2222-2222-2222-222222222222",
		"33333333-3333-3333-3333-333333333333",
	}
	seen := map[string]string{} // deviceMid -> accountID
	for i, a := range accts {
		p := writeCredWithAccount(t, dir, "zcode-"+string(rune('a'+i))+".json", a)
		deviceMidPersistedMu.Lock()
		deviceMidPersisted = map[string]bool{}
		deviceMidPersistedMu.Unlock()
		c, err := LoadFile(p)
		if err != nil {
			t.Fatal(err)
		}
		if prev, dup := seen[c.DeviceMid]; dup {
			t.Errorf("账号 %s 与 %s 得到了**相同**的 deviceMid %q —— "+
				"每个账号必须有独立的设备标识（否则上游看到「一台设备并发一批账号」）",
				a, prev, c.DeviceMid)
		}
		seen[c.DeviceMid] = a
	}
	if len(seen) != len(accts) {
		t.Errorf("应有 %d 个不同的 deviceMid，实际 %d 个", len(accts), len(seen))
	}
}

// TestPerAccountDeviceMidStable 每个账号的值落盘后**稳定**（重复加载不变）。
func TestPerAccountDeviceMidStable(t *testing.T) {
	home := t.TempDir()
	t.Setenv("USERPROFILE", home)
	t.Setenv("HOME", home)
	writeOfficialCredentials(t, home, "00000000-0000-4000-8000-000000000001")
	writeOfficial(t, home, "00000000-0000-4000-8000-0000000000d1")

	credPath := writeCredWithAccount(t, t.TempDir(), "zcode-stable.json",
		"99999999-9999-9999-9999-999999999999")

	deviceMidPersistedMu.Lock()
	deviceMidPersisted = map[string]bool{}
	deviceMidPersistedMu.Unlock()

	first, err := LoadFile(credPath)
	if err != nil {
		t.Fatal(err)
	}
	if got := readDeviceMid(t, credPath); got != first.DeviceMid {
		t.Fatalf("首次加载应把值固化到磁盘：内存 %q，磁盘 %q", first.DeviceMid, got)
	}
	// 之后多次加载（含清掉去重表，模拟新进程）都必须一致
	for i := 0; i < 3; i++ {
		deviceMidPersistedMu.Lock()
		deviceMidPersisted = map[string]bool{}
		deviceMidPersistedMu.Unlock()
		again, err := LoadFile(credPath)
		if err != nil {
			t.Fatal(err)
		}
		if again.DeviceMid != first.DeviceMid {
			t.Fatalf("第 %d 次加载得到不同值（%q vs %q）—— "+
				"设备标识必须稳定", i+2, first.DeviceMid, again.DeviceMid)
		}
	}
}

// TestPerAccountSurvivesOfficialLogout 官方客户端登出/换账号不影响已固化的值。
//
// 每个账号的值落盘后与官方文件无关 —— 官方怎么变都不该动我们已固化的标识。
func TestPerAccountSurvivesOfficialLogout(t *testing.T) {
	home := t.TempDir()
	t.Setenv("USERPROFILE", home)
	t.Setenv("HOME", home)
	officialAcct := "00000000-0000-4000-8000-000000000001"
	writeOfficialCredentials(t, home, officialAcct)
	writeOfficial(t, home, "00000000-0000-4000-8000-0000000000d1")

	credPath := writeCredWithAccount(t, t.TempDir(), "zcode-logout.json", officialAcct)
	first, err := LoadFile(credPath)
	if err != nil {
		t.Fatal(err)
	}

	// 官方客户端换账号（或登出）：credentials.json 变成别的 uuid
	writeOfficialCredentials(t, home, "ffffffff-ffff-ffff-ffff-ffffffffffff")

	again, err := LoadFile(credPath)
	if err != nil {
		t.Fatal(err)
	}
	if again.DeviceMid != first.DeviceMid {
		t.Errorf("官方换账号后，已固化的 deviceMid 不该变（%q → %q）",
			first.DeviceMid, again.DeviceMid)
	}
}
