package zcode

// logincli.go 供宿主调用的登录子命令实现。
//
// ## 为什么是子命令而不是 HTTP 接口
//
// 与 Qoder 同样的理由：宿主需要在**网关未启动**时也能登录 ——
// 登录是配置阶段的事，而网关可能因为配置不完整还没跑起来。
//
// ## 会话必须落盘
//
// 发起与轮询是**两个独立进程**：
//
//	宿主 → gateway.exe zcode-login start --provider zai   （进程 A，退出）
//	宿主 → gateway.exe zcode-login poll  --session <id>    （进程 B，新进程）
//
// 进程 A 生成的 pollToken 必须被进程 B 拿到（轮询要用它做 Bearer），
// 否则轮询会 401。故进程 A 把会话写进临时文件。

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"time"
)

// sessionTTL 会话文件的最长存活时间。
//
// 用户在浏览器里授权可能要几分钟（登录、二次验证、切换账号）。
// 参考实现的登录超时是 5 分钟，这里给 30 分钟留足余量 ——
// 反正会话文件过期会自动清理。
const sessionTTL = 30 * time.Minute

// sessionsDir 会话文件目录。
//
// 放在 authDir 的**同级**（不是里面）：authDir 会被 LoadDir 用
// glob `zcode*.json` 扫描，会话文件混进去会被当成凭证解析并报错。
func sessionsDir(authDir string) string {
	return filepath.Join(filepath.Dir(authDir), "sessions")
}

func sessionFile(authDir, flowID string) string {
	return filepath.Join(sessionsDir(authDir), sanitizeName(flowID)+".json")
}

// sanitizeName 把任意字符串变成安全的文件名片段（防路径穿越）。
//
// 安全项：flowID 来自上游响应，直接拼进路径的话，
// 恶意/异常的 flowID（如 "../../etc/passwd"）会写到目录外。
func sanitizeName(s string) string {
	out := make([]rune, 0, len(s))
	for _, r := range s {
		switch {
		case r >= 'a' && r <= 'z', r >= 'A' && r <= 'Z', r >= '0' && r <= '9', r == '-', r == '_':
			out = append(out, r)
		default:
			out = append(out, '_')
		}
	}
	if len(out) == 0 {
		return "unknown"
	}
	return string(out)
}

func saveSession(authDir string, s *LoginSession) error {
	dir := sessionsDir(authDir)
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return fmt.Errorf("创建会话目录失败: %w", err)
	}
	raw, err := json.MarshalIndent(s, "", "  ")
	if err != nil {
		return err
	}
	// 0600：会话里含 pollToken（泄漏它等于让别人能抢答轮询）
	return os.WriteFile(sessionFile(authDir, s.FlowID), raw, 0o600)
}

func loadSession(authDir, flowID string) (*LoginSession, error) {
	path := sessionFile(authDir, flowID)
	raw, err := os.ReadFile(path)
	if err != nil {
		if os.IsNotExist(err) {
			return nil, fmt.Errorf("登录会话不存在或已过期（%s）。请重新发起登录", flowID)
		}
		return nil, err
	}
	var s LoginSession
	if err := json.Unmarshal(raw, &s); err != nil {
		return nil, fmt.Errorf("会话文件损坏: %w", err)
	}
	if s.CreatedAt > 0 && time.Since(time.Unix(s.CreatedAt, 0)) > sessionTTL {
		_ = os.Remove(path)
		return nil, fmt.Errorf("登录会话已超过 %d 分钟，请重新发起登录", int(sessionTTL.Minutes()))
	}
	return &s, nil
}

// captchaConfig 是网关配置里的验证码求解器参数（纯数据）。
//
// 单独抽成类型是为了让 `loadCaptchaConfig` 保持**纯**：只读文件、只返回值，
// 不碰任何包级状态。为什么这很重要见下。
type captchaConfig struct {
	Dir   string
	URL   string
	Token string
}

