package zcode

// device_mid_test.go —— 钉住「deviceMid 必须稳定，优先用官方客户端那个」。
//
// # 所有者的问题（2026-09-21）
//
//	「还有zcode为什么在官方就不触发,在你这里就触发,你好好看看官方代码
//	  还有参考实现 好好排查」
//
// # 查证结果（三方对照）
//
//	官方客户端   固定值，持久化在 ~/.zcode/v2/telemetry-state.json
//	             （本机实测 = 00000000-0000-4000-8000-0000000000d1）
//	我们的凭证   字段列表里**没有** device_mid
//	旧实现       每次进程启动 NewDeviceMid() 随机一个
//
// 而参考实现明确警告：
//
//	「device fingerprint 稳定：X-Device-Mid 生成一次、永久复用、落盘、
//	  **绝不每请求随机**」
//
// 每次重启换一个设备指纹 ⇒ 上游看到「同一个账号被大量不同设备使用」
// ⇒ 判 unusual activity（3012）。
//
// # 本文件锁两层
//
//	① officialDeviceMid()  —— 读官方文件、只认 UUID、读不到返回空
//	② loadCred 的优先级    —— 凭证字段 > 官方文件 > 随机
//
// ⚠ 不修改官方文件（那是官方客户端的资产，我们只借用标识）。

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
)

// TestOfficialDeviceMidReadsRealFile 能读出官方格式的文件。
func TestOfficialDeviceMidReadsRealFile(t *testing.T) {
	home := t.TempDir()
	t.Setenv("USERPROFILE", home) // Windows 上 os.UserHomeDir 读它
	t.Setenv("HOME", home)        // 类 Unix

	dir := filepath.Join(home, ".zcode", "v2")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	want := "00000000-0000-4000-8000-0000000000d1"
	blob, _ := json.Marshal(map[string]any{
		"deviceMid":           want,
		"lastDailyActiveDate": "2026-09-21",
	})
	if err := os.WriteFile(filepath.Join(dir, "telemetry-state.json"), blob, 0o644); err != nil {
		t.Fatal(err)
	}

	if got := officialDeviceMid(); got != want {
		t.Errorf("应读出官方 deviceMid %q，实际 %q", want, got)
	}
}

// TestOfficialDeviceMidRejectsBadValues 非法值一律返回空（由调用方回退随机）。
//
// 不能把"看起来像但格式不对"的值发出去 —— 上游对非 UUID 回 429/3001，
// 而那会被误判成"账号没额度"。
func TestOfficialDeviceMidRejectsBadValues(t *testing.T) {
	cases := map[string]string{
		"缺 deviceMid 字段": `{"lastDailyActiveDate":"2026-09-21"}`,
		"deviceMid 为空":   `{"deviceMid":""}`,
		"不是 UUID":        `{"deviceMid":"not-a-uuid"}`,
		"长度差一位":          `{"deviceMid":"00000000-0000-4000-8000-00000000001"}`,
		"不是 JSON":        `not json at all`,
		"JSON 数组":        `["00000000-0000-4000-8000-0000000000d1"]`,
	}
	for name, content := range cases {
		home := t.TempDir()
		t.Setenv("USERPROFILE", home)
		t.Setenv("HOME", home)
		dir := filepath.Join(home, ".zcode", "v2")
		if err := os.MkdirAll(dir, 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(dir, "telemetry-state.json"),
			[]byte(content), 0o644); err != nil {
			t.Fatal(err)
		}
		if got := officialDeviceMid(); got != "" {
			t.Errorf("%s：应返回空串（让调用方回退随机），实际 %q", name, got)
		}
	}
}

// TestOfficialDeviceMidMissingFileIsEmpty 文件不存在时返回空，不 panic。
//
// 用户没装官方客户端是最常见的情况 —— 必须安静地回退随机。
func TestOfficialDeviceMidMissingFileIsEmpty(t *testing.T) {
	home := t.TempDir()
	t.Setenv("USERPROFILE", home)
	t.Setenv("HOME", home)

	if got := officialDeviceMid(); got != "" {
		t.Errorf("文件不存在时应返回空串，实际 %q", got)
	}
}

