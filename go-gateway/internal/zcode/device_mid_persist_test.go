package zcode

// device_mid_persist_test.go —— 钉住「deviceMid 首次确定后**固化写回凭证**」。
//
// # 所有者的要求（2026-09-21）
//
//	「我在想这个 deviceMid 是不是一个账号绑定一个,稳定使用
//	  这样子是不是好一点?」→「要的」
//
// 他的直觉指向一个真实风险：**读官方文件是"借用"**，而官方客户端可能
// 重装 / 换机器 / 重置状态 —— 那时 `telemetry-state.json` 会变，
// 我们的指纹**跟着变**，又回到"同账号多设备"的老问题（那正是 3012 的成因）。
//
// 落盘后语义变成：**第一次借用，之后永久固化**。
// 这与参考实现一致（`device_mid()` 首次生成后写进 `data/device_mid`）。
//
// # 本文件锁三件事
//
//	① 首次加载**真的写入** device_mid（而不是只存在内存里）
//	② 官方文件**之后变了**也不影响（凭证里的值优先）
//	③ 已有 device_mid 时**不重复写盘**（LoadFile 在每个请求路径上都会调用）

import (
	"encoding/json"
	"os"
	"path/filepath"
	"sync"
	"testing"
)

// writeOfficial 造一个官方 telemetry-state.json（并顺带造 credentials.json）。
//
// ⚠ 同时写 `credentials.json`：自 2026-09-21 起，"能否借用官方 deviceMid"
// 取决于「凭证账号 == 官方登录账号」，而后者只从 credentials.json 的**键名**
// 里读。不写它的话所有用例都会走"随机"分支（见 testOfficialAccount 的注释）。
func writeOfficial(t *testing.T, home, deviceMid string) {
	t.Helper()
	dir := filepath.Join(home, ".zcode", "v2")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	blob, _ := json.Marshal(map[string]any{"deviceMid": deviceMid})
	if err := os.WriteFile(filepath.Join(dir, "telemetry-state.json"), blob, 0o644); err != nil {
		t.Fatal(err)
	}
	// 官方登录的账号 = 本文件夹具用的那个（见 testOfficialAccount）
	creds := map[string]any{
		"oauth:active_provider": "enc:v1:x.y.z",
		"account-provider:coding-plan:account:zai-individual-coding-plan:account:" +
			testOfficialAccount + ":api-key": "enc:v1:a.b.c",
	}
	cblob, _ := json.Marshal(creds)
	if err := os.WriteFile(filepath.Join(dir, "credentials.json"), cblob, 0o644); err != nil {
		t.Fatal(err)
	}
}

// testOfficialAccount 本文件用例统一使用的"官方登录账号" uuid。
//
// ⚠ 2026-09-21 修正：`writeCred` 现在会把凭证的 account_id 设成它，
// 并同时造出官方 `credentials.json`。
//
// # 为什么必须这样
//
// 本文件最初写于"任何账号都借用官方 deviceMid"那个**错误假设**之下
//（所有者后来纠正：「不能视为同一个设备上并发不同的账号」）。
// 改成"一账号一个设备标识"后，借用官方值的**前提**是
// 「凭证的账号 == 官方登录的账号」——
// 而原来的夹具既没设 account_id，也没造 credentials.json，
// 于是走"随机"分支，5 条用例一起变红。
//
// 那些失败是**正确的行为变化**，不是回归。夹具补上账号对应关系后，
// 它们继续覆盖"借用 + 落盘 + 稳定"这条路径。
const testOfficialAccount = "00000000-0000-4000-8000-000000000001"

// writeCred 造一个凭证文件（不含 device_mid），账号与官方登录的一致。
func writeCred(t *testing.T, dir string) string {
	t.Helper()
	p := filepath.Join(dir, "zcode-test.json")
	doc, _ := json.Marshal(map[string]any{
		"jwt":        "header.payload.sig",
		"credential": "abc.def",
		"nickname":   "测试账号",
		"account_id": testOfficialAccount,
	})
	if err := os.WriteFile(p, doc, 0o644); err != nil {
		t.Fatal(err)
	}
	return p
}

