package zcode

// identity.go 身份头 —— 让网关在**指纹层**与官方 ZCode 客户端不可区分。
//
// ## 为什么照发这些头（既然实测证明它们不影响认证）
//
// 参考实现的注释写得很明确（`identity.ts` 模块头）：
//
//	so the proxy is indistinguishable from the official client at the
//	fingerprinting layer
//
// 也就是说：**认证**与**风控**是两件事。实测证明认证不看这些头
//（见 signing.go 的对照实验），但风控可能看 —— 而风控的代价
//（账号被标记、被限流）远高于发几个头的成本。
//
// ## 但头是**可选**的
//
// 本包**不因缺头而失败**。理由：
//   · 实测证明认证不需要它们；
//   · 在非 Windows 平台或环境探测失败时，硬凑一个假值
//     （如 platform="unknown"）反而**更容易被识别**（真实客户端不会发这个）；
//   · 参考实现自己也是"取不到就省略"（`...(v.n ? {...} : {})`）。
//
// ## 头的形状（照抄参考实现的 buildLlmIdentityHeaders）
//
// 顺序在 HTTP 里不重要（header 是无序的），但**清单**必须对：
//
//	HTTP-Referer          固定为 zcode 官网（官方客户端就这么发）
//	User-Agent            ZCode/{版本}
//	X-ZCode-App-Version   版本号
//	X-Title               "Z Code@{sourceTitle}"
//	X-Release-Channel     production / test
//	X-Client-Language     zh-CN（官方是 Intl locale）
//	X-Client-Timezone     Asia/Shanghai
//	X-ZCode-Agent         "glm"（固定）
//	X-Platform            windows-amd64 等
//	X-Os-Category         windows / macos / linux
//	X-Os-Version          系统版本
//
// **不发** X-Device-Mid：参考实现注释说 3.12.3 起 LLM 请求路径不再发它
//（`X-Device-Mid is NEVER sent`），只有控制面请求才发。
// 多带一个真实客户端不发的头反而会成为**区分特征**。

import (
	"os"
	"runtime"
	"strings"
)

// DefaultAppVersion 默认上报的客户端版本。
//
// 参考实现对着 ZCode 3.12.3 的桌面包逆向，故用同一个版本号 ——
// 报一个不存在或过旧的版本可能触发风控。
//
// ⚠ 上游升级后这个值会过时。参考实现的做法是让它可配置
//（`identity.appVersion`），本包同样保留 Identity.AppVersion 字段可覆盖。
const DefaultAppVersion = "3.12.3"

// Identity 身份头的取值来源。
type Identity struct {
	// AppVersion 上报的客户端版本。
	AppVersion string
	// SourceTitle X-Title 的后半段（官方是 "Z Code@{值}"）。
	SourceTitle string
	// RefererOrigin HTTP-Referer 的值。
	RefererOrigin string
	// ReleaseChannel production / test。
	ReleaseChannel string
	// ClientLanguage 如 zh-CN。
	ClientLanguage string
	// ClientTimezone 如 Asia/Shanghai。
	ClientTimezone string
	// Platform / Arch / OSVersion 系统信息。
	Platform  string
	Arch      string
	OSVersion string
}

// DefaultIdentity 按当前运行环境构造身份。
func DefaultIdentity() Identity {
	lang := os.Getenv("LANG")
	if lang == "" {
		lang = "zh-CN"
	}
	// 环境变量里的 locale 形如 "zh_CN.UTF-8"，归一成 "zh-CN"
	lang = strings.ReplaceAll(strings.SplitN(lang, ".", 2)[0], "_", "-")

	tz := os.Getenv("TZ")
	if tz == "" {
		tz = "Asia/Shanghai"
	}

	return Identity{
		AppVersion:     DefaultAppVersion,
		SourceTitle:    "zcode",
		RefererOrigin:  "https://zcode.z.ai",
		ReleaseChannel: "production",
		ClientLanguage: lang,
		ClientTimezone: tz,
		Platform:       runtime.GOOS,
		Arch:           runtime.GOARCH,
		OSVersion:      "", // 取不到就省略（不硬凑假值）
	}
}

