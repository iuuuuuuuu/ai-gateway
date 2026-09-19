package qoderclient

// 用**真实**客户端数据验证读取与解密。
//
// ## 为什么必须有这个测试
//
// 这条链路的每一环都容易"看起来对但实际不通"：
// DPAPI 的调用约定、Electron 的密钥前缀、AES-GCM 的分段顺序……
// 我在这条链上已经误判过一次（早期说"自定义加密、拿不到"）。
//
// 故用所有者**真实的**登录态验证一遍。找不到文件时跳过（CI/他人机器上
// 没装 Qoder 是正常的），但只要能找到就必须解得出来。

import (
	"os"
	"testing"
)

// TestRealClientAuthReadable 真实客户端凭证可读可解。
func TestRealClientAuthReadable(t *testing.T) {
	dir, ok := FindClientAppDir()
	if !ok {
		t.Skip("本机没有 Qoder 客户端登录数据（未安装或未登录）")
	}
	t.Logf("客户端目录: %s", dir)

	auth, err := ReadClientAuthFrom(dir)
	if err != nil {
		t.Fatalf("读取客户端凭证失败: %v", err)
	}

	if auth.SchemaVersion != 1 {
		t.Errorf("schemaVersion 应为 1，实际 %d", auth.SchemaVersion)
	}
	if auth.Token == "" {
		t.Error("token 为空")
	}
	if auth.RefreshToken == "" {
		t.Error("refreshToken 为空")
	}
	if auth.User.ID == "" {
		t.Error("user.id 为空 —— 那是账号的主键，缺了整个账号就没法用")
	}

	// 令牌形态：客户端的 DT/DRT 都是带前缀的（COSY 层要用）
	if len(auth.Token) < 10 {
		t.Errorf("token 太短（%d）：%q", len(auth.Token), auth.Token)
	}

	t.Logf("✓ 账号   : %s", auth.User.Name)
	t.Logf("✓ uid    : %s", auth.User.ID)
	t.Logf("✓ DT     : %s…（%d 字符）", mask(auth.Token), len(auth.Token))
	t.Logf("✓ DRT    : %s…（%d 字符）", mask(auth.RefreshToken), len(auth.RefreshToken))
	t.Logf("✓ 到期   : %s", auth.ExpiresAt)
	t.Logf("✓ region : %s", Region(dir))

	if mid := MachineID(dir); mid != "" {
		t.Logf("✓ 机器指纹: %s", mid)
	} else {
		t.Log("⚠ 没有 auth.machine-id（COSY 签名需要，调用方要自己造一个）")
	}
}

// TestMissingDirIsClearError 找不到目录时给**明确**的错误。
//
// 不能返回空结构：那会让调用方以为"读到了但账号是空的"，
// 在界面上显示成一个无名账号。
func TestMissingDirIsClearError(t *testing.T) {
	_, err := ReadClientAuthFrom(t.TempDir())
	if err == nil {
		t.Fatal("空目录应报错，而不是返回空结构")
	}
	t.Logf("空目录的错误信息: %v", err)
}

// TestRegionInference 由目录名推断区域。
//
// 猜错的后果是请求打到错误端点（必然失败），故这条要有测试。
func TestRegionInference(t *testing.T) {
	if r := Region(`C:\Users\x\AppData\Roaming\com.qodercn.app.stable`); r != "cn" {
		t.Errorf("含 cn 的目录应判为 cn，实际 %q", r)
	}
	if r := Region(`C:\Users\x\AppData\Roaming\com.qoder.app.stable`); r != "intl" {
		t.Errorf("不含 cn 的目录应判为 intl，实际 %q", r)
	}
}

// TestAppDirsNonEmpty 候选目录列表非空（且含实测的那个）。
func TestAppDirsNonEmpty(t *testing.T) {
	if os.Getenv("APPDATA") == "" {
		t.Skip("没有 APPDATA（非 Windows 或无环境变量）")
	}
	dirs := appDirs()
	if len(dirs) == 0 {
		t.Fatal("候选目录不该为空")
	}
	found := false
	for _, d := range dirs {
		if contains(d, "com.qodercn.app.stable") {
			found = true
		}
	}
	if !found {
		t.Error("候选里应含实测的 com.qodercn.app.stable")
	}
}

func mask(s string) string {
	if len(s) <= 8 {
		return "****"
	}
	return s[:6] + "…"
}

func contains(hay, needle string) bool {
	return len(hay) >= len(needle) && (func() bool {
		for i := 0; i+len(needle) <= len(hay); i++ {
			if hay[i:i+len(needle)] == needle {
				return true
			}
		}
		return false
	})()
}