// loadCaptchaConfig 从网关配置文件里读出验证码求解器的三个参数。
//
// # 为什么需要它（这是一处真实的接线缺口，不是预防性加固）
//
// 求解器**只在服务模式被配置**：`cmd/server/main.go` 在 `flag.Parse()` 之前
// 就拦截了 `qoder-login` / `zcode-login` 并 `os.Exit`，配置**根本不读**。
// 于是 `gateway.exe zcode-login warm --uid …` 走到 `warm.go::mintCaptcha`
// → `client.go::solveCaptcha` → `SharedCaptchaSolver()`（dir 与 externalURL
// 都是空）→ `UnavailableReason()` 返回「求解器组件未安装」。
//
// 后果是 `warm` 这条**宿主要用的路径**永远拿不到 param，领取阶段必回 3007
// —— 而服务模式的同一功能是好的。这正是 captcha.go 注释里记过的那类缺陷：
// 「新路径接好了、旧的前置检查把它挡在门外」。
//
// 形状照抄 `qoder/logincli.go` 的 `loadProxyConfig`（同样的三个候选位置、
// 同样「找不到就不报错」）：两者读的是**同一份** `gateway_native_config.json`。
//
// # ⚠ 本函数必须是纯的（只读文件、只返回值）
//
// 曾经把它写成「读配置 + 直接 SetDir/SetExternalSolver」的一体式函数，并在
// `RunLoginCLI` 开头**无条件**调用。实测证伪：那会**污染包级共享求解器**。
//
// `captcha_link_test.go` 明写着本包的不变量：「本包测试从不给
// `SharedCaptchaSolver()` 设 dir / 外部求解服务，故 `UnavailableReason()`
// 必非空 ⇒ `solveCaptcha` 确定性失败」。而本机
// `~/.wb-switch/gateway/gateway_native_config.json` 真实存在（候选②命中），
// 于是任意一次 `RunLoginCLI` 都会把这个不变量翻掉：
//
//	BEFORE: "求解器组件未安装（发行包应包含 assets/zcode-captcha）"
//	AFTER:  ""   DIR="D:\\APP\\AI Gateway\\_up_\\assets\\zcode-captcha"
//
// 当时全包 `go test` 仍绿，纯属 `captcha_link_test.go` 字典序在前、先跑完的
// **偶然**（`logincli_warm_test.go` 有四处调 `RunLoginCLI`），不是设计保证。
//
// 故这里只返回值；「装到求解器上」那一步交给 `installCaptchaSolver`，且只在
// 真正要求解的子命令里调。
func loadCaptchaConfig(authDir string) captchaConfig {
	// 配置文件与凭证目录同属一个数据目录：
	//   <data>/gateway/gateway_native_config.json
	//   <data>/zcode/auths
	// 用 authDir 往上找而不是写死 %USERPROFILE% —— 既支持自定义数据目录，
	// 也让测试能指向临时目录。
	var candidates []string
	if d := filepath.Dir(filepath.Dir(authDir)); d != "" {
		candidates = append(candidates,
			filepath.Join(d, "gateway", "gateway_native_config.json"))
	}
	if home, err := os.UserHomeDir(); err == nil {
		candidates = append(candidates,
			filepath.Join(home, ".wb-switch", "gateway", "gateway_native_config.json"))
	}
	if env := strings.TrimSpace(os.Getenv("AI_GATEWAY_HOME")); env != "" {
		candidates = append(candidates,
			filepath.Join(env, "gateway", "gateway_native_config.json"))
	}

	for _, p := range candidates {
		raw, err := os.ReadFile(p)
		if err != nil {
			continue
		}
		// ⚠ 这几个 captcha 键在 `pool` 段**内**，不在顶层 ——
		// 顶层解不出来会静默拿到空串，表现与"文件不存在"一模一样。
		//
		// `zcode_captcha_enabled` 刻意**不解析**：见下面"与 main.go 同口径"。
		var doc struct {
			Pool struct {
				Dir   string `json:"zcode_captcha_dir"`
				URL   string `json:"zcode_captcha_solver_url"`
				Token string `json:"zcode_captcha_solver_token"`
			} `json:"pool"`
		}
		if json.Unmarshal(raw, &doc) != nil {
			continue
		}

		// 与 main.go 同口径：**刻意不读 zcode_captcha_enabled**。
		// 老配置里可能留着 false，据此关闭会让升级后的用户"功能永远关着"。
		return captchaConfig{
			Dir:   strings.TrimSpace(doc.Pool.Dir),
			URL:   strings.TrimSpace(doc.Pool.URL),
			Token: strings.TrimSpace(doc.Pool.Token),
		}
	}
	// 找不到配置文件不是错误：调用方拿零值，求解器保持不可用，
	// 与"用户没配求解器"表现一致（而不是凭空报一个读文件失败）。
	return captchaConfig{}
}

// installCaptchaSolver 把 loadCaptchaConfig 读到的参数装到共享求解器上。
//
// ⚠ 只在**真正要求解**的子命令里调（当前只有 `warm`），且要在凭证加载成功
// **之后**调 —— 见 loadCaptchaConfig 的注释：无条件调用会污染包级单例，
// 把 `captcha_link_test.go` 依赖的那个不变量翻掉。
//
// 当前只有 `warm` 需要：`claim-plan` 的 doc 明写着「本命令不主动求解」，
// 它只把调用方给的 `Cred.CaptchaParam` 转发出去（claim.go:249）。
func installCaptchaSolver(authDir string) {
	cfg := loadCaptchaConfig(authDir)
	if cfg.Dir == "" && cfg.URL == "" {
		// 一个都没配：不必碰单例，让 UnavailableReason() 保持原样。
		return
	}

	cs := SharedCaptchaSolver()
	if cfg.Dir != "" {
		cs.SetDir(cfg.Dir)
	}
	// 外部求解（宿主 WebView2）优先：真实浏览器环境，比模拟环境更快更稳。
	if cfg.URL != "" {
		cs.SetExternalSolver(cfg.URL, cfg.Token)
	}
	// 配置完仍不可用就**如实报**到 stderr（不阻断子命令）：
	// 否则用户只会看到 3007，而不知道为什么。
	if reason := cs.UnavailableReason(); reason != "" {
		fmt.Fprintf(os.Stderr,
			"⚠ ZCode 验证码求解器不可用：%s（本次领取会因 3007 失败）\n", reason)
	}
}