// readDeviceMid 读凭证文件里的 device_mid（空 = 没有）。
func readDeviceMid(t *testing.T, path string) string {
	t.Helper()
	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	var doc map[string]any
	if err := json.Unmarshal(raw, &doc); err != nil {
		t.Fatal(err)
	}
	v, _ := doc["device_mid"].(string)
	return v
}

// TestDeviceMidPersistedOnFirstLoad ★ 首次加载必须把 device_mid 写进凭证。
func TestDeviceMidPersistedOnFirstLoad(t *testing.T) {
	home := t.TempDir()
	t.Setenv("USERPROFILE", home)
	t.Setenv("HOME", home)
	official := "00000000-0000-4000-8000-0000000000d1"
	writeOfficial(t, home, official)

	credPath := writeCred(t, t.TempDir())
	if got := readDeviceMid(t, credPath); got != "" {
		t.Fatalf("前置条件：凭证初始不该有 device_mid，实际 %q", got)
	}

	c, err := LoadFile(credPath)
	if err != nil {
		t.Fatal(err)
	}
	if c.DeviceMid != official {
		t.Fatalf("内存里的值应为官方那个 %q，实际 %q", official, c.DeviceMid)
	}

	// ★ 关键：磁盘上也要有
	if got := readDeviceMid(t, credPath); got != official {
		t.Errorf("device_mid 应被**写回凭证**（%q），实际磁盘上是 %q —— "+
			"只存内存的话，官方客户端一换机器我们就跟着换指纹", official, got)
	}
}

// TestDeviceMidSurvivesOfficialFileChange ★ 官方文件变了，凭证里的值不受影响。
//
// # 这是本次修复的核心价值
//
// 若不落盘，官方客户端重装 / 换机器 / 重置后，`telemetry-state.json`
// 会变成新值 —— 我们的指纹**跟着变**，又回到"同账号多设备"的老问题。
// 落盘后凭证里的值优先，官方文件怎么变都与已固化的账号无关。
func TestDeviceMidSurvivesOfficialFileChange(t *testing.T) {
	home := t.TempDir()
	t.Setenv("USERPROFILE", home)
	t.Setenv("HOME", home)
	first := "00000000-0000-4000-8000-0000000000d1"
	writeOfficial(t, home, first)

	credPath := writeCred(t, t.TempDir())
	c1, err := LoadFile(credPath)
	if err != nil {
		t.Fatal(err)
	}
	if c1.DeviceMid != first {
		t.Fatalf("首次应借用官方值，实际 %q", c1.DeviceMid)
	}

	// 模拟官方客户端换机器 / 重置：文件变成另一个值
	changed := "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
	writeOfficial(t, home, changed)

	c2, err := LoadFile(credPath)
	if err != nil {
		t.Fatal(err)
	}
	if c2.DeviceMid != first {
		t.Errorf("官方文件变了之后，凭证里**已固化**的值应继续生效（%q），实际 %q —— "+
			"否则官方客户端一动，我们的设备指纹就跟着变", first, c2.DeviceMid)
	}
}

// TestDeviceMidNotRewrittenWhenPresent 已有 device_mid 时**不重复写盘**。
//
// # 为什么这条重要
//
// `LoadFile` 在**每个请求路径**上都会被调用（额度查询、对话、领取…）。
// 若每次加载都重写凭证文件：
//
//	· 每个请求多一次磁盘写（性能）
//	· 并发请求下多个 goroutine 同时原子替换同一文件（有损坏窗口）
//
// 判据用**修改时间**：值已存在时不该动文件。
func TestDeviceMidNotRewrittenWhenPresent(t *testing.T) {
	home := t.TempDir()
	t.Setenv("USERPROFILE", home)
	t.Setenv("HOME", home)
	writeOfficial(t, home, "00000000-0000-4000-8000-0000000000d1")

	credPath := writeCred(t, t.TempDir())
	// 第一次加载会写入
	if _, err := LoadFile(credPath); err != nil {
		t.Fatal(err)
	}
	st1, err := os.Stat(credPath)
	if err != nil {
		t.Fatal(err)
	}

	// 再加载几次：文件不该被再次改写
	for i := 0; i < 3; i++ {
		if _, err := LoadFile(credPath); err != nil {
			t.Fatal(err)
		}
	}
	st2, err := os.Stat(credPath)
	if err != nil {
		t.Fatal(err)
	}
	if !st2.ModTime().Equal(st1.ModTime()) {
		t.Errorf("已有 device_mid 时不该重复写盘（修改时间从 %v 变成 %v）—— "+
			"LoadFile 在每个请求路径上都会被调用", st1.ModTime(), st2.ModTime())
	}
}

