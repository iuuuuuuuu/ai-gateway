package zcode

// 阿里云无痕验证的**求解**（ZCode 对话通道的 3007「captcha verify failed」）。
//
// # 为什么需要它
//
// 实测（2026-09-19）：ZCode 对话端点对只带 JWT 的请求一律回
//
//	HTTP 400 {"code":3007,"msg":"captcha verify failed"}
//
// 且与**模型名大小写**、**请求头完整度**都无关（已用多组对照排除）。
// 服务端要求带 `X-Aliyun-Captcha-Verify-Param`。
//
// # 求解原理（以及它的性质）
//
// `assets/zcode-captcha/solver.js` 用 **happy-dom** 在 Node 里模拟一个
// 浏览器环境，在其中加载并运行**阿里云官方的验证 SDK**，走完它的正常
// 流程拿到 verifyParam。
//
// ⚠ 说清楚它是什么、不是什么：
//
//	是：在精简环境里正常执行官方 SDK
//	不是：逆向破解验证算法
//
// 但它在**没有真人操作**的情况下产出通过凭证，这一点必须让用户知道 ——
// 故本能力：默认关闭、界面上说明用途、**只在用户主动开启后**才参与请求。
//
// # 三条实测出来的硬约束（都踩过）
//
//  1. **verifyParam 是一次性的**。同一个值用第二次必然回 3007。
//     故**不能做预解池复用** —— 每个请求要现解（或用完即弃）。
//
//  2. **求解会被限流**。连续请求几次后上游回
//     `[pe-stall] pe.xxx.js x1` 且退出码 2；等约 90 秒恢复。
//     故必须有**冷却**，失败后一段时间内不再尝试。
//
//  3. **调试信息在 stderr、结果在 stdout**。
//     合并两者（`2>&1`）会被 `[pe-stall]` 抢先，看起来像失败 ——
//     我因此误判过一次，以为求解器不可用。
//
// # 它解决不了什么（同样重要）
//
// 实测：带上有效 verifyParam 后 **3007 消失**，但会变成
//
//	HTTP 405 {"code":3012,"msg":"request has been blocked due to unusual activity."}
//
// 3012 是**账号/风控层**，与 deviceMid 取值、param 复用都无关
//（三种 deviceMid 取值实测都是 3012）。故本模块**只解决验证码这一层**，
// 不承诺对话一定能通 —— 界面要如实说明。

import (
	"bufio"
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"time"
)

// CaptchaSolver 求解器（带限流冷却与 node 探测）。
type CaptchaSolver struct {
	mu sync.Mutex
	// dir 求解器所在目录（含 solver.js 与 node_modules）。
	dir string
	// nodePath 已解析到的 node 可执行文件；空 = 尚未探测。
	nodePath string
	// nodeErr 探测失败的原因（供界面如实展示）。
	nodeErr error
	// cooldownUntil 限流冷却截止；零值 = 不在冷却中。
	cooldownUntil time.Time
	// cooldownReason 进入冷却的原因（供诊断）。
	cooldownReason string
	// config 上游下发的 captcha 配置（scene/region/prefix）。
	scene  string
	region string
	prefix string
}

// DefaultCaptchaCooldown 被限流后的冷却时长。
//
// 实测：连续求解约 3-5 次后开始 `pe-stall`，等 90 秒恢复。
// 取 2 分钟留余量。
const DefaultCaptchaCooldown = 2 * time.Minute

// captchaEntryFile 求解器入口文件名。
//
// # 为什么优先用打包好的单文件
//
// 原始做法是把整个 `node_modules/` 塞进安装包 ——
// **11.36MB / 3353 个文件**。而 Tauri 的 NSIS 模板对每个资源文件生成
// 一条 `File /a "/oname=..."`，即**逐文件解压**；3353 个零散小文件的
// 写入远慢于单个大文件（尤其被杀软逐个扫描时）。
//
// 实测：装了这种包的机器上，安装过程慢到用户专门反馈
//（「安装的时候那个 node_modules 解压速度超级慢」）。
//
// 现在用 esbuild 打成**单个文件**（`solver.bundle.cjs`，928KB），
// 安装时的文件写入从 3353 次降到 1 次。
// 打包方法与坑见 `assets/zcode-captcha/README-打包说明.md`。
//
// ⚠ 保留 `solver.js` 作为**回退**：万一某台机器上 bundle 出问题
//（如 node 版本过旧不支持某个语法），源码目录还在就能原地诊断。
// 这也是"打包产物不可读"的补偿 —— 排查时有源码可看。
const (
	captchaEntryBundle = "solver.bundle.cjs"
	captchaEntrySource = "solver.js"
)

// captchaConfig 上游下发的 captcha 默认值（取自 client/configs 实测）。
const (
	defaultCaptchaScene  = "11xygtvd"
	defaultCaptchaRegion = "cn"
	defaultCaptchaPrefix = "no8xfe"
)

