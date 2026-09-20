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
// **LLM 请求路径不发** X-Device-Mid：参考实现注释说 3.12.3 起该路径不再发它
//（`X-Device-Mid is NEVER sent`）。多带一个真实客户端不发的头反而会成为**区分特征**。
//
// ## ⚠ 但**控制面**（额度/账单）**必须发** —— 我最初漏读了这半句
//
// 参考实现原话是「only control-plane requests send it」，我当时只看到
// LLM 那半句，于是额度查询也漏发了它，结果是：
//
//	GET /api/v1/zcode-plan/billing/balance   →  400 {"code":3001,"msg":"parameter error"}
//
// 我把那个 3001 误判成"额度需要 JWT、只导入凭证的账号查不到"，
// 甚至在界面上做了「额度未知」这个状态。**那个结论是错的。**
//
// 实测（uitest/probe-zcode-quota-auth2.cjs）：
//
//	不带头            → 400 code=3001
//	UUID 格式的头     → **200 code=0，拿到完整额度**
//	非 UUID 字符串     → 429
//	空串              → 400 code=3001
//
// 且**任意 UUID 都行**（随机生成的也通过）—— 不需要是注册过的设备。
// 故这里是"格式校验"而非"设备身份校验"。
//
// 结论：控制面请求必须带一个 UUID 形态的 X-Device-Mid。见 ControlPlaneHeaders。

import (
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"os"
	"runtime"
	"strings"
)

// NewDeviceMid 生成一个 UUID v4 形态的 deviceMid。
//
// ## 为什么随机就够
//
// 实测（uitest/probe-zcode-quota-auth.cjs）：上游**只校验格式**，
// 随机生成的 UUID 与客户端真实读到的一样能通过（都回 200）。
// 故不需要假装是某个已注册设备 —— 那反而更可疑。
//
// ## 为什么要是 v4 形态（第 13 位 = 4）
//
// 上游会对"看着不像 UUID"的值直接 429/3001。规范的 v4 形态最安全。
func NewDeviceMid() string {
	var b [16]byte
	if _, err := rand.Read(b[:]); err != nil {
		// 随机源不可用时回退一个固定值 —— 额度查询失败好过整个功能不可用
		return "00000000-0000-4000-8000-000000000000"
	}
	// 版本位（第 7 字节高 4 位 = 4）与变体位（第 9 字节高 2 位 = 10）
	b[6] = (b[6] & 0x0f) | 0x40
	b[8] = (b[8] & 0x3f) | 0x80
	s := hex.EncodeToString(b[:])
	return s[0:8] + "-" + s[8:12] + "-" + s[12:16] + "-" + s[16:20] + "-" + s[20:32]
}