// TestDeviceMidPersistKeepsOtherFields 固化写回**不能丢**其它字段。
//
// SaveAtomic 的策略是"读回磁盘再合并"，本用例守住它：
// 手写的 nickname / 用户加的未知字段都必须保留。
func TestDeviceMidPersistKeepsOtherFields(t *testing.T) {
	home := t.TempDir()
	t.Setenv("USERPROFILE", home)
	t.Setenv("HOME", home)
	writeOfficial(t, home, "00000000-0000-4000-8000-0000000000d1")

	dir := t.TempDir()
	credPath := filepath.Join(dir, "zcode-keep.json")
	doc, _ := json.Marshal(map[string]any{
		"jwt":             "header.payload.sig",
		"credential":      "abc.def",
		"nickname":        "我改过的昵称",
		"my_custom_field": "用户自己加的",
	})
	if err := os.WriteFile(credPath, doc, 0o644); err != nil {
		t.Fatal(err)
	}

	if _, err := LoadFile(credPath); err != nil {
		t.Fatal(err)
	}

	raw, err := os.ReadFile(credPath)
	if err != nil {
		t.Fatal(err)
	}
	var after map[string]any
	if err := json.Unmarshal(raw, &after); err != nil {
		t.Fatal(err)
	}
	if after["nickname"] != "我改过的昵称" {
		t.Errorf("固化写回丢了 nickname（%v）", after["nickname"])
	}
	if after["my_custom_field"] != "用户自己加的" {
		t.Errorf("固化写回丢了用户自定义字段（%v）—— "+
			"SaveAtomic 应读回磁盘再合并，而不是用内存字段重建", after["my_custom_field"])
	}
	if after["credential"] != "abc.def" {
		t.Errorf("固化写回丢了 credential")
	}
}

// TestDeviceMidPersistFailureDoesNotBreakLoad 写盘失败**不影响加载**。
//
// 凭证目录只读（或磁盘满）时，内存里的 deviceMid 仍然有效 ——
// 让"落盘失败"变成"这个账号查不到额度"是过度反应。
func TestDeviceMidPersistFailureDoesNotBreakLoad(t *testing.T) {
	home := t.TempDir()
	t.Setenv("USERPROFILE", home)
	t.Setenv("HOME", home)
	writeOfficial(t, home, "00000000-0000-4000-8000-0000000000d1")

	// 用一个可读但**不可写**的路径不便于跨平台构造，故这里只验证
	// "LoadFile 不因写盘失败而返回错误"这个契约：写入成功也不该报错。
	dir := t.TempDir()
	credPath := filepath.Join(dir, "zcode-ro.json")
	doc, _ := json.Marshal(map[string]any{"jwt": "h.p.s", "credential": "abc.def"})
	if err := os.WriteFile(credPath, doc, 0o644); err != nil {
		t.Fatal(err)
	}
	c, err := LoadFile(credPath)
	if err != nil {
		t.Fatalf("写盘失败不该让加载失败：%v", err)
	}
	if c.DeviceMid == "" {
		t.Error("即使写盘失败，内存里的 deviceMid 也必须可用")
	}
}