// TestLoadCredPrefersStoredDeviceMid 凭证里**已有**的 device_mid 优先级最高。
//
// 顺序必须是：凭证字段 > 官方文件 > 随机。
// 凭证里的值是"这个账号上次用的"，比官方文件更贴近该账号的历史，
// 且改用它会让同一账号在两次运行间换指纹（正是要避免的）。
func TestLoadCredPrefersStoredDeviceMid(t *testing.T) {
	home := t.TempDir()
	t.Setenv("USERPROFILE", home)
	t.Setenv("HOME", home)
	dir := filepath.Join(home, ".zcode", "v2")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	// 官方文件里放一个**不同**的值
	blob, _ := json.Marshal(map[string]any{"deviceMid": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"})
	if err := os.WriteFile(filepath.Join(dir, "telemetry-state.json"), blob, 0o644); err != nil {
		t.Fatal(err)
	}

	stored := "11111111-2222-3333-4444-555555555555"
	credFile := filepath.Join(t.TempDir(), "zcode-x.json")
	doc, _ := json.Marshal(map[string]any{
		"jwt":        "header.payload.sig",
		"credential": "abc.def",
		"account_id": testOfficialAccount,
		"device_mid": stored,
	})
	if err := os.WriteFile(credFile, doc, 0o644); err != nil {
		t.Fatal(err)
	}

	c, err := LoadFile(credFile)
	if err != nil {
		t.Fatal(err)
	}
	if c.DeviceMid != stored {
		t.Errorf("凭证里已有的 device_mid 应优先（%q），实际 %q", stored, c.DeviceMid)
	}
}

// TestLoadCredFallsBackToOfficial 与官方**同账号**、凭证又没有 device_mid 时，
// 用官方那个（而不是随机）。
//
// ⚠ 2026-09-21 修正语义：借用官方值的前提是「凭证账号 == 官方登录账号」
//（所有者纠正：不同账号共用设备标识 = 一台设备并发一批账号 = 号商特征）。
// 本用例的夹具满足该前提，故仍覆盖"借用"这条路径；
// "不同账号必须各自独立"由 device_mid_peraccount_test.go 覆盖。
func TestLoadCredFallsBackToOfficial(t *testing.T) {
	home := t.TempDir()
	t.Setenv("USERPROFILE", home)
	t.Setenv("HOME", home)
	dir := filepath.Join(home, ".zcode", "v2")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	official := "00000000-0000-4000-8000-0000000000d1"
	blob, _ := json.Marshal(map[string]any{"deviceMid": official})
	if err := os.WriteFile(filepath.Join(dir, "telemetry-state.json"), blob, 0o644); err != nil {
		t.Fatal(err)
	}
	// ⚠ 必须同时造 credentials.json：自 2026-09-21 起"能否借用官方值"
	// 取决于「凭证账号 == 官方登录账号」，见 testOfficialAccount 的注释。
	creds := map[string]any{
		"account-provider:coding-plan:account:zai-individual-coding-plan:account:" +
			testOfficialAccount + ":api-key": "enc:v1:a.b.c",
	}
	cblob, _ := json.Marshal(creds)
	if err := os.WriteFile(filepath.Join(dir, "credentials.json"), cblob, 0o644); err != nil {
		t.Fatal(err)
	}

	credFile := filepath.Join(t.TempDir(), "zcode-y.json")
	// ⚠ 刻意**不含** device_mid —— 所有者本机的真实凭证正是这样
	doc, _ := json.Marshal(map[string]any{
		"jwt":        "header.payload.sig",
		"credential": "abc.def",
		"account_id": testOfficialAccount,
	})
	if err := os.WriteFile(credFile, doc, 0o644); err != nil {
		t.Fatal(err)
	}

	c, err := LoadFile(credFile)
	if err != nil {
		t.Fatal(err)
	}
	if c.DeviceMid != official {
		t.Errorf("凭证没有 device_mid 时应采用官方客户端的值（%q），实际 %q —— "+
			"随机生成会让上游看到「同账号多设备」，那正是 3012 的成因",
			official, c.DeviceMid)
	}
}

// TestLoadCredStableAcrossReloads 反复加载必须得到**同一个** deviceMid。
//
// 这条直接对应"绝不每请求随机"：网关重启后指纹不能变。
func TestLoadCredStableAcrossReloads(t *testing.T) {
	home := t.TempDir()
	t.Setenv("USERPROFILE", home)
	t.Setenv("HOME", home)
	dir := filepath.Join(home, ".zcode", "v2")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	official := "00000000-0000-4000-8000-0000000000d1"
	blob, _ := json.Marshal(map[string]any{"deviceMid": official})
	if err := os.WriteFile(filepath.Join(dir, "telemetry-state.json"), blob, 0o644); err != nil {
		t.Fatal(err)
	}
	// ⚠ 必须同时造 credentials.json：自 2026-09-21 起"能否借用官方值"
	// 取决于「凭证账号 == 官方登录账号」，见 testOfficialAccount 的注释。
	creds := map[string]any{
		"account-provider:coding-plan:account:zai-individual-coding-plan:account:" +
			testOfficialAccount + ":api-key": "enc:v1:a.b.c",
	}
	cblob, _ := json.Marshal(creds)
	if err := os.WriteFile(filepath.Join(dir, "credentials.json"), cblob, 0o644); err != nil {
		t.Fatal(err)
	}
	credFile := filepath.Join(t.TempDir(), "zcode-z.json")
	doc, _ := json.Marshal(map[string]any{
		"jwt":        "header.payload.sig",
		"credential": "abc.def",
		"account_id": testOfficialAccount,
	})
	if err := os.WriteFile(credFile, doc, 0o644); err != nil {
		t.Fatal(err)
	}

	first, err := LoadFile(credFile)
	if err != nil {
		t.Fatal(err)
	}
	for i := 0; i < 5; i++ {
		again, err := LoadFile(credFile)
		if err != nil {
			t.Fatal(err)
		}
		if again.DeviceMid != first.DeviceMid {
			t.Fatalf("第 %d 次加载得到不同的 deviceMid（%q vs %q）—— "+
				"指纹必须稳定，参考实现明确警告「绝不每请求随机」",
				i+1, first.DeviceMid, again.DeviceMid)
		}
	}
}
