package zcode

// 额度查询的两个**静默失败**模式（都曾被我误判）。
//
// ## 背景：为什么这两条值得单独钉住
//
// 额度查询失败时，**用户看到的是"没有额度"而不是"查询坏了"** ——
// 这两者对他的行动指引完全不同（前者让他去充值/换号，后者让他去报障）。
//
// 实测踩到的两个问题，都属于"错误被包装成成功"：
//
//  1. **缺 X-Device-Mid** → `400 {"code":3001,"msg":"parameter error"}`
//     我把它读成"参数写错了"，进而误判成"只导入凭证的账号查不到额度"。
//  2. **host 用错** → `HTTP 200 {"code":500,"msg":"404 NOT_FOUND"}`
//     注意**状态码是 200**，故不会被 `if resp.StatusCode >= 400` 拦下；
//     若只判状态码就会当成"查到了、但没有余额"。
//
// 两条都有回归测试（下面）。测试的价值不在于"能不能查"，
// 而在于**失败时会不会被误读成成功**。

import (
	"os"
	"strings"
	"testing"
)

// writeTestFile 写一个测试用凭证文件（0600，与生产一致）。
func writeTestFile(path, body string) error {
	return os.WriteFile(path, []byte(body), 0o600)
}

// TestQuotaHostIsSeparateFromBizHost 额度与凭证兑换的 host 不同。
//
// 实测三个 host 对照（uitest/diag-balance-endpoint.cjs）：
//
//	zcode.z.ai          → code=0，完整额度 ✓
//	open.bigmodel.cn    → code=500 "404 NOT_FOUND" ✗
//	api.z.ai            → code=500 "404 NOT_FOUND" ✗
//
// 我最初让额度查询复用 BizHost，于是 bigmodel 账号永远查不到额度，
// 而响应是 200 —— 看起来像"这个账号没额度"。
func TestQuotaHostIsSeparateFromBizHost(t *testing.T) {
	// 两个服务商的额度都走统一的账单网关
	if ProviderBigmodel.QuotaHost() != QuotaHost {
		t.Errorf("bigmodel 的额度 host 应为 %s，实际 %s", QuotaHost, ProviderBigmodel.QuotaHost())
	}
	if ProviderZAI.QuotaHost() != QuotaHost {
		t.Errorf("zai 的额度 host 应为 %s，实际 %s", QuotaHost, ProviderZAI.QuotaHost())
	}

	// 必须与 BizHost 区分开 —— 混用就是那个 bug
	if ProviderBigmodel.QuotaHost() == ProviderBigmodel.BizHost() {
		t.Error("额度 host 与凭证兑换 host 相同 —— bigmodel 会回 404 NOT_FOUND（且状态码是 200）")
	}

	// BizHost 仍应是各服务商自己的（凭证兑换用）
	if !strings.Contains(ProviderBigmodel.BizHost(), "bigmodel") {
		t.Errorf("bigmodel 的 BizHost 应指向 bigmodel，实际 %s", ProviderBigmodel.BizHost())
	}
	if !strings.Contains(ProviderZAI.BizHost(), "z.ai") {
		t.Errorf("zai 的 BizHost 应指向 z.ai，实际 %s", ProviderZAI.BizHost())
	}

	// 额度 host 必须是 zcode.z.ai（实测唯一给出数据的那个）
	if !strings.Contains(QuotaHost, "zcode.z.ai") {
		t.Errorf("额度 host 应含 zcode.z.ai，实际 %s", QuotaHost)
	}
}

// TestControlPlaneNeedsDeviceMid 控制面必须带 X-Device-Mid，LLM 路径必须不带。
//
// 实测（uitest/probe-zcode-quota-auth2.cjs）：
//
//	不带头        → 400 code=3001
//	UUID 格式的头 → **200 code=0，拿到完整额度**
//	非 UUID       → 429
//
// 而 LLM 路径**发了反而暴露**（真实客户端在该路径不发它）。
// 两条路径的要求是**相反**的，故必须各有测试。
func TestControlPlaneNeedsDeviceMid(t *testing.T) {
	id := DefaultIdentity()

	// LLM 路径：不带
	if _, ok := id.Headers()["X-Device-Mid"]; ok {
		t.Error("LLM 请求路径**不该**带 X-Device-Mid（真实客户端不发它，多带会成为区分特征）")
	}

	// 控制面：必须带
	mid := "019fc667-facb-7315-a16e-846d6a0923a4"
	h := id.ControlPlaneHeaders(mid)
	if h["X-Device-Mid"] != mid {
		t.Error("控制面请求**必须**带 X-Device-Mid —— 缺了回 400 code=3001，会被误读成「查不到额度」")
	}
	// 其余头照旧都在
	for _, k := range []string{"User-Agent", "X-Platform", "X-ZCode-App-Version"} {
		if h[k] == "" {
			t.Errorf("控制面请求缺少身份头 %s", k)
		}
	}

	// deviceMid 为空时**不发**这个头（保持原行为，便于显式关闭）
	if _, ok := id.ControlPlaneHeaders("  ")["X-Device-Mid"]; ok {
		t.Error("deviceMid 为空时不该发这个头")
	}
}

