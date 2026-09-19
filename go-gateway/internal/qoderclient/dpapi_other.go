//go:build !windows

package qoderclient

// 非 Windows 平台的占位实现。
//
// Qoder 客户端的加密是 **Windows 专属**的（Electron 在 Windows 上用
// DPAPI 保护密钥；macOS 用 Keychain，Linux 用 kwallet/gnome-libsecret）。
//
// 本仓库的宿主面向 Windows（见 AGENTS.md 的 Shell Policy），故这里
// 明确返回"不支持"而不是静默失败 —— 调用方能给用户准确的原因。

import (
	"errors"
	"runtime"
)

func dpapiUnprotect([]byte) ([]byte, error) {
	return nil, errors.New("读取 Qoder 客户端凭证目前只支持 Windows（当前平台：" + runtime.GOOS + "）")
}