// PlatformArch 返回上游要求的 "{platform}-{arch}" 形状。
//
// ## ⚠ 必须用 **Node.js 的命名**，不是 Go 的
//
// 实测（2026-09-19，uitest/diag-zcode-platform-value.cjs）：
// `client/configs` 端点**严格校验**这个值，取值错误返回
// `400 {"code":3001,"msg":"parameter error"}` —— 而这个错误信息
// **完全不提示是哪个参数错**，排查成本很高。
//
//	取值              结果
//	────────────────  ────────────────────────────────
//	win32-x64         ✓ HTTP 200（**正确**）
//	windows-amd64     ✗ 400（Go 的 runtime.GOOS/GOARCH 命名）
//	win32-amd64       ✗ 400
//	windows-x64       ✗ 400
//	win-x64           ✗ 400
//	win32 / windows   ✗ 400
//	（不发这个头）     ✓ HTTP 200
//
// 原因：上游是照 **Node.js 的 `process.platform` / `process.arch`** 校验的
//（官方客户端是 Electron 应用）。故必须做这层映射 —— 直接发 Go 的值会被拒。
func (i Identity) PlatformArch() string {
	return nodePlatform(i.Platform) + "-" + nodeArch(i.Arch)
}

// nodePlatform 把 GOOS 映射成 Node.js 的 process.platform 取值。
//
// 实测确认的对应关系（错误的取值会被上游 400 拒绝）：
//
//	goos      Node.js process.platform
//	────────  ────────────────────────
//	windows   "win32"     ← 注意不是 "windows"（这是最容易踩的坑）
//	darwin    "darwin"
//	linux     "linux"
func nodePlatform(goos string) string {
	switch strings.ToLower(strings.TrimSpace(goos)) {
	case "windows", "win32":
		return "win32"
	case "darwin", "macos":
		return "darwin"
	case "":
		// 空值回退到真实值（不能拼出 "-x64" 这种形状）
		return nodePlatform(runtime.GOOS)
	default:
		return "linux"
	}
}

// nodeArch 把 GOARCH 映射成 Node.js 的 process.arch 取值。
//
//	goarch    Node.js process.arch
//	────────  ─────────────────────
//	amd64     "x64"       ← 注意不是 "amd64"
//	arm64     "arm64"
//	386       "ia32"
func nodeArch(goarch string) string {
	switch strings.ToLower(strings.TrimSpace(goarch)) {
	case "amd64", "x64":
		return "x64"
	case "arm64", "aarch64":
		return "arm64"
	case "386", "ia32", "x86":
		return "ia32"
	case "":
		return nodeArch(runtime.GOARCH)
	default:
		return strings.ToLower(goarch)
	}
}

// osCategory 把 GOOS 归一成官方客户端的 X-Os-Category 取值。
//
// 官方取值（参考实现 identity.ts 的 normalizeOsCategory）：
//
//	darwin → "macos"
//	win32  → "windows"
//	其它   → "linux"
func osCategory(goos string) string {
	switch strings.ToLower(goos) {
	case "darwin", "macos":
		return "macos"
	case "windows", "win32":
		return "windows"
	default:
		return "linux"
	}
}

// Headers 返回要发的身份头。
//
// 取不到的值**直接省略**而不是填 "unknown" —— 理由见文件头注释
//（填假值比不发更容易被识别）。
func (i Identity) Headers() map[string]string {
	h := map[string]string{}

	// 这四个是固定的（官方客户端必发）
	h["HTTP-Referer"] = firstNonEmpty(i.RefererOrigin, "https://zcode.z.ai")
	h["X-ZCode-Agent"] = "glm"
	h["X-Client-Language"] = firstNonEmpty(i.ClientLanguage, "zh-CN")
	h["X-Client-Timezone"] = firstNonEmpty(i.ClientTimezone, "Asia/Shanghai")

	// User-Agent 与 App-Version 成对出现
	ver := strings.TrimSpace(i.AppVersion)
	if ver != "" {
		h["User-Agent"] = "ZCode/" + ver
		h["X-ZCode-App-Version"] = ver
	} else {
		// 版本取不到时官方会回退成 "ZCode/unknown"（参考实现的 fio 行为）
		h["User-Agent"] = "ZCode/unknown"
	}

	h["X-Title"] = "Z Code@" + firstNonEmpty(i.SourceTitle, "zcode")
	h["X-Release-Channel"] = firstNonEmpty(i.ReleaseChannel, "production")

	// 平台相关：platform 与 arch **必须都有**才发（官方是 `${platform}-${arch}`）
	//
	// ⚠ 取值必须用 Node.js 的命名（win32 / x64），不是 Go 的（windows / amd64）——
	// 实测取值错误会被上游 `400 {"code":3001}` 拒绝，见 PlatformArch 的注释。
	p, a := strings.TrimSpace(i.Platform), strings.TrimSpace(i.Arch)
	if p != "" && a != "" {
		h["X-Platform"] = nodePlatform(p) + "-" + nodeArch(a)
	}
	if p != "" {
		h["X-Os-Category"] = osCategory(p)
	}
	if v := strings.TrimSpace(i.OSVersion); v != "" {
		h["X-Os-Version"] = v
	}
	return h
}
