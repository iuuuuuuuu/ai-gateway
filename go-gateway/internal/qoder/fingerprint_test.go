package qoder

// 机器指纹**必须跨加载稳定** —— 这是一个真实 bug 的回归测试。
//
// ## bug 是什么
//
// `EnsureFingerprint` 会生成 MachineID / MachineToken / MachineType，
// 而 `Cred.MachineID` 的注释明确写了为什么必须持久化：
//
//	上游把「同一账号 + 同一机器指纹」视为同一设备。
//	每次重启都换一组指纹会让上游看到「同一账号从大量不同设备登录」，
//	可能触发风控。
//
// **但读写是不对称的**（实测发现）：
//
//	SaveAtomic  只写 auth / account 两段 —— machine 段从来没落盘
//	LoadFile    只读 auth / account 两段 —— machine 段从来没读回来
//
// 于是每次加载都生成一套**新的**指纹并写回，下一次又丢 —— 正好
// 制造了注释里警告的那个现象。注释写对了，代码没落实。
//
// 这条测试钉住的是**往返**（写出去 → 读回来 → 值不变），
// 而不是"某个函数会生成指纹"。

import (
	"os"
	"path/filepath"
	"testing"
)

// TestFingerprintSurvivesSaveLoad 指纹写出去后能读回来，且值不变。
func TestFingerprintSurvivesSaveLoad(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "qoder-fp.json")

	c := &Cred{
		UID:      "u-fp",
		Nickname: "n",
		DT:       "dt-token",
		DRT:      "drt-token",
		Region:   RegionCN,
	}
	if !c.EnsureFingerprint() {
		t.Fatal("首次应生成指纹（changed 应为 true）")
	}
	mid, mtok, mtype := c.MachineID, c.MachineToken, c.MachineType
	c.FilePath = p
	if err := c.SaveAtomic(); err != nil {
		t.Fatal(err)
	}

	// 重新加载：指纹必须**原样**回来
	again, err := LoadFile(p)
	if err != nil {
		t.Fatal(err)
	}
	if again.MachineID != mid {
		t.Errorf("MachineID 没保住：保存前 %q，加载后 %q\n"+
			"（每次换指纹会让上游看到「同一账号从大量不同设备登录」）",
			mid, again.MachineID)
	}
	if again.MachineToken != mtok {
		t.Errorf("MachineToken 没保住：%q → %q", mtok, again.MachineToken)
	}
	if again.MachineType != mtype {
		t.Errorf("MachineType 没保住：%q → %q", mtype, again.MachineType)
	}

	// 且不该再被认为"需要补"（否则就是又生成了一套）
	if again.EnsureFingerprint() {
		t.Error("往返后 EnsureFingerprint 仍报 changed —— 说明指纹丢了，正在生成新的")
	}
}

// TestFingerprintStableAcrossRepeatedSaves 反复保存不变。
//
// 模拟"网关重启多次"：每次都 load → 保存，指纹应始终是同一个。
func TestFingerprintStableAcrossRepeatedSaves(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "qoder-fp2.json")

	c := &Cred{UID: "u-fp2", DT: "dt", DRT: "drt", Region: RegionCN}
	c.EnsureFingerprint()
	c.FilePath = p
	if err := c.SaveAtomic(); err != nil {
		t.Fatal(err)
	}
	first := c.MachineID

	for i := 0; i < 5; i++ {
		loaded, err := LoadFile(p)
		if err != nil {
			t.Fatal(err)
		}
		if loaded.MachineID != first {
			t.Fatalf("第 %d 轮加载后指纹变了：%q → %q", i+1, first, loaded.MachineID)
		}
		// 模拟网关的常规行为：加载后可能再保存一次（令牌刷新等）
		if err := loaded.SaveAtomic(); err != nil {
			t.Fatal(err)
		}
	}
}

// TestFingerprintAbsentStaysAbsent 文件里没有 machine 段时不该凭空造。
//
// `SaveAtomic` 只在有值时才写 machine 段 —— 这对"用户手写的凭证文件"
// 很重要：不该因为我们读了一次就给它加上一堆字段。
func TestFingerprintAbsentStaysAbsent(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "qoder-fp3.json")
	body := `{"auth":{"accessToken":"dt","refreshToken":"drt","expiresAt":1,"domain":"qoder.com.cn"},"account":{"uid":"u3"}}`
	if err := os.WriteFile(p, []byte(body), 0o600); err != nil {
		t.Fatal(err)
	}

	c, err := LoadFile(p)
	if err != nil {
		t.Fatal(err)
	}
	if c.MachineID != "" {
		t.Errorf("原文件没有 machine 段，不该读出一个值：%q", c.MachineID)
	}

	// 直接保存（不调 EnsureFingerprint）→ 不该凭空加 machine 段
	if err := c.SaveAtomic(); err != nil {
		t.Fatal(err)
	}
	again, err := LoadFile(p)
	if err != nil {
		t.Fatal(err)
	}
	if again.MachineID != "" {
		t.Errorf("没调用 EnsureFingerprint 就不该造出指纹，实际 %q", again.MachineID)
	}
}

// TestEnsureFingerprintIdempotent 有值时不再改（幂等）。
func TestEnsureFingerprintIdempotent(t *testing.T) {
	c := &Cred{
		MachineID:    "keep-id",
		MachineToken: "keep-token",
		MachineType:  "5",
	}
	if c.EnsureFingerprint() {
		t.Error("三项都齐时不该报 changed")
	}
	if c.MachineID != "keep-id" || c.MachineToken != "keep-token" || c.MachineType != "5" {
		t.Error("EnsureFingerprint 不该覆盖已有的值")
	}
}

// TestDeterministicUUID 由 uid 派生的指纹是**稳定**的。
//
// 用途：客户端没落盘 machine-id 时给一个稳定值。
// 若每次随机，就又回到"每次都是新设备"的老问题。
func TestDeterministicUUID(t *testing.T) {
	a := deterministicUUID("uid-1")
	b := deterministicUUID("uid-1")
	c := deterministicUUID("uid-2")

	if a != b {
		t.Errorf("同一 uid 应得到同一指纹：%q vs %q", a, b)
	}
	if a == c {
		t.Error("不同 uid 应得到不同指纹")
	}
	// 形态：8-4-4-4-12 且是 v4
	if len(a) != 36 || a[14] != '4' {
		t.Errorf("应是 UUID v4 形态，实际 %q", a)
	}
	for i, ch := range a {
		switch i {
		case 8, 13, 18, 23:
			if ch != '-' {
				t.Errorf("第 %d 位应是 '-'，实际 %q（%s）", i, ch, a)
			}
		default:
			if !((ch >= '0' && ch <= '9') || (ch >= 'a' && ch <= 'f')) {
				t.Errorf("第 %d 位应是十六进制，实际 %q（%s）", i, ch, a)
			}
		}
	}
}