// TestDeviceMidFormat 生成的 deviceMid 必须是合法 UUID。
//
// 上游**只校验格式**：非 UUID 字符串会被 429，而任意 UUID 都通过
//（实测随机生成的也通过，不需要是注册过的设备）。
func TestDeviceMidFormat(t *testing.T) {
	for i := 0; i < 50; i++ {
		m := NewDeviceMid()
		if !IsUUID(m) {
			t.Fatalf("生成的 deviceMid 不是合法 UUID: %q", m)
		}
		// v4 形态：第 15 位是 '4'，第 20 位在 89ab 里
		if m[14] != '4' {
			t.Errorf("应是 UUID v4 形态（第 15 位为 4），实际 %q", m)
		}
		if !strings.ContainsRune("89ab", rune(m[19])) {
			t.Errorf("变体位不合法: %q", m)
		}
	}

	// 两次生成必须不同（否则同一台设备会被当成同一账号的指纹）
	if NewDeviceMid() == NewDeviceMid() {
		t.Error("deviceMid 必须每次不同（否则大量账号会共用同一个设备指纹）")
	}

	// IsUUID 的判据
	valid := []string{
		"019fc667-facb-7315-a16e-846d6a0923a4",
		"00000000-0000-4000-8000-000000000000",
		"AABBCCDD-EEFF-0011-2233-445566778899", // 大写也认
	}
	for _, v := range valid {
		if !IsUUID(v) {
			t.Errorf("应判为 UUID: %q", v)
		}
	}
	// 实测这些取值会让上游报错，必须判为非法
	invalid := []string{
		"", "not-a-uuid-at-all", "019fc667facb7315a16e846d6a0923a4",
		"019fc667-facb-7315-a16e-846d6a0923a", "019fc667-facb-7315-a16e-846d6a0923a4x",
		"019fc667-facb-7315-a16e-846d6a0923gz", // 非十六进制
	}
	for _, v := range invalid {
		if IsUUID(v) {
			t.Errorf("应判为非 UUID（上游会拒绝）: %q", v)
		}
	}
}

// TestCredAlwaysHasDeviceMid 加载凭证时**总是**补上 deviceMid。
//
// 留空会让额度查询恒失败（3001），而那个失败看起来像"这个账号没有额度"。
// 故解析阶段就补齐，而不是留给调用方记得传。
func TestCredAlwaysHasDeviceMid(t *testing.T) {
	dir := t.TempDir()
	p := dir + "/zcode-x.json"
	if err := writeTestFile(p, `{"uid":"zcode-x","provider":"zai","credential":"k.s"}`); err != nil {
		t.Fatal(err)
	}
	c, err := LoadFile(p)
	if err != nil {
		t.Fatal(err)
	}
	if !IsUUID(c.DeviceMid) {
		t.Errorf("加载后应自动补上合法 deviceMid，实际 %q —— 留空会让额度查询恒回 3001", c.DeviceMid)
	}
}

// TestCredKeepsExistingDeviceMid 已有的 deviceMid 必须保留（不能每次重新生成）。
//
// 稳定性很重要：同一个账号应始终表现为同一台设备，
// 否则服务端会看到"一个账号被大量不同设备查询"。
func TestCredKeepsExistingDeviceMid(t *testing.T) {
	dir := t.TempDir()
	p := dir + "/zcode-y.json"
	mid := "019fc667-facb-7315-a16e-846d6a0923a4"
	body := `{"uid":"zcode-y","provider":"zai","credential":"k.s","device_mid":"` + mid + `"}`
	if err := writeTestFile(p, body); err != nil {
		t.Fatal(err)
	}
	c, err := LoadFile(p)
	if err != nil {
		t.Fatal(err)
	}
	if c.DeviceMid != mid {
		t.Errorf("应保留已有的 deviceMid %q，实际 %q", mid, c.DeviceMid)
	}

	// 写回后仍在（否则重启一次就换设备指纹）
	if err := c.SaveAtomic(); err != nil {
		t.Fatal(err)
	}
	again, err := LoadFile(p)
	if err != nil {
		t.Fatal(err)
	}
	if again.DeviceMid != mid {
		t.Errorf("写回后 deviceMid 应保持不变，实际 %q", again.DeviceMid)
	}
}