// RunLoginCLI 执行 zcode-login 子命令。
//
// 用法：
//
//	gateway zcode-login start --provider zai|bigmodel [--auth-dir <dir>]
//	gateway zcode-login poll  --session <flow_id> --auth-dir <dir>
//	gateway zcode-login import --credential <key.secret> [--provider zai] --auth-dir <dir>
//
// 输出一律是**单行 JSON**（宿主按行解析），错误写 stderr 并以非零码退出。
func RunLoginCLI(args []string, defaultAuthDir string) int {
	// ⚠ 这里**刻意不配置验证码求解器**：子命令模式下 `main.go` 确实不读配置，
	// 但求解器只有 `warm` 用得上，装它必须延后到真正要用的地方
	// （`runWarm` 里、凭证加载成功之后）。在这里无条件装会改包级单例，
	// 破坏 `captcha_link_test.go` 依赖的不变量 —— 详见 loadCaptchaConfig。
	if len(args) == 0 {
		// ⚠ 帮助文案必须与**实际派发的子命令**一致（同 qoder 侧的教训）：
		// 旧文案只列 start|poll|import，漏了 quota / models，
		// 排查时会误判「子命令没注册」。
		fmt.Fprintln(os.Stderr, "用法: zcode-login <start|poll|import|quota|models|preview-plan|claim-plan|warm> [选项]")
		return 2
	}
	switch args[0] {
	case "start":
		return runStart(args[1:], defaultAuthDir)
	case "poll":
		return runPoll(args[1:], defaultAuthDir)
	case "import":
		return runImport(args[1:], defaultAuthDir)
	case "quota":
		// 查额度并输出 JSON。
		//
		// ## 为什么需要子命令
		//
		// `Client.FetchQuota` 早就实现了，但**从来没有生产者调用它** ——
		// 实测确认（`grep FetchQuota` 只有测试与定义本身）。于是界面上
		// 额度恒为 0、到期时间恒为空，用户以为"查不到额度"。
		//
		// 宿主侧要的是「按 uid 查一次并拿回结构化结果」，故这里给一个
		// CLI 入口，形状与 import 一致（单行 JSON）。
		return runQuota(args[1:], defaultAuthDir)
	case "models":
		// 查该账号**实际可用**的模型清单。
		//
		// 同理：`Client.FetchModels` 早已实现，但没有生产者调用 ——
		// 界面上看不到"这个账号能用哪些模型"。
		//
		// ⚠ 是**按账号**查（走该账号的凭证），不是查全局清单：
		// 不同账号/服务商的可用模型不同，全局列表会误导。
		return runModels(args[1:], defaultAuthDir)
	case "preview-plan":
		// 查**可领取**的套餐（限时活动）。
		//
		// 与 `quota` 是两个端点，别混：
		//	quota        → billing/balance  **已生效**的套餐与余额
		//	preview-plan → billing/preview  **可领取**的套餐
		return runPreviewPlan(args[1:], defaultAuthDir)
	case "claim-plan":
		// 领取套餐（**写操作**）。
		//
		// ⚠ 只由用户显式触发（界面点击或本子命令），**不做定时自动抢**。
		return runClaimPlan(args[1:], defaultAuthDir)
	case "warm":
		// 激活账号（**写操作**）：报两个激活事件，再领上游给出的所有套餐。
		//
		// ⚠ 同样只由用户显式触发。绝不挂进 claim_scheduler 的自动轮询 ——
		// 定时自动"激活 + 领取"会让账号表现出非人类的活动模式。
		return runWarm(args[1:], defaultAuthDir)
	default:
		fmt.Fprintf(os.Stderr, "未知子命令 %q（应为 start / poll / import / quota / models / preview-plan / claim-plan / warm）\n", args[0])
		return 2
	}
}

func runStart(args []string, defaultAuthDir string) int {
	fs := flag.NewFlagSet("zcode-login start", flag.ContinueOnError)
	provider := fs.String("provider", "", "服务商：zai 或 bigmodel")
	authDir := fs.String("auth-dir", defaultAuthDir, "凭证目录")
	if err := fs.Parse(args); err != nil {
		return 2
	}

	p := ParseProvider(*provider)
	if p == ProviderUnknown {
		// 不默认成某个服务商：两者的授权页与端点都不同，猜错会让用户
		// 打开错误服务商的页面、登录后账号却指向另一个。
		fmt.Fprintln(os.Stderr, "必须指定服务商（--provider zai 或 --provider bigmodel）")
		return 2
	}

	c := New()
	s, err := c.StartLogin(context.Background(), p)
	if err != nil {
		fmt.Fprintf(os.Stderr, "%v\n", err)
		return 1
	}
	if err := saveSession(*authDir, s); err != nil {
		fmt.Fprintf(os.Stderr, "保存登录会话失败: %v\n", err)
		return 1
	}

	writeJSON(map[string]any{
		"sessionId":     s.FlowID,
		"authUrl":       s.AuthorizeURL,
		"provider":      string(s.Provider),
		"expiresAt":     s.ExpiresAt,
		"pollIntervalS": s.PollIntervalSec,
	})
	return 0
}

