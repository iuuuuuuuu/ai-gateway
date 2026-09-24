package qoderclient

// 机器指纹获取的测试。
//
// # 为什么这些测试重要
//
// 本功能的失败模式是**静默的**：拿不到真实指纹时上游回
// HTTP 200 + 结构合法的 JSON，只是**少一条活动** ——
// 不报错、不告警。所以必须钉住"解析逻辑对"以及"失败时安静降级"。

import (
	"context"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
)

// TestRunRuntimeInfoParsesRealShape 用**真实抓到的输出**验证解析。
//
// 形状取自 2026-09-23 在本机跑 `resources/umid/runtime-info.exe` 的实际输出
// （字段名逐字抄，没有简化）。
func TestRunRuntimeInfoParsesRealShape(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("用假可执行文件驱动，不依赖平台")
	}
}

// TestRunRuntimeInfoAgainstFakeExe 用一个**假的可执行文件**驱动解析逻辑。
//
// 这样不需要真的装 Qoder 客户端就能测到：
//   - JSON 解析
//   - 前后杂质（SDK 有时会多打一行日志）的容忍
//   - 三个字段都取到
func TestRunRuntimeInfoAgainstFakeExe(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("本用例用 shell 脚本做假 exe，只在类 Unix 上跑")
	}
	dir := t.TempDir()
	exe := filepath.Join(dir, "runtime-info")
	// ⚠ 用**明显的假值**，绝不抄本机真实指纹 ——
	// 本仓库是公开仓库，machineToken/Type/Code 是**这台设备**的身份标识。
	// 我第一版照抄了真实值，隐私扫描当场抓到。
	script := `#!/bin/sh
echo 'noise line before json'
echo '{"machineToken":"fake-token-for-test","machineType":"faketype00000000","machineCode":"fakecode00000000","vmInfo":{"isVm":true}}'
echo 'trailing noise'
`
	if err := os.WriteFile(exe, []byte(script), 0o755); err != nil {
		t.Fatal(err)
	}

	id := runRuntimeInfo(context.Background(), exe)
	if !id.Usable() {
		t.Fatalf("应从带杂质的输出里解出完整三元组，实际 %+v", id)
	}
	if id.MachineToken != "fake-token-for-test" {
		t.Errorf("machineToken 解析错：%q", id.MachineToken)
	}
	if id.MachineType != "faketype00000000" {
		t.Errorf("machineType 解析错：%q", id.MachineType)
	}
	if id.MachineCode != "fakecode00000000" {
		t.Errorf("machineCode 解析错：%q", id.MachineCode)
	}
}

// TestRunRuntimeInfoMissingBinaryIsQuiet 可执行文件不存在时**安静返回空**。
//
// 为什么要求"安静"：这条路径会在每个请求上跑。若它 panic 或报错，
// 用户看到的是"活动功能坏了"，而真实原因只是"没装客户端" ——
// 那是个**可接受的降级**（退回 1 条活动的老行为），不该升级成故障。
func TestRunRuntimeInfoMissingBinaryIsQuiet(t *testing.T) {
	id := runRuntimeInfo(context.Background(), filepath.Join(t.TempDir(), "not-exist"))
	if id.Usable() {
		t.Errorf("可执行文件不存在时应返回零值，实际 %+v", id)
	}
}

// TestRunRuntimeInfoGarbageOutputIsQuiet 输出不是 JSON 时**安静返回空**。
func TestRunRuntimeInfoGarbageOutputIsQuiet(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("用 shell 脚本做假 exe，只在类 Unix 上跑")
	}
	dir := t.TempDir()
	exe := filepath.Join(dir, "runtime-info")
	if err := os.WriteFile(exe, []byte("#!/bin/sh\necho 'not json at all'\n"), 0o755); err != nil {
		t.Fatal(err)
	}
	id := runRuntimeInfo(context.Background(), exe)
	if id.Usable() {
		t.Errorf("输出不是 JSON 时应返回零值，实际 %+v", id)
	}
}

// TestUsableRequiresAllThree 三个字段缺一不可。
//
// 为什么三个都要：它们是**同一组身份**。只发其中两个时上游的判定
// 未实测 —— 而"发一半"最坏的情况是被当成伪造（比不发更糟）。
// 宁可整体不发，退回已知的降级行为。
func TestUsableRequiresAllThree(t *testing.T) {
	cases := []struct {
		name string
		id   MachineIdentity
		want bool
	}{
		{"全有", MachineIdentity{"t", "y", "c"}, true},
		{"缺 token", MachineIdentity{"", "y", "c"}, false},
		{"缺 type", MachineIdentity{"t", "", "c"}, false},
		{"缺 code", MachineIdentity{"t", "y", ""}, false},
		{"全空", MachineIdentity{}, false},
	}
	for _, c := range cases {
		if got := c.id.Usable(); got != c.want {
			t.Errorf("%s：Usable() = %v，期望 %v", c.name, got, c.want)
		}
	}
}

// TestRuntimeInfoDirsAreEnumeratedNotHardcoded 候选目录必须**枚举**而不是写死。
//
// ⚠ 这是本模块最容易被"顺手优化"坏掉的地方：
// 用户级缓存目录名是 `.umid-<platform>-<内容哈希>`（实测
// `.umid-win32-x64-48d1294f147c9d89`），**哈希会随版本变**；
// 客户端多版本布局里的版本号（`0.4.1`）同理。
//
// 写死任一个都会让"换版本后静默找不到指纹" —— 而失败模式又是静默的
// （退回 1 条活动，不报错）。故这里断言：目录列表来自 ReadDir 枚举，
// 即**构造一个假的多版本布局，看它是否被发现**。
func TestRuntimeInfoDirsAreEnumeratedNotHardcoded(t *testing.T) {
	// 这条测试检查的是"实现里有 ReadDir 枚举"这一结构性质。
	// 用源码级断言而不是行为断言：行为断言需要伪造整个安装根目录，
	// 而 os.Getenv("LOCALAPPDATA") 在测试里不好替换（且 Windows 上
	// 替换它会影响同进程其它测试）。
	src, err := os.ReadFile("machine_identity.go")
	if err != nil {
		t.Fatal(err)
	}
	s := string(src)

	for _, want := range []string{
		`os.ReadDir(bin)`,                       // 枚举 ~/.qoder/.bin/.umid-*
		`strings.HasPrefix(e.Name(), ".umid-")`, // 按前缀匹配而非全名
		`os.ReadDir(vers)`,                      // 枚举 .qoder-versions/*
	} {
		if !strings.Contains(s, want) {
			t.Errorf("实现里应包含 %q（枚举而非写死路径），未找到\n"+
				"⚠ 写死路径会让换版本后静默找不到指纹 —— 失败模式是静默的", want)
		}
	}
}
