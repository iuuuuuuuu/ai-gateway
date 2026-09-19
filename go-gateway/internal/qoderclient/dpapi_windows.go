//go:build windows

package qoderclient

// Windows 侧调用 DPAPI（`CryptUnprotectData`）解开 Electron 保护的 AES 密钥。
//
// ## 为什么必须走系统 API
//
// DPAPI 的密钥由 Windows 用**当前用户**的凭据派生（存在用户配置目录里），
// 无法在用户态复现 —— 唯一可行且正确的做法就是调它。
//
// ⚠ 一个易踩的坑（我踩过）：**必须传 `CRYPTPROTECT_UI_FORBIDDEN`（0x1）**。
// 不传时若密钥需要用户交互，调用会**弹窗并阻塞** —— 在无界面场景（网关
// 子进程）下就是永久挂起。传了之后失败会立刻返回错误码。

import (
	"errors"
	"fmt"
	"unsafe"

	"golang.org/x/sys/windows"
)

// cryptProtectUIForbidden 禁止 DPAPI 弹 UI（见上面的说明）。
const cryptProtectUIForbidden = 0x1

type dataBlob struct {
	cbData uint32
	pbData *byte
}

var (
	crypt32              = windows.NewLazySystemDLL("crypt32.dll")
	kernel32             = windows.NewLazySystemDLL("kernel32.dll")
	procCryptUnprotect   = crypt32.NewProc("CryptUnprotectData")
	procLocalFree        = kernel32.NewProc("LocalFree")
)

// dpapiUnprotect 用当前 Windows 用户解开 DPAPI blob。
func dpapiUnprotect(data []byte) ([]byte, error) {
	if len(data) == 0 {
		return nil, errors.New("DPAPI 输入为空")
	}

	in := dataBlob{cbData: uint32(len(data)), pbData: &data[0]}
	var out dataBlob

	// CryptUnprotectData(pDataIn, ppszDataDescr, pOptionalEntropy,
	//                    pvReserved, pPromptStruct, dwFlags, pDataOut)
	ret, _, err := procCryptUnprotect.Call(
		uintptr(unsafe.Pointer(&in)),
		0, // ppszDataDescr：不要描述
		0, // pOptionalEntropy：Electron 不用额外熵
		0, // pvReserved
		0, // pPromptStruct
		uintptr(cryptProtectUIForbidden),
		uintptr(unsafe.Pointer(&out)),
	)
	if ret == 0 {
		return nil, fmt.Errorf("CryptUnprotectData 失败: %w（该数据可能不属于当前 Windows 用户）", err)
	}
	defer procLocalFree.Call(uintptr(unsafe.Pointer(out.pbData)))

	if out.cbData == 0 || out.pbData == nil {
		return nil, errors.New("DPAPI 返回了空数据")
	}
	// 拷出来（LocalFree 之后这块内存就失效了）
	res := make([]byte, out.cbData)
	copy(res, unsafe.Slice(out.pbData, out.cbData))
	return res, nil
}