func runPoll(args []string, defaultAuthDir string) int {
	fs := flag.NewFlagSet("zcode-login poll", flag.ContinueOnError)
	sessionID := fs.String("session", "", "会话标识（start 返回的 sessionId）")
	authDir := fs.String("auth-dir", defaultAuthDir, "凭证目录")
	if err := fs.Parse(args); err != nil {
		return 2
	}
	if *sessionID == "" {
		fmt.Fprintln(os.Stderr, "缺少 --session 参数")
		return 2
	}

	s, err := loadSession(*authDir, *sessionID)
	if err != nil {
		fmt.Fprintf(os.Stderr, "%v\n", err)
		return 1
	}

	c := New()
	tokens, err := c.PollOnce(context.Background(), s)
	if err != nil {
		// 未授权：**正常中间态**，不是错误
		if _, ok := err.(LoginPending); ok {
			writeJSON(map[string]any{"status": "pending"})
			return 0
		}
		// 真失败：清掉会话（用户要重新发起）
		_ = os.Remove(sessionFile(*authDir, *sessionID))
		fmt.Fprintf(os.Stderr, "%v\n", err)
		return 1
	}

	// 授权成功 → 兑换最终凭证
	cred, err := c.ExchangeCredential(context.Background(), tokens)
	if err != nil {
		fmt.Fprintf(os.Stderr, "兑换凭证失败: %v\n", err)
		return 1
	}

	// 落盘（文件名与 LoadDir 的 glob 对齐）
	if err := os.MkdirAll(*authDir, 0o700); err != nil {
		fmt.Fprintf(os.Stderr, "创建凭证目录失败: %v\n", err)
		return 1
	}
	cred.FilePath = filepath.Join(*authDir, cred.FileBaseName())
	if err := cred.SaveAtomic(); err != nil {
		fmt.Fprintf(os.Stderr, "保存凭证失败: %v\n", err)
		return 1
	}
	// 用完即删会话
	_ = os.Remove(sessionFile(*authDir, *sessionID))

	writeJSON(map[string]any{
		"status":   "ok",
		"uid":      cred.UID,
		"provider": string(cred.Provider),
		// 脱敏：凭证不能出现在 stdout（宿主可能记日志）
		"credential": cred.MaskedCredential(),
		"hasJwt":     cred.JWT != "",
	})
	return 0
}

// runImport 导入用户粘贴的凭证（ZCode 的**主路径**）。
//
// ZCode 与 Qoder 不同：凭证是用户能从控制台复制的字符串，
// 故"粘贴导入"比 OAuth 更常用。
// runQuota 查一个账号的额度并输出 JSON。
//
// 用法：`zcode-login quota --uid <uid> --auth-dir <dir>`
//
// 输出（成功）：
//
//	{"status":"ok","uid":"...","remaining":N,"total":N,"used":N,
//	 "expiresAt":<Unix 秒>,"entries":[{"showName":"...","remaining":N,...}]}
//
// 输出（该账号没有 JWT —— 只导入了凭证，额度查不到）：
//
//	{"status":"no_jwt","uid":"...","message":"..."}
//
// ⚠ `no_jwt` 是**正常状态**而不是错误：ZCode 的额度查询只认 JWT，
// 而只导入对话凭证的账号本来就没有。宿主据此显示「额度未知」
// 而不是 0（0 会被用户误读成"额度耗尽"）。
// loadCredForPlan 按 uid 载入凭证，并**强制要求 JWT**。
//
// preview-plan / claim-plan 两个端点都只认 JWT（`Authorization: Bearer <jwt>`），
// 不认 `{apiKey}.{secret}` 对话凭证。缺 JWT 时**如实说明**，
// 而不是发一个必然 401 的请求上去（那会让用户以为"账号没资格"，
// 实际只是"导入时没带 JWT"）。
func loadCredForPlan(authDir, uid string) (*Cred, int) {
	if strings.TrimSpace(uid) == "" {
		fmt.Fprintln(os.Stderr, "缺少 --uid 参数")
		return nil, 2
	}
	c, err := loadCredByUID(authDir, uid)
	if err != nil {
		fmt.Fprintf(os.Stderr, "%v\n", err)
		return nil, 1
	}
	if strings.TrimSpace(c.JWT) == "" {
		writeJSON(map[string]any{
			"status":  "no_jwt",
			"uid":     c.UID,
			"message": "该账号只导入了对话凭证，没有套餐查询/领取用的 JWT",
		})
		return nil, 0
	}
	return c, 0
}

