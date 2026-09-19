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
	if len(args) == 0 {
		fmt.Fprintln(os.Stderr, "用法: zcode-login <start|poll|import> [选项]")
		return 2
	}
	switch args[0] {
	case "start":
		return runStart(args[1:], defaultAuthDir)
	case "poll":
		return runPoll(args[1:], defaultAuthDir)
	case "import":
		return runImport(args[1:], defaultAuthDir)
	default:
		fmt.Fprintf(os.Stderr, "未知子命令 %q（应为 start / poll / import）\n", args[0])
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
// jwtIssuedAt 从 JWT 里读出 `iat`（签发时刻），**不验签**。
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