// NewCaptchaSolver 造一个求解器（带实测默认的运营参数）。
//
// ⚠ 千万不要直接 `&CaptchaSolver{}` —— 那样 scene/region/prefix 都是空串，
// 求解必然失败（上游要求这三个非空）。本测试
// `TestSetCaptchaConfigFromUpstream` 就是抓住这个才写的。
func NewCaptchaSolver() *CaptchaSolver {
	return &CaptchaSolver{
		scene:  defaultCaptchaScene,
		region: defaultCaptchaRegion,
		prefix: defaultCaptchaPrefix,
	}
}

var (
	captchaOnce   sync.Once
	captchaShared *CaptchaSolver
)

// SharedCaptchaSolver 进程级共享的求解器（含冷却状态）。
//
// 为什么共享：冷却状态必须是**进程级**的 —— 每个账号各造一个求解器的话，
// 限流会变成"每个账号各自撞墙"，而限流是**上游按来源**施加的。
func SharedCaptchaSolver() *CaptchaSolver {
	captchaOnce.Do(func() {
		captchaShared = NewCaptchaSolver()
	})
	return captchaShared
}

// SetCaptchaConfig 用上游配置覆盖默认值（scene/region/prefix 都是运营参数）。
func (s *CaptchaSolver) SetCaptchaConfig(scene, region, prefix string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if v := strings.TrimSpace(scene); v != "" {
		s.scene = v
	}
	if v := strings.TrimSpace(region); v != "" {
		s.region = v
	}
	if v := strings.TrimSpace(prefix); v != "" {
		s.prefix = v
	}
}

// SetDir 指定求解器目录（宿主把它作为资源释放到某处后告诉网关）。
func (s *CaptchaSolver) SetDir(dir string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.dir = strings.TrimSpace(dir)
	s.nodePath = "" // 目录变了要重新探测
	s.nodeErr = nil
}

// Dir 返回当前求解器目录。
func (s *CaptchaSolver) Dir() string {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.dir
}

// Region 返回验证码所属区域（`X-Aliyun-Captcha-Verify-Region` 的值）。
//
// # 为什么需要这个访问器
//
// 它与 param **成对**：实测缺 region 就是 3007（见 cred.go 的注释）。
// 而 region 是求解器的运营参数（可能被上游配置覆盖，见 SetCaptchaConfig），
// 故**不能**在调用方硬编码 —— 那样一旦上游换区域，我们会带着
// 旧 region 去配新 param，结果是"求解成功但仍 3007"，极难查。
//
// 空串表示未配置，调用方应回落默认值。
func (s *CaptchaSolver) Region() string {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.region
}

// resolveNode 找到可用的 node 可执行文件。
//
// 顺序：环境变量 ZCODE_NODE_PATH → PATH 里的 node。
//
// ⚠ 为什么不用内置 node：node.exe 有 **87MB**，打进发行包代价过大
//（实测 nvmd 里的真实二进制）。故这里如实探测，找不到就报清楚 ——
// 而不是静默失效。
func (s *CaptchaSolver) resolveNode() (string, error) {
	if s.nodePath != "" {
		return s.nodePath, nil
	}
	if s.nodeErr != nil {
		return "", s.nodeErr
	}
	if env := strings.TrimSpace(os.Getenv("ZCODE_NODE_PATH")); env != "" {
		if _, err := os.Stat(env); err == nil {
			s.nodePath = env
			return env, nil
		}
	}
	name := "node"
	if runtime.GOOS == "windows" {
		name = "node.exe"
	}
	if p, err := exec.LookPath(name); err == nil {
		s.nodePath = p
		return p, nil
	}
	s.nodeErr = fmt.Errorf("找不到 node 可执行文件（求解验证码需要 Node.js；" +
		"可安装 Node.js，或用环境变量 ZCODE_NODE_PATH 指定路径）")
	return "", s.nodeErr
}

// entryFile 选出要执行的求解器入口（优先打包好的单文件）。
//
// 返回 (入口文件名, 错误)。两个都不在时报错 —— 空目录会被当成
// "有效组件"，于是求解时才发现缺文件（fail late）。
//
// 调用方需持锁（读 s.dir 之外无共享状态，但保持一致）。
func (s *CaptchaSolver) entryFile() (string, error) {
	if s.dir == "" {
		return "", fmt.Errorf("求解器组件未安装")
	}
	// 优先单文件 bundle：它就是为"安装快"而打出来的
	if _, err := os.Stat(filepath.Join(s.dir, captchaEntryBundle)); err == nil {
		return captchaEntryBundle, nil
	}
	if _, err := os.Stat(filepath.Join(s.dir, captchaEntrySource)); err == nil {
		return captchaEntrySource, nil
	}
	return "", fmt.Errorf("求解器目录里既没有 %s 也没有 %s：%s",
		captchaEntryBundle, captchaEntrySource, s.dir)
}