// runPreviewPlan 查**可领取**的套餐（billing/preview）。
//
// 与 `quota` 的区别（两个端点，别混）：
//
//	quota        → billing/balance  **已生效**的套餐与余额
//	preview-plan → billing/preview  **可领取**的套餐（限时活动）
//
// ⚠ 该端点**必须带 UUID 形态的 `X-Device-Mid`**，缺它上游回
// `400 {"code":3001,"msg":"parameter error"}` —— 实测确认
//（见 `uitest/diag-zcode-claim.cjs`）。头由 `ControlPlaneHeaders` 提供。
func runPreviewPlan(args []string, defaultAuthDir string) int {
	fs := flag.NewFlagSet("zcode-login preview-plan", flag.ContinueOnError)
	uid := fs.String("uid", "", "账号 uid")
	authDir := fs.String("auth-dir", defaultAuthDir, "凭证目录")
	if err := fs.Parse(args); err != nil {
		return 2
	}

	c, code := loadCredForPlan(*authDir, *uid)
	if c == nil {
		return code
	}

	pv, err := New().FetchPlanPreview(context.Background(), c)
	if err != nil {
		// 与 quota 同一口径：错误回传给宿主显示，但**退出码 0** ——
		// 否则宿主会把"上游返回业务错误"当成"网关命令执行失败"，
		// 用户看到的是笼统的"命令失败"而不是上游那句具体原因。
		writeJSON(map[string]any{"status": "error", "uid": c.UID, "message": err.Error()})
		return 0
	}

	offers := make([]map[string]any, 0, len(pv.Offers))
	for _, o := range pv.Offers {
		offers = append(offers, map[string]any{
			"planId": o.PlanID,
			"name":   o.Name,
			"status": o.Status,
			"endsAt": o.EndsAt,
		})
	}
	writeJSON(map[string]any{
		"status":     "ok",
		"uid":        c.UID,
		"serverTime": pv.ServerTime,
		"offers":     offers,
		// 显式给出"有没有可领的"，让宿主不必自己判空数组
		"claimable": len(offers),
	})
	return 0
}

// runClaimPlan 领取一个套餐（billing/claim）。
//
// # ⚠ 这是**写操作**
//
// 只由用户显式触发（界面点击或本子命令），**不做定时自动抢** ——
// 定时抢会让账号表现出非人类的活动模式（与 Qoder campaign 同一取舍）。
//
// # 验证码
//
// 该端点可能要求 `X-Aliyun-Captcha-Verify-Param`。本命令**不主动求解**：
// 求解器是可选组件（需用户显式启用），且求解会被限流。缺验证码时上游回
// `3007`，我们如实透传 —— 由调用方决定要不要先求解一个。
func runClaimPlan(args []string, defaultAuthDir string) int {
	fs := flag.NewFlagSet("zcode-login claim-plan", flag.ContinueOnError)
	uid := fs.String("uid", "", "账号 uid")
	planID := fs.String("plan-id", "", "要领取的套餐 ID（从 preview-plan 拿）")
	authDir := fs.String("auth-dir", defaultAuthDir, "凭证目录")
	if err := fs.Parse(args); err != nil {
		return 2
	}
	if strings.TrimSpace(*planID) == "" {
		// 本地拦下：不拦的话会发一个 `{"plan_id":""}` 上去，
		// 上游回 3001「parameter error」—— 那个文案完全看不出是
		// "我们没传 ID"，排查方向会被引向"是不是缺 X-Device-Mid"。
		fmt.Fprintln(os.Stderr, "缺少 --plan-id 参数（先用 preview-plan 查看可领的套餐）")
		return 2
	}

	c, code := loadCredForPlan(*authDir, *uid)
	if c == nil {
		return code
	}

	res, err := New().ClaimPlan(context.Background(), c, *planID)
	if err != nil {
		writeJSON(map[string]any{"status": "error", "uid": c.UID, "planId": *planID, "message": err.Error()})
		return 0
	}
	writeJSON(map[string]any{
		"status":         "ok",
		"uid":            c.UID,
		"planId":         *planID,
		"alreadyClaimed": res.AlreadyClaimed,
		"claimedAt":      res.ClaimedAt,
		// 已领过也算达成目标 —— 明确说出来，否则用户重复点会以为每次都失败
		"message": func() string {
			if res.AlreadyClaimed {
				return "该套餐此前已领取过（不是错误）"
			}
			return "领取成功"
		}(),
	})
	return 0
}

