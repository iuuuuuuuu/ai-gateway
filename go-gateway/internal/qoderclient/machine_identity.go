package qoderclient

// 机器指纹（machineToken / machineType / machineCode）的获取。
//
// # 为什么需要这个文件（2026-09-23 实测定位）
//
// 所有者报：「国内的有这个领取任务，国际版怎么只剩下一个活动了，这是bug吧?」
//
// 根因**不在**活动接口本身，而在机器指纹：
//
//	openapi.qoder.sh + Bearer + Cosy-ClientType              → 1 条活动
//	+ 伪造的 Cosy-MachineToken/Cosy-MachineType              → 1 条活动
//	+ **真实的** Cosy-MachineToken/Cosy-MachineType          → 2 条活动（含「每天领 100 Credits」）
//
// ⚠ 上游对残缺请求回的是 **HTTP 200 + 结构合法的 JSON**，只是少一条活动 ——
// 不报错、不告警。这就是它藏了这么久、并让排查两次误判方向的原因。
//
// # 真实值从哪来：客户端自带的 runtime-info.exe
//
// 我们此前在 `cred.go` 的 `EnsureFingerprint` 里**本地编造**
// （`hexShort(32)` + 固定 `"5"`）—— 国服 host 不校验所以一直没暴露，
// 而**国际版 host 严格校验** (token, machineToken, machineType) 三元组。
//
// 真实值**不在客户端磁盘上**（`auth.v1.dat` 只有 token/refreshToken；
// `auth.machine-id` 只是个 UUID；sqlite / leveldb / 日志里都没有），
// 而是由客户端**自带的阿里云风控 SDK** 在运行时产出：
//
//	resources/umid/runtime-info.exe  →  stdout 一行 JSON:
//	{"machineToken":"P1gAAbR…","machineType":"300a54ab…",
//	 "machineCode":"b3c4e04f…","vmInfo":{…}}
//
// 客户端自己的做法（`app.asar` 里的 `AXe`）就是 spawn 它、从 stdout 读 JSON：
//
//	const t = e.platform === "darwin" || e.platform === "win32"
//	const A = spawn(e.executablePath, t ? [env, "--account-stdin"] : [env],
//	                { stdio: [t ? "pipe" : "ignore", "pipe", "pipe"] })
//
// ⚠ 实测 **不带 `--account-stdin` 也能拿到同样的三个值**
//（带它只多一个 `accountOutcome` 字段，那是账号维度的风控结论，我们不用）。
// 故这里用最简单的调用形式，少一个可能随版本变化的参数。
//
// # 设计取舍
//
//   - **只读、不落盘**：每次需要时现场跑一次（进程启动约几十毫秒）。
//     缓存进凭证文件是**错的** —— 这些值属于「这台机器」而不是「这个账号」，
//     写进 per-account 凭证会让换机器后带着旧机器的指纹，反而更可疑。
//   - **失败不致命**：拿不到时返回空，调用方**不发**那几个头
//     （退回"降级但可用"的老行为），而不是让整个功能报错。
//   - **不伪造**：这是本文件存在的全部意义。宁可少一个活动，
//     也不要拿假指纹去请求 —— 假值对严格校验的 host 毫无帮助，
//     而且把"设备指纹"这个概念污染了。

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"time"
)

// MachineIdentity 风控 SDK 产出的机器指纹三元组。
type MachineIdentity struct {
	MachineToken string `json:"machineToken"`
	MachineType  string `json:"machineType"`
	MachineCode  string `json:"machineCode"`
}

// Usable 报告三个值是否齐全。任一为空都不该拿去请求。
func (m MachineIdentity) Usable() bool {
	return m.MachineToken != "" && m.MachineType != "" && m.MachineCode != ""
}

