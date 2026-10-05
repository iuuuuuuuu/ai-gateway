package zcode

import (
	"io"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// captureStderr 把 os.Stderr 换成管道，跑 fn，返回它写进去的内容。
//
// 为什么要费这个事：`RunLoginCLI` 的帮助文案与 `default:` 分支的文案是
// **手工维护的字符串**，而 logincli.go 的文件注释里明确记着一次事故 ——
// 「帮助文案必须与**实际派发的子命令**一致（同 qoder 侧的教训）：旧文案
// 只列 start|poll|import，漏了 quota / models，排查时会误判「子命令没注册」」。
//
// 新增一个子命令却忘了改这两处文案，是**同一个 bug 的第三次**。这条测试
// 就是为了让它不可能再发生。
func captureStderr(t *testing.T, fn func()) string {
	t.Helper()

	old := os.Stderr
	r, w, err := os.Pipe()
	if err != nil {
		t.Fatalf("建管道失败: %v", err)
	}
	os.Stderr = w

	done := make(chan string, 1)
	go func() {
		b, _ := io.ReadAll(r)
		done <- string(b)
	}()

	fn()

	_ = w.Close()
	os.Stderr = old
	out := <-done
	_ = r.Close()
	return out
}

// TestHelpTextListsEveryDispatchedSubcommand 帮助文案里的子命令集合
// 与实际派发的**完全一致**。
//
// 这是 logincli.go 文件注释里记录的那次事故的回归测试：文案漏一个子命令，
// 用户会以为「子命令没注册」，排查方向直接跑偏。
func TestHelpTextListsEveryDispatchedSubcommand(t *testing.T) {
	// 实际派发的子命令（与 switch 的 case 一一对应）。
	// ⚠ 新增子命令时**必须**同步这里与 logincli.go 的两处文案。
	dispatched := []string{
		"start", "poll", "import", "quota", "models",
		"preview-plan", "claim-plan", "warm",
	}

	help := captureStderr(t, func() {
		if code := RunLoginCLI(nil, t.TempDir()); code != 2 {
			t.Errorf("空参数应返回 2，得到 %d", code)
		}
	})
	for _, sub := range dispatched {
		if !strings.Contains(help, sub) {
			t.Errorf("帮助文案里没有 %q：\n%s", sub, help)
		}
	}

	// default: 分支的文案同样要列全（它才是用户打错时看到的那句）。
	unknown := captureStderr(t, func() {
		if code := RunLoginCLI([]string{"no-such-subcommand"}, t.TempDir()); code != 2 {
			t.Errorf("未知子命令应返回 2，得到 %d", code)
		}
	})
	for _, sub := range dispatched {
		if !strings.Contains(unknown, sub) {
			t.Errorf("未知子命令的提示里没有 %q：\n%s", sub, unknown)
		}
	}
}

// TestWarmSubcommandRequiresUID warm 子命令缺 --uid 时**本地拦下**（退出码 2），
// 而不是发一个必然失败的请求上去。
//
// 与 claim-plan 缺 --plan-id 的处置同源：本地拦下能给出"你漏了参数"这句
// 人话，透传上去则会得到上游的 parameter error —— 那个文案完全看不出是
// 我们没传参数，排查方向会被引向"是不是缺 X-Device-Mid"。
func TestWarmSubcommandRequiresUID(t *testing.T) {
	code := RunLoginCLI([]string{"warm", "--auth-dir", t.TempDir()}, t.TempDir())
	if code != 2 {
		t.Errorf("缺 --uid 应返回 2，得到 %d", code)
	}
}

// TestWarmSubcommandFlagsParse warm 子命令的两个 flag 都能被解析
// （`--uid` / `--auth-dir`），不会因为 flag 名拼错而落到"未知 flag"分支。
//
// 这里**不**期望成功（临时目录里没有凭证 ⇒ loadCredByUID 必然失败），
// 只断言"flag 本身被接受"：flag.Parse 失败会返回 2，而凭证找不到返回 1。
func TestWarmSubcommandFlagsParse(t *testing.T) {
	code := RunLoginCLI([]string{"warm", "--uid", "zcode-does-not-exist", "--auth-dir", t.TempDir()}, t.TempDir())
	if code == 2 {
		t.Error("两个 flag 都应被 flag 包接受；返回 2 说明 flag 解析失败了（名字拼错？）")
	}
	if code != 1 {
		t.Errorf("凭证不存在应返回 1，得到 %d", code)
	}
}

// TestRunLoginCLIDoesNotTouchSharedCaptchaSolver 钉住一个**实测踩过的坑**：
// `RunLoginCLI` 不得改动包级共享的验证码求解器。
//
// # 为什么这条测试存在
//
// 曾经把「读配置 + SetDir/SetExternalSolver」写成一体式函数并在
// `RunLoginCLI` 开头**无条件**调用，实测证伪：
//
//	BEFORE: "求解器组件未安装（发行包应包含 assets/zcode-captcha）"
//	AFTER:  ""   DIR="D:\\APP\\AI Gateway\\_up_\\assets\\zcode-captcha"
//
// 本机 `~/.wb-switch/gateway/gateway_native_config.json` 真实存在，所以
// `loadCaptchaConfig` 的候选②会命中 —— 这跟传入的 authDir 无关。
//
// 后果是 `captcha_link_test.go` 明写的那个不变量被翻掉：
//
//	「本包测试从不给 `SharedCaptchaSolver()` 设 dir / 外部求解服务，
//	 故 `UnavailableReason()` 必非空 ⇒ `solveCaptcha` 确定性失败」
//
// 当时全包 `go test` 仍然绿，纯属 `captcha_link_test.go` 字典序在前、
// 先跑完的**偶然**（本文件有四处调 `RunLoginCLI`）。测试之间的顺序依赖
// 一旦形成，加个 `-run` 过滤或改个文件名就会莫名其妙地红。
//
// 故这里显式断言：跑一次 `RunLoginCLI` 前后，共享求解器的可用性**不变**。
func TestRunLoginCLIDoesNotTouchSharedCaptchaSolver(t *testing.T) {
	s := SharedCaptchaSolver()
	before := s.UnavailableReason()
	beforeDir := s.Dir()

	// 空参数走帮助分支（最早的一条早退路径，也是最容易被塞进"开头统一配置"的位置）。
	if code := RunLoginCLI(nil, t.TempDir()); code != 2 {
		t.Fatalf("空参数应返回 2，得到 %d", code)
	}
	// 再走一遍真实的 warm 派发：凭证加载失败会在 installCaptchaSolver 之前返回。
	if code := RunLoginCLI([]string{"warm", "--uid", "zcode-does-not-exist", "--auth-dir", t.TempDir()}, t.TempDir()); code != 1 {
		t.Fatalf("凭证不存在应返回 1，得到 %d", code)
	}

	if after := s.UnavailableReason(); after != before {
		t.Errorf("RunLoginCLI 改动了共享求解器的可用性：\n  before=%q\n  after =%q", before, after)
	}
	if after := s.Dir(); after != beforeDir {
		t.Errorf("RunLoginCLI 改动了共享求解器的 dir：\n  before=%q\n  after =%q", beforeDir, after)
	}
}

// TestLoadCaptchaConfigReadsPoolSection 钉住配置的**嵌套形状**。
//
// 四个 captcha 键在 `pool` 段**内**，不在顶层。这个坑很隐蔽：解错层级不会
// 报任何错，只会静默拿到空串 —— 表现与「配置文件不存在」**一模一样**，
// 于是现象是「求解器明明配了却总说没装」。而本函数只在候选①命中时返回，
// 所以用临时目录构造的配置能确定性地验证它。
//
// 布局必须与生产一致（候选① = `authDir` 上两级 + `/gateway/...`）：
//
//	<tmp>/gateway/gateway_native_config.json   ← 配置
//	<tmp>/zcode/auths                          ← authDir
func TestLoadCaptchaConfigReadsPoolSection(t *testing.T) {
	tmp := t.TempDir()
	if err := os.MkdirAll(filepath.Join(tmp, "gateway"), 0o755); err != nil {
		t.Fatal(err)
	}
	authDir := filepath.Join(tmp, "zcode", "auths")
	if err := os.MkdirAll(authDir, 0o755); err != nil {
		t.Fatal(err)
	}

	// 顶层也放一组同名键（值故意不同）：如果实现改成读顶层，这条会立刻红。
	cfgJSON := `{
	  "zcode_captcha_dir": "TOP-LEVEL-SHOULD-BE-IGNORED",
	  "pool": {
	    "zcode_captcha_dir": "/tmp/solver-dir",
	    "zcode_captcha_solver_url": "http://127.0.0.1:1/solve",
	    "zcode_captcha_solver_token": "tok-123",
	    "zcode_captcha_enabled": false
	  }
	}`
	if err := os.WriteFile(filepath.Join(tmp, "gateway", "gateway_native_config.json"),
		[]byte(cfgJSON), 0o644); err != nil {
		t.Fatal(err)
	}

	got := loadCaptchaConfig(authDir)
	if got.Dir != "/tmp/solver-dir" {
		t.Errorf("Dir 应从 pool 段解出 /tmp/solver-dir，得到 %q"+
			"（拿到顶层值说明解错了层级，会静默退化成\"求解器未安装\"）", got.Dir)
	}
	if got.URL != "http://127.0.0.1:1/solve" {
		t.Errorf("URL 应解出 http://127.0.0.1:1/solve，得到 %q", got.URL)
	}
	if got.Token != "tok-123" {
		t.Errorf("Token 应解出 tok-123，得到 %q", got.Token)
	}
}