// runWarm 激活一个账号（**写操作**）：先报激活事件，再领上游给出的所有套餐。
//
// # 为什么需要它
//
// 刚导入的账号在上游看来"从未活动过"：额度查得到、模型列得出，但活动套餐
// 要么拿不到 offer、要么被判异常活动。官方客户端在用户首次启动时会报两个
// 激活事件（`app_launch` / `app_daily_active`），本条子命令复刻的就是那个
// 行为 —— 详见 warm.go 的文件头。
//
// # ⚠ 这是写操作，只由用户显式触发
//
// 与 `claim-plan` 同一取舍：**绝不挂进 claim_scheduler 的自动轮询**。定时
// 自动"激活 + 领取"会让账号表现出非人类的活动模式，而上游正是按活动模式
// 判风控的。
//
// ⚠ 本目录是历史 Go 实现，已不参与构建；Rust 版另有「宿主启动后每账号一次」
// 的幂等自动入口（`multi_product_credit_patrol::warm_zcode_accounts`）。
//
// # 退出码
//
// 与 `preview-plan` / `claim-plan` 一致：**上游业务状态回退出码 0**，
// 结果（含失败原因）写进 JSON 的 events / claim / skipped 三个字段
// （**空串 = 成功**）。只有"账号都加载不出来"才是非零退出码 ——
// 否则宿主会把"上游拒绝了这次领取"当成"网关命令执行失败"。
func runWarm(args []string, defaultAuthDir string) int {
	fs := flag.NewFlagSet("zcode-login warm", flag.ContinueOnError)
	uid := fs.String("uid", "", "账号 uid")
	authDir := fs.String("auth-dir", defaultAuthDir, "凭证目录")
	if err := fs.Parse(args); err != nil {
		return 2
	}

	// 复用 loadCredForPlan：激活的事件 body 需要 user_id（从 JWT 解）、
	// 领取需要 Authorization: Bearer <jwt>，所以缺 JWT 时**如实说明**
	// 而不是发一堆必然失败的请求上去。
	c, code := loadCredForPlan(*authDir, *uid)
	if c == nil {
		return code
	}

	// 求解器装在这里而不是 `RunLoginCLI` 开头：只有本子命令要求解，且必须在
	// 凭证确实加载出来之后 —— 早退路径（缺 --uid / 账号不存在 / 无 JWT）不该
	// 碰共享单例。原因见 loadCaptchaConfig 的注释（实测污染过一次）。
	installCaptchaSolver(*authDir)

	rep, err := New().WarmAccount(context.Background(), c)
	if err != nil {
		writeJSON(map[string]any{"status": "error", "uid": c.UID, "message": err.Error()})
		return 0
	}

	// 汇总一句话，让宿主/用户不必自己拼三个字段的语义。
	// 口径与 WarmReport 一致：空串 = 该阶段成功。
	summary := "激活完成"
	switch {
	case rep.Events != "":
		summary = "激活事件未发出，未执行领取"
	case rep.Skipped != "":
		summary = "激活事件已发出；未执行领取：" + rep.Skipped
	case rep.Claim != "":
		summary = fmt.Sprintf("激活事件已发出；领取部分失败：%s", rep.Claim)
	case len(rep.Claimed) > 0:
		summary = fmt.Sprintf("激活事件已发出；已领取 %d 个套餐", len(rep.Claimed))
	}

	writeJSON(map[string]any{
		"status":  "ok",
		"uid":     c.UID,
		"events":  rep.Events,
		"claim":   rep.Claim,
		"skipped": rep.Skipped,
		"plans":   rep.Plans,
		"claimed": rep.Claimed,
		"message": summary,
	})
	return 0
}