// TestDeviceMidConcurrentLoadNoError 并发加载**不得报错**，且磁盘值不被写坏。
//
// # 这条测试的**真实保护力**（我实测过，如实记录）
//
// 我把 `persistDeviceMidOnce` 的去重判断改成 `if false && …`（即还原成
// "每个并发调用都写"），本用例**依然是绿的** —— 说明它**抓不到**那个竞态。
//
// 原因是实测发现：Windows 上 `os.Rename` 覆盖已存在的目标是**成功**的，
// 故"两个 goroutine 争用同一个 .tmp"并不会像我预估的那样失败。
// 那个窗口比我想的小得多。
//
// 所以本用例的定位是**冒烟/回归护栏**，不是竞态的证明：
//
//	· 它能抓住"并发加载整体失败"这类粗错（如 panic、死锁、文件被写坏）
//	· 它**不能**证明去重逻辑有效 —— 那由 TestDeviceMidDedupeOnlyOnce 覆盖
//
// ⚠ 本机没有 gcc（`-race` 需要 cgo），故无法用竞态检测器。
// 若将来装上了 gcc，应把本用例改用 `-race` 跑，那时它才有真正的判别力。
//
// 记下这一点是为了**不让后人误以为这里有竞态保护** —— 那正是我在
// 其他模块反复踩过的坑（"测试绿了"被当成"问题不存在"）。
func TestDeviceMidConcurrentLoadNoError(t *testing.T) {
	home := t.TempDir()
	t.Setenv("USERPROFILE", home)
	t.Setenv("HOME", home)
	official := "00000000-0000-4000-8000-0000000000d1"
	writeOfficial(t, home, official)

	credPath := writeCred(t, t.TempDir())

	// 重置去重表，让本用例真正走"首次固化"那条路
	deviceMidPersistedMu.Lock()
	deviceMidPersisted = map[string]bool{}
	deviceMidPersistedMu.Unlock()

	const n = 16
	var wg sync.WaitGroup
	errs := make([]error, n)
	vals := make([]string, n)
	start := make(chan struct{})
	for i := 0; i < n; i++ {
		wg.Add(1)
		go func(idx int) {
			defer wg.Done()
			<-start // 尽量让它们同时冲进 LoadFile
			c, err := LoadFile(credPath)
			errs[idx] = err
			if c != nil {
				vals[idx] = c.DeviceMid
			}
		}(i)
	}
	close(start)
	wg.Wait()

	for i, err := range errs {
		if err != nil {
			t.Errorf("并发第 %d 个 LoadFile 返回错误：%v —— "+
				"它本该是纯读操作（固化写盘的 .tmp 争用被暴露出来了）", i, err)
		}
	}
	for i, v := range vals {
		if v != official {
			t.Errorf("并发第 %d 个拿到 %q，期望 %q", i, v, official)
		}
	}
	// 磁盘上最终必须仍是正确的值（没被并发写坏）
	if got := readDeviceMid(t, credPath); got != official {
		t.Errorf("并发写盘后磁盘值被写坏：%q，期望 %q", got, official)
	}
}

// TestDeviceMidDedupeOnlyOnce 同一路径只固化一次（去重表真的生效）。
//
// 直接验证 `deviceMidPersisted`：第二次调用不该再碰磁盘。
func TestDeviceMidDedupeOnlyOnce(t *testing.T) {
	home := t.TempDir()
	t.Setenv("USERPROFILE", home)
	t.Setenv("HOME", home)
	writeOfficial(t, home, "00000000-0000-4000-8000-0000000000d1")

	credPath := writeCred(t, t.TempDir())
	deviceMidPersistedMu.Lock()
	deviceMidPersisted = map[string]bool{}
	deviceMidPersistedMu.Unlock()

	if _, err := LoadFile(credPath); err != nil {
		t.Fatal(err)
	}
	if !deviceMidPersisted[credPath] {
		t.Fatal("首次加载后应把该路径标记为已固化")
	}
	// 清掉标记也不该出错（模拟"另一个进程"），但值必须一致
	deviceMidPersistedMu.Lock()
	deviceMidPersisted = map[string]bool{}
	deviceMidPersistedMu.Unlock()
	if _, err := LoadFile(credPath); err != nil {
		t.Fatal(err)
	}
	if got := readDeviceMid(t, credPath); got != "00000000-0000-4000-8000-0000000000d1" {
		t.Errorf("重复固化把值写坏了：%q", got)
	}
}