// runtimeInfoTimeout 单次取指纹的超时。
//
// 实测本机约 30~80ms。给 5 秒是**宽裕**值：SDK 在某些虚拟化环境里
// 会做更多探测（实测 vmInfo 报 isVm=true，那种机器更慢）。
// 超时后放弃（返回空），不阻塞调用方 —— 领取活动不值得为指纹卡住。
const runtimeInfoTimeout = 5 * time.Second

// runtimeInfoNames 该可执行文件的候选名（按平台）。
//
// Windows 实测是 `runtime-info.exe`；其它平台的文件名未实测，
// 故一并列出常见形态，不存在就跳过（不猜、不报错）。
func runtimeInfoNames() []string {
	switch runtime.GOOS {
	case "windows":
		return []string{"runtime-info.exe", "runtime-info"}
	default:
		return []string{"runtime-info", "runtime-info.exe"}
	}
}

// runtimeInfoDirs 候选目录（**通用，不写死本机路径**）。
//
// 覆盖三种实测/可能的布局：
//
//	① 随客户端安装：`<install>/resources/umid/`         ← 实测本机
//	② 多版本布局：  `<install>/.qoder-versions/<v>/resources/umid/`
//	③ 用户级缓存：  `~/.qoder/.bin/.umid-<platform>-<hash>/`  ← 实测本机也有
//
// ⚠ 一律**枚举**而不是拼死一个路径：客户端的版本号与哈希后缀会变
// （实测 `.umid-win32-x64-48d1294f147c9d89` 里那段哈希是内容哈希）。
func runtimeInfoDirs() []string {
	var dirs []string

	// ③ 用户级缓存：枚举 ~/.qoder/.bin/ 下所有 .umid-* 目录。
	if home, err := os.UserHomeDir(); err == nil {
		bin := filepath.Join(home, ".qoder", ".bin")
		if entries, err := os.ReadDir(bin); err == nil {
			for _, e := range entries {
				if e.IsDir() && strings.HasPrefix(e.Name(), ".umid-") {
					dirs = append(dirs, filepath.Join(bin, e.Name()))
				}
			}
		}
	}

	// ①② 随客户端安装：枚举常见安装根，再看 resources/umid 与
	//     .qoder-versions/*/resources/umid。
	for _, root := range installRoots() {
		dirs = append(dirs, filepath.Join(root, "resources", "umid"))
		vers := filepath.Join(root, ".qoder-versions")
		if entries, err := os.ReadDir(vers); err == nil {
			for _, e := range entries {
				if e.IsDir() {
					dirs = append(dirs, filepath.Join(vers, e.Name(), "resources", "umid"))
				}
			}
		}
	}
	return dirs
}

// installRoots 客户端可能的安装根目录。
//
// 与 `appDirs` 的取舍相同：**列多个而不是只认一个** ——
// 只认 CN 会在国际版机器上静默找不到，反之亦然。
func installRoots() []string {
	var roots []string
	if la := os.Getenv("LOCALAPPDATA"); la != "" {
		for _, n := range []string{"Qoder", "Qoder CN", "QoderCN", "Programs\\Qoder", "Programs\\Qoder CN"} {
			roots = append(roots, filepath.Join(la, n))
			roots = append(roots, filepath.Join(la, "Programs", n))
		}
	}
	// macOS / Linux 的常见位置（未实测，保守列出；不存在就跳过）。
	roots = append(roots, "/Applications/Qoder.app/Contents/Resources")
	if home, err := os.UserHomeDir(); err == nil {
		roots = append(roots, filepath.Join(home, ".qoder", "app"))
	}
	return roots
}

// FindRuntimeInfo 找出可用的 `runtime-info` 可执行文件。
func FindRuntimeInfo() (string, bool) {
	for _, dir := range runtimeInfoDirs() {
		for _, name := range runtimeInfoNames() {
			p := filepath.Join(dir, name)
			if fileExists(p) {
				return p, true
			}
		}
	}
	return "", false
}