func runQuota(args []string, defaultAuthDir string) int {
	fs := flag.NewFlagSet("zcode-login quota", flag.ContinueOnError)
	uid := fs.String("uid", "", "账号 uid")
	authDir := fs.String("auth-dir", defaultAuthDir, "凭证目录")
	if err := fs.Parse(args); err != nil {
		return 2
	}
	if strings.TrimSpace(*uid) == "" {
		fmt.Fprintln(os.Stderr, "缺少 --uid 参数")
		return 2
	}

	c, err := loadCredByUID(*authDir, *uid)
	if err != nil {
		fmt.Fprintf(os.Stderr, "%v\n", err)
		return 1
	}

	if strings.TrimSpace(c.JWT) == "" {
		// 如实说明原因，而不是报错或返回 0
		writeJSON(map[string]any{
			"status":  "no_jwt",
			"uid":     c.UID,
			"message": "该账号只导入了对话凭证，没有额度查询用的 JWT",
		})
		return 0
	}

	cli := New()
	q, err := cli.FetchQuota(context.Background(), c)
	if err != nil {
		// 把上游错误回传（宿主据此显示具体原因），但**退出码为 0** ——
		// "查不到额度"不是命令执行失败，不该让调用方当成崩溃。
		writeJSON(map[string]any{
			"status":  "error",
			"uid":     c.UID,
			"message": err.Error(),
		})
		return 0
	}

	entries := make([]map[string]any, 0, len(q.Entries))
	for _, e := range q.Entries {
		entries = append(entries, map[string]any{
			"showName":  e.ShowName,
			"remaining": e.Remaining,
			"total":     e.Total,
			"used":      e.Used,
			"unitType":  e.UnitType,
			// ⚠ 这是**每日周期**的结束（实测当天 23:59:59），不是套餐到期。
			// 套餐到期见下面的 planExpiresAt。
			"expiresAt":   e.ExpiresAt,
			"periodStart": e.PeriodStart,
			"periodEnd":   e.PeriodEnd,
			// 该桶来自哪个套餐 —— 界面据此说"这 300 万是体验套餐送的"
			"planId":        e.PlanID,
			"entitlementId": e.EntitlementID,
			"grantUnits":    e.GrantUnits,
		})
	}

	// 套餐（含**整体到期**）。
	//
	// 所有者的实测反馈：「这个体验套餐是 9月23号23:59 过期时间」——
	// 那个时刻在 `plans[].ends_at` 里，而我们此前**完全没读 plans**。
	plans := make([]map[string]any, 0, len(q.Plans))
	for _, p := range q.Plans {
		ents := make([]map[string]any, 0, len(p.Entitlements))
		for _, e := range p.Entitlements {
			ents = append(ents, map[string]any{
				"entitlementId": e.EntitlementID,
				"showName":      e.ShowName,
				"grantUnits":    e.GrantUnits,
				"period":        e.Period,
				"unitType":      e.UnitType,
				"effectiveAt":   e.EffectiveAt,
			})
		}
		plans = append(plans, map[string]any{
			"planId":      p.PlanID,
			"name":        p.Name,
			"description": p.Description,
			"status":      p.Status,
			"priority":    p.Priority,
			"startsAt":    p.StartsAt,
			"endsAt":      p.EndsAt,
			"entitlements": ents,
		})
	}

	// 套餐类型（体验 / 付费 / 按量）—— 由 plan.go 按上游静态配置判定。
	//
	// 判据不硬编码数字：拿 `startPlanPreview` 的赠送量与实际总量比对。
	// 取不到配置时返回 unknown（**不猜**）。
	var planKind string
	if sp, serr := cli.FetchStartPlanPreview(context.Background()); serr == nil {
		planKind = string(ClassifyPlan(q, sp))
	} else {
		planKind = string(PlanUnknown)
	}

	writeJSON(map[string]any{
		"status":    "ok",
		"uid":       c.UID,
		"remaining": q.Remaining,
		"total":     q.Total,
		"used":      q.Used,
		// 最早到期时刻 —— 界面用它显示"最快要过期的额度"
		"expiresAt": q.SoonestExpiry(),
		"entries":   entries,
		// 套餐信息（含**套餐整体到期**）—— 与 entries 的每日周期到期不同
		"plans": plans,
		// 套餐整体到期（Unix 秒；0 = 无套餐/未知）
		"planExpiresAt": q.PlanExpiry(),
		"planKind":      planKind,
		// 该凭证**自己的**上游账号标识。
		//
		// 为什么由这里返回：宿主需要它来判断"客户端登录态里的身份是否属于
		// 这个账号"。没有它，宿主只能猜 —— 而猜错的后果是所有账号被贴上
		// 同一个身份（实测踩到：3 个不同账号的 accountId 全变成同一个）。
		"accountId": accountIDOf(c),
	})
	return 0
}

// accountIDOf 取凭证对应的上游账号标识。
//
// 优先用文件里已存的 `account_id`；没有就从 JWT 现解 ——
// 老版本导入的凭证文件里没有这个字段，现解能把它补上。
func accountIDOf(c *Cred) string {
	if c == nil {
		return ""
	}
	if c.AccountID != "" {
		return c.AccountID
	}
	if c.JWT != "" {
		return AccountIDFromJWT(c.JWT)
	}
	return ""
}

// runModels 查一个账号实际可用的模型并输出 JSON。
//
// 用法：`zcode-login models --uid <uid> --auth-dir <dir>`
//
// 输出：`{"status":"ok","uid":...,"count":N,"models":[{"id":..,"name":..,"contextWindow":..,"maxOutput":..,"reasoning":..,"vision":..}]}`
//
// 失败时同样返回 `{"status":"error",...}` 且退出码 0 —— 见 runQuota 的说明。
func runModels(args []string, defaultAuthDir string) int {
	fs := flag.NewFlagSet("zcode-login models", flag.ContinueOnError)
	uid := fs.String("uid", "", "账号 uid")
	authDir := fs.String("auth-dir", defaultAuthDir, "凭证目录")
	if err := fs.Parse(args); err != nil {
		return 2
	}
	if strings.TrimSpace(*uid) == "" {
		fmt.Fprintln(os.Stderr, "缺少 --uid 参数")
		return 2
	}

	c, err := loadCredByUID(*authDir, *uid)
	if err != nil {
		writeJSON(map[string]any{"status": "error", "uid": *uid, "message": err.Error()})
		return 0
	}

	cli := New()
	models, err := cli.FetchModels(context.Background(), c)
	if err != nil {
		writeJSON(map[string]any{"status": "error", "uid": c.UID, "message": err.Error()})
		return 0
	}

	list := make([]map[string]any, 0, len(models))
	for _, m := range models {
		list = append(list, map[string]any{
			"id":            m.ID,
			"name":          m.Name,
			"contextWindow": m.ContextWindow,
			"maxOutput":     m.MaxOutput,
			"reasoning":     m.Reasoning,
			"vision":        m.Vision,
			// builtin 来源 = 上游拿不到时的离线兜底清单
			"source": m.Source,
		})
	}

	writeJSON(map[string]any{
		"status": "ok",
		"uid":    c.UID,
		"count":  len(list),
		"models": list,
	})
	return 0
}