// Available 报告求解器当前是否可用（供界面判断要不要显示开关）。
func (s *CaptchaSolver) Available() bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	if _, err := s.entryFile(); err != nil {
		return false
	}
	_, err := s.resolveNode()
	return err == nil
}

// UnavailableReason 不可用的原因（为空 = 可用）。
func (s *CaptchaSolver) UnavailableReason() string {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.dir == "" {
		return "求解器组件未安装（发行包应包含 assets/zcode-captcha）"
	}
	if _, err := s.entryFile(); err != nil {
		return err.Error()
	}
	if _, err := s.resolveNode(); err != nil {
		return err.Error()
	}
	return ""
}

// Solve 求解一个 verifyParam。
//
// ⚠ 返回值是**一次性**的：用一次就作废，不要缓存复用（实测第二次必回 3007）。
func (s *CaptchaSolver) Solve(ctx context.Context) (string, error) {
	s.mu.Lock()
	dir := s.dir
	scene, region, prefix := s.scene, s.region, s.prefix
	// 冷却检查
	if !s.cooldownUntil.IsZero() && time.Now().Before(s.cooldownUntil) {
		left := time.Until(s.cooldownUntil).Round(time.Second)
		reason := s.cooldownReason
		s.mu.Unlock()
		return "", fmt.Errorf("验证码求解处于冷却中（还需 %s%s）", left,
			func() string {
				if reason == "" {
					return ""
				}
				return "：" + reason
			}())
	}
	s.mu.Unlock()

	if dir == "" {
		return "", fmt.Errorf("求解器组件未安装")
	}
	node, err := func() (string, error) {
		s.mu.Lock()
		defer s.mu.Unlock()
		return s.resolveNode()
	}()
	if err != nil {
		return "", err
	}
	// 入口文件在锁内解析（它读 s.dir，与 SetDir 竞争）
	entry, err := func() (string, error) {
		s.mu.Lock()
		defer s.mu.Unlock()
		return s.entryFile()
	}()
	if err != nil {
		return "", err
	}

	cctx, cancel := context.WithTimeout(ctx, 90*time.Second)
	defer cancel()

	cmd := exec.CommandContext(cctx, node, entry, scene, region, prefix)
	cmd.Dir = dir

	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return "", err
	}
	var stderr strings.Builder
	cmd.Stderr = &stderr

	if err := cmd.Start(); err != nil {
		return "", fmt.Errorf("启动求解器失败: %w", err)
	}

	// ⚠ 只读 **stdout**：调试信息走 stderr（`[pe-stall] …`）。
	// 合并两者会被调试信息抢先 —— 我因此误判过一次"求解器不可用"。
	var param string
	sc := bufio.NewScanner(stdout)
	sc.Buffer(make([]byte, 0, 64*1024), 1<<20)
	for sc.Scan() {
		line := strings.TrimSpace(sc.Text())
		if v, ok := strings.CutPrefix(line, "VERIFY_PARAM="); ok {
			param = strings.TrimSpace(v)
		}
	}

	waitErr := cmd.Wait()
	stderrText := strings.TrimSpace(stderr.String())

	if param != "" {
		// 成功：清掉冷却
		s.mu.Lock()
		s.cooldownUntil = time.Time{}
		s.cooldownReason = ""
		s.mu.Unlock()
		return param, nil
	}

	// 失败：区分"被限流"与"其它错误"，前者进冷却
	limited := strings.Contains(stderrText, "pe-stall") ||
		strings.Contains(stderrText, "captcha solve stall") ||
		strings.Contains(stderrText, "timeout")
	if limited {
		s.mu.Lock()
		s.cooldownUntil = time.Now().Add(DefaultCaptchaCooldown)
		s.cooldownReason = "被上游限流（多次求解会被限制，属正常保护）"
		s.mu.Unlock()
		return "", fmt.Errorf("验证码求解被上游限流，已冷却 %s：%s",
			DefaultCaptchaCooldown, firstLine(stderrText))
	}

	msg := firstLine(stderrText)
	if msg == "" && waitErr != nil {
		msg = waitErr.Error()
	}
	return "", fmt.Errorf("验证码求解失败：%s", msg)
}

// CooldownLeft 距离冷却结束还有多久（0 = 不在冷却）。
func (s *CaptchaSolver) CooldownLeft() time.Duration {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.cooldownUntil.IsZero() {
		return 0
	}
	if d := time.Until(s.cooldownUntil); d > 0 {
		return d.Round(time.Second)
	}
	return 0
}

func firstLine(s string) string {
	if i := strings.IndexByte(s, '\n'); i >= 0 {
		s = s[:i]
	}
	return strings.TrimSpace(s)
}