// cachedIdentity 进程级缓存。
//
// 为什么可以缓存：这些值描述的是**这台机器**，在一次进程生命周期内不会变，
// 而它们要被用在**每个请求**上 —— 不缓存就要每个请求 spawn 一个进程。
//
// ⚠ 与"不落盘"不矛盾：落盘会跨机器/跨重装残留，进程内缓存不会。
var (
	identityOnce   sync.Once
	identityCached MachineIdentity
)

// MachineIdentityOf 取本机机器指纹（带进程级缓存）。
//
// 失败时返回零值 —— 调用方据 `Usable()` 决定是否发那几个头。
// **不返回 error**：调用方（活动查询/领取）没有任何有意义的补救动作，
// 而把 error 一路传上去只会让"拿不到指纹"看起来像"功能坏了"。
func MachineIdentityOf(ctx context.Context) MachineIdentity {
	identityOnce.Do(func() {
		identityCached = readMachineIdentity(ctx)
	})
	return identityCached
}

// readMachineIdentity 真正去跑 runtime-info 并解析输出。
func readMachineIdentity(ctx context.Context) MachineIdentity {
	exe, ok := FindRuntimeInfo()
	if !ok {
		return MachineIdentity{}
	}
	return runRuntimeInfo(ctx, exe)
}

// runRuntimeInfo 跑一次可执行文件并解析 stdout 的 JSON。
//
// 抽出来是为了让测试能对着一个假的可执行文件断言解析逻辑
// （不需要真的装 Qoder 客户端）。
func runRuntimeInfo(ctx context.Context, exe string) MachineIdentity {
	ctx, cancel := context.WithTimeout(ctx, runtimeInfoTimeout)
	defer cancel()

	var stdout, stderr bytes.Buffer
	cmd := exec.CommandContext(ctx, exe)
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr
	// 不继承环境变量的**全部**内容 —— SDK 只依赖系统信息，
	// 而我们不该把网关进程的凭据类变量传给它。
	cmd.Env = minimalEnv()

	if err := cmd.Run(); err != nil {
		// 包含 ctx 超时。两者都只是"拿不到"，不区分原因 ——
		// 调用方无从补救，区分只会增加分支。
		return MachineIdentity{}
	}

	// ⚠ 输出可能带前后空白/多行，取**第一个 `{` 开始的完整 JSON**。
	// 直接 json.Unmarshal 整个 stdout 在 SDK 多打一行日志时会失败。
	raw := stdout.Bytes()
	start := bytes.IndexByte(raw, '{')
	if start < 0 {
		return MachineIdentity{}
	}
	// 从最后一个 `}` 往回，容忍尾随换行/日志。
	end := bytes.LastIndexByte(raw, '}')
	if end <= start {
		return MachineIdentity{}
	}

	var id MachineIdentity
	if err := json.Unmarshal(raw[start:end+1], &id); err != nil {
		return MachineIdentity{}
	}
	return id
}

// minimalEnv 传给 SDK 的最小环境变量集合。
//
// Windows 上必须保留 SystemRoot（缺了 DLL 加载会失败），
// 其余按需保留 PATH 之类。**刻意不透传**整个 os.Environ()：
// 网关进程里可能有 API key 等敏感变量，没理由交给一个第三方 SDK。
func minimalEnv() []string {
	keep := []string{"SystemRoot", "windir", "TEMP", "TMP", "PATH", "HOME", "USERPROFILE"}
	out := make([]string, 0, len(keep))
	for _, k := range keep {
		if v := os.Getenv(k); v != "" {
			out = append(out, k+"="+v)
		}
	}
	return out
}

// ErrNoRuntimeInfo 找不到风控 SDK 可执行文件。
//
// 仅供**测试与诊断**使用：生产路径不返回它（见 MachineIdentityOf 的注释）。
var ErrNoRuntimeInfo = errors.New("找不到 runtime-info 可执行文件（客户端可能未安装或布局变了）")