func loadCredByUID(dir, uid string) (*Cred, error) {
	want := uid
	if !strings.HasPrefix(want, "zcode-") {
		want = "zcode-" + want
	}
	p := filepath.Join(dir, want+".json")
	if _, err := os.Stat(p); err == nil {
		return LoadFile(p)
	}
	// 回退：文件名可能与 uid 不同（用户手工放的文件），逐个比对
	creds, _, _ := LoadDir(dir)
	for _, c := range creds {
		if c.UID == uid || c.UID == want {
			return c, nil
		}
	}
	return nil, fmt.Errorf("在 %s 里找不到 uid=%s 的凭证", dir, uid)
}

//
// 只用于展示 token 年龄（排障用）。参考实现明确说明：这个 JWT
// **没有 exp 字段**、不因时间过期 —— 故**不能**据此判断"是否过期"，
// 只有上游回 401/3012 才表示需要重新登录。
//
// 解不开或没有 iat 时返回 0（未知），不报错 —— 它只是个展示字段，
// 为它中断导入是不值得的。
func jwtIssuedAt(token string) int64 {
	parts := strings.Split(strings.TrimSpace(token), ".")
	if len(parts) != 3 {
		return 0
	}
	// base64url → base64（Go 的 RawURLEncoding 直接吃 base64url，无需补 '='）
	raw, err := base64.RawURLEncoding.DecodeString(parts[1])
	if err != nil {
		// 有些实现带 padding，兼容一下
		raw, err = base64.URLEncoding.DecodeString(parts[1])
		if err != nil {
			return 0
		}
	}
	var payload struct {
		Iat int64 `json:"iat"`
	}
	if json.Unmarshal(raw, &payload) != nil {
		return 0
	}
	return payload.Iat
}

func runImport(args []string, defaultAuthDir string) int {
	fs := flag.NewFlagSet("zcode-login import", flag.ContinueOnError)
	credential := fs.String("credential", "", "凭证（形如 apiKey.secret）")
	provider := fs.String("provider", "", "服务商：zai 或 bigmodel（可选，默认 zai）")
	nickname := fs.String("nickname", "", "昵称（可选）")
	// --jwt：额度查询用的 OAuth 令牌（start-plan）。
	//
	// ## 为什么导入时就要带上它（闭环的关键）
	//
	// 额度查询**只认 JWT**，不认 `{apiKey}.{secret}`。而 ZCode 客户端把它们
	// 分成两个 provider 条目落盘（coding-plan 是对话凭证、start-plan 是 JWT）。
	//
	// 若扫描导入只带对话凭证，用户会看到「额度未知」—— 而他明明有额度。
	// 故扫描器把同一个客户端配置里的两条**配对**导入：凭证给这条，
	// JWT 一并通过 `--jwt` 落进同一个凭证文件。
	jwt := fs.String("jwt", "", "额度查询用的 JWT（可选；客户端 start-plan 那条）")
	authDir := fs.String("auth-dir", defaultAuthDir, "凭证目录")
	if err := fs.Parse(args); err != nil {
		return 2
	}
	if *credential == "" {
		fmt.Fprintln(os.Stderr, "缺少 --credential 参数")
		return 2
	}

	cred, err := Parse(*credential)
	if err != nil {
		fmt.Fprintf(os.Stderr, "%v\n", err)
		return 1
	}
	cred.Provider = ParseProvider(*provider)
	if cred.Provider == ProviderUnknown {
		// 未指定服务商时**不猜**：猜错会让账号打到错误端点、必然失败。
		// 但导入场景下用户往往不填 —— 故这里回退到 Z.AI（更常见），
		// 并**明确告知**让用户知道可以改。
		cred.Provider = ProviderZAI
	}
	cred.Nickname = *nickname
	if strings.TrimSpace(*jwt) != "" {
		cred.JWT = strings.TrimSpace(*jwt)
		cred.JWTIssuedAt = jwtIssuedAt(cred.JWT)
		// 上游账号标识 —— 让界面能识别"同一账号的多把 key"
		cred.AccountID = AccountIDFromJWT(cred.JWT)
	}

	if err := os.MkdirAll(*authDir, 0o700); err != nil {
		fmt.Fprintf(os.Stderr, "创建凭证目录失败: %v\n", err)
		return 1
	}
	cred.FilePath = filepath.Join(*authDir, cred.FileBaseName())
	if err := cred.SaveAtomic(); err != nil {
		fmt.Fprintf(os.Stderr, "保存凭证失败: %v\n", err)
		return 1
	}

	writeJSON(map[string]any{
		"status":   "ok",
		"uid":      cred.UID,
		"provider": string(cred.Provider),
		"file":     filepath.Base(cred.FilePath),
	})
	return 0
}

// writeJSON 输出单行 JSON 到 stdout。
func writeJSON(v any) {
	raw, err := json.Marshal(v)
	if err != nil {
		fmt.Fprintf(os.Stderr, "序列化输出失败: %v\n", err)
		return
	}
	fmt.Println(string(raw))
}