// IsUUID 报告 s 是否是 UUID 形态（控制面请求的硬要求）。
//
// 宽松判据（只认形状，不校验版本位）：上游就是这么判的 ——
// 实测非 UUID 字符串会被 429，而任意 UUID（含非 v4）都通过。
func IsUUID(s string) bool {
	s = strings.TrimSpace(s)
	if len(s) != 36 {
		return false
	}
	for i, c := range s {
		switch i {
		case 8, 13, 18, 23:
			if c != '-' {
				return false
			}
		default:
			if !((c >= '0' && c <= '9') || (c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F')) {
				return false
			}
		}
	}
	return true
}

// DefaultAppVersion 默认上报的客户端版本。
//
// # 版本号必须跟着官方客户端走（2026-09-20 更新）
//
// 参考实现（TriDefender/zcode-api）在最新提交 `8dca421 "Improved compliance"`
// 把默认值从 `3.12.3` 升到 **`3.14.0`**，并新增了 3.14.0 对应的 host builder。
// 作者原话（译）：*"每次客户端发版都要跟上，否则 User-Agent 与
// X-ZCode-App-Version 会变成可区分的特征。"*
//
// 即：**报一个过旧的版本本身就是指纹**。我们此前停在 3.12.3，
// 而官方客户端已经是 3.14.0 —— 那会让我们的请求与真机不一致。
//
// ⚠ 上游升级后这个值还会过时。参考实现的做法是让它可配置
//（`identity.appVersion`），本包同样保留 Identity.AppVersion 字段可覆盖。
// 排查"为什么被风控"时，**先核对这个版本号是否还是最新的**。
const DefaultAppVersion = "3.14.0"

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

	// AccountID 当前请求所属的**账号**标识。
	//
	// # 为什么身份里需要它
	//
	// `x-session-id` 与 `x-zcode-trace-id` 必须是**会话级稳定**的
	//（抓包实测：官方相隔 58 分钟的两次请求，这两个值完全相同），
	// 而网关无状态、没有真实会话概念 —— 故用账号标识派生稳定 id。
	//
	// 见 TraceHeaders 的长注释（含抓包对照表与"为什么这很可能是 3012 真因"）。
	AccountID string

	// SessionID 调用方已知的真实会话 id（可选，优先于 AccountID）。
	//
	// 若是 UUID 形态就**直接用作** `x-session-id`（最贴近官方行为）；
	// 否则作为派生的种子。
	SessionID string
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

// TraceHeaders 返回**追踪头**（对话通道用）。
//
// # ⚠⚠ 2026-09-20 重大更正：这些 id 的**生命周期**此前搞错了
//
// 抓包对比官方客户端**相隔 58 分钟**的两次成功请求：
//
//	头                 10:01:26              10:59:12              行为
//	─────────────────────────────────────────────────────────────────────
//	x-query-id        01a0bc8b-dd41-…       01a0bcc0-dd41-…       每条消息**不同**
//	x-request-id      04f0cee5-ab55-…       f816eb32-8fa5-…       每请求**不同**
//	x-session-id      8fc6b5b0-fb13-…       8fc6b5b0-fb13-…       **完全相同**
//	x-zcode-trace-id  65638a21-a6ea-…       65638a21-a6ea-…       **完全相同**
//
// 而旧实现**四个全都每请求重新随机生成**（见 git 历史）。
//
// 为什么这很可能是 3012 的真因：风控看的是**行为模式**。
// 一个"每发一条消息就换一次 trace-id / session-id"的客户端，
// 在服务端看来与脚本无异 —— 真实客户端的会话标识在整个会话期内是**稳定的**。
//
// 故现在按生命周期分三类：
//
//	每请求新   x-request-id        （官方每请求都换）
//	每条消息新 x-query-id          （官方每条消息都换）
//	每会话稳定 x-session-id        （官方会话期内不变）
//	每会话稳定 x-zcode-trace-id    （官方会话期内不变）
//
// ## "每会话"的粒度
//
// 我们没有真实会话概念（网关是无状态的），故粒度取"**每账号稳定**"：
//   · `sessionID` 非空时用它（调用方若能给出真实会话 id 更好）
//   · 否则用**账号标识**派生（同一账号恒定 → 上游看到的是一个稳定会话）
//
// 这比"每请求随机"接近官方行为得多。真正的会话级复用需要网关层
// 透传客户端的 session id，那是更大的改动（见 TraceHeaders 的用法）。
//
// ⚠ 每次调用仍要**重新生成** x-request-id / x-query-id（它们本来就该变）。
func (i Identity) TraceHeaders() map[string]string {
	// 会话级稳定值：优先用调用方给的会话 id，否则按账号派生。
	//
	// ⚠ 没有账号标识时**退回随机**（而不是退回固定串）：固定串会让
	// 所有账号共用一个 session-id，那比随机更糟（上游会看到"一个会话
	// 在给所有账号发请求"）。
	sessionID := strings.TrimSpace(i.SessionID)
	if sessionID == "" {
		sessionID = strings.TrimSpace(i.AccountID)
	}
	if sessionID == "" {
		// 无账号信息：退回每请求随机，至少不比原来差。
		return map[string]string{
			"x-request-id":         newTraceID(),
			"x-zcode-session-type": "main",
			"x-zcode-trace-id":     newTraceID(),
			"x-query-id":           newTraceID(),
			"x-session-id":         newTraceID(),
		}
	}
	// 用账号标识**派生**两个稳定 id（同一账号恒定，不同账号不同）。
	//
	// 为什么不直接用 AccountID 本身：官方这两个值是 **UUID 形态**
	//（上游会对"看着不像 UUID"的值回 429/3001，见 NewDeviceMid 的注释）。
	// 而账号标识可能是 `zcode-1b2941c020ef` 或长数字 —— 直接发会被拒。
	traceID := stableUUID("zcode-trace:" + sessionID)
	stableSession := stableUUID("zcode-session:" + sessionID)
	// 若调用方给的就是 UUID 形态的会话 id，直接用它作为 session-id
	//（更贴近官方：官方就是用会话 uuid）。
	if looksLikeUUID(sessionID) {
		stableSession = sessionID
	}
	return map[string]string{
		"x-request-id":         newTraceID(), // 每请求新（官方如此）
		"x-zcode-session-type": "main",
		"x-zcode-trace-id":     traceID,      // 每会话稳定
		"x-query-id":           newTraceID(), // 每条消息新（官方如此）
		"x-session-id":         stableSession, // 每会话稳定
	}
}

// looksLikeUUID 判断是否 UUID 形态（8-4-4-4-12 的十六进制）。
func looksLikeUUID(s string) bool {
	if len(s) != 36 {
		return false
	}
	for idx, r := range s {
		switch idx {
		case 8, 13, 18, 23:
			if r != '-' {
				return false
			}
		default:
			if !((r >= '0' && r <= '9') || (r >= 'a' && r <= 'f') || (r >= 'A' && r <= 'F')) {
				return false
			}
		}
	}
	return true
}

// stableUUID 从一个任意字符串**确定性地**派生一个 UUIDv4 形态的 id。
//
// 用途：需要"同一输入恒定、不同输入不同、且形态必须是 UUID"的标识。
// 用 SHA-256 截断 —— 确定性来自哈希，不需要额外状态（网关重启后依旧一样，
// 这点很重要：重启即换 session-id 会再次暴露"非真实会话"）。
func stableUUID(seed string) string {
	sum := sha256.Sum256([]byte(seed))
	b := sum[:16]
	// 设版本位（v4）与变体位 —— 与 NewDeviceMid 一致，否则形态不像 UUID
	b[6] = (b[6] & 0x0f) | 0x40
	b[8] = (b[8] & 0x3f) | 0x80
	return fmt.Sprintf("%x-%x-%x-%x-%x", b[0:4], b[4:6], b[6:8], b[8:10], b[10:16])
}

// newTraceID 生成一个 UUIDv4 形态的追踪 id。
//
// 复用 `NewDeviceMid`（它已经处理了 v4 的版本位与变体位）—— 追踪头与
// 设备标识对**形态**的要求相同：上游会对"看着不像 UUID"的值直接拒绝
//（参考实现用 `str(uuid.uuid4())`）。
//
// 单独包一层只是为了语义清晰：这里是"每次请求一个新 id"，
// 而 NewDeviceMid 的调用点语义是"一个稳定的设备标识"。
func newTraceID() string { return NewDeviceMid() }

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
		// ⚠ 必须带 runtime 后缀（官方实测值），见 userAgentRuntimeSuffix 的注释。
		h["User-Agent"] = "ZCode/" + ver + userAgentRuntimeSuffix
		h["X-ZCode-App-Version"] = ver
	} else {
		// 版本取不到时官方会回退成 "ZCode/unknown"（参考实现的 fio 行为）
		h["User-Agent"] = "ZCode/unknown" + userAgentRuntimeSuffix
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

// ControlPlaneHeaders 在 `Headers()` 之上补上控制面（额度/账单）**必需**的头。
//
// ## 唯一差异是 X-Device-Mid
//
// 上游对控制面要求一个 **UUID 形态**的 `X-Device-Mid`：
//
//	不发 / 空串 / 非 UUID   → 400 {"code":3001,"msg":"parameter error"}
//	任意 UUID              → 200（实测随机生成的也通过）
//
// 故这里不需要"注册设备" —— 生成一个稳定的 UUID 即可。
// **稳定**很重要（不是每次请求随机）：同一个账号的额度查询应呈现为同一台设备，
// 否则在服务端看来是"一个账号被大量不同设备查询"。
//
// deviceMid 为空时**不发**这个头（保持原行为），调用方据此可显式关闭它。
func (i Identity) ControlPlaneHeaders(deviceMid string) map[string]string {
	h := i.Headers()
	if v := strings.TrimSpace(deviceMid); v != "" {
		h["X-Device-Mid"] = v
	}
	return h
}

// ClaimHeaders 是 **claim（领取限量套餐）** 端点的请求头。
//
// # 为什么不能用 ControlPlaneHeaders（2026-09-20 按参考实现对齐）
//
// 参考实现（TriDefender/zcode-api `src/claim/client.ts`）的 claim 请求
// **只发 6~7 个头**，且与身份头 bundle 是两套：
//
//	Authorization: Bearer {jwt}
//	Content-Type: application/json
//	X-Aliyun-Captcha-Verify-Param: {verifyParam}
//	[X-Aliyun-Captcha-Verify-Region: {region}]
//	X-ZCode-App-Version: {appVersion}      ← 我们此前**缺这个**
//	X-Platform: {platform}                 ← 我们此前**缺这个**
//	[X-Device-Mid: {uuid}]                 ← 活动期硬要求
//
// 而我们此前给 claim 发的是 `ControlPlaneHeaders` **整套身份头**
//（User-Agent / X-Title / HTTP-Referer / X-Os-* / X-ZCode-Agent …）。
//
// # 为什么"多送头"是风险而不是保险
//
// 直觉上多发头像是"更完整"，但上游的风控判据是**与真机一致**：
// 官方客户端在这条路径上只发那几个头，我们多发一堆反而是**可区分的特征**。
// 参考实现特意记载过这个反复：早期版本发全套身份头
// （2026-08-28 活动期"经验上被接受"），3.12.3 起**改回逐字镜像客户端**。
//
// 故这里刻意**不**调用 `i.Headers()`，而是按参考实现逐字构造。
//
// ⚠ `X-Device-Mid` 仍然必需（活动期网关硬要求）——
// 参考实现 commit `6b7327e "修复自动claim"` 专门修的就是这个：
// 缺它必回 `biz 3001 parameter error`。我们此前已实现，此处保留。
func (i Identity) ClaimHeaders(deviceMid string) map[string]string {
	h := map[string]string{
		"X-ZCode-App-Version": i.AppVersion,
		"X-Platform":          i.PlatformArch(),
	}
	if v := strings.TrimSpace(deviceMid); v != "" {
		h["X-Device-Mid"] = v
	}
	return h
}

// PreviewHeaders 是 **preview（查询可领套餐）** 端点的请求头。
//
// # 比 ClaimHeaders 更简（参考实现实测）
//
// 参考实现的 preview 是**极简头**：有 JWT 时只发
// `Authorization`（+ 活动期的 `X-Device-Mid`），**连 Content-Type /
// Accept / 身份头都不发**；匿名时**一个头都不发**。
//
// ⚠ 那为什么不干脆复用 ClaimHeaders？
// 因为 preview 与 claim 在参考实现里就是**两个不同的头集**，
// 而风控按路径比对。既然已知真机行为，就照它来。
//
// 返回的头由调用方再补 `Authorization` —— 那取决于凭证是否存在，
// 而本函数不该依赖凭证（它只是身份信息的投影）。
func (i Identity) PreviewHeaders(deviceMid string) map[string]string {
	h := map[string]string{}
	if v := strings.TrimSpace(deviceMid); v != "" {
		// 活动期网关对 preview 也要求 UUID 形态的 X-Device-Mid
		//（与 claim 同一个实测结论：缺它 3001）。
		h["X-Device-Mid"] = v
	}
	return h
}
