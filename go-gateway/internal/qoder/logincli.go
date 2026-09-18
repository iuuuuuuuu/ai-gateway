package qoder

// logincli.go 供宿主调用的登录子命令实现。
//
// ## 为什么是子命令而不是 HTTP 接口
//
// 宿主（Tauri）需要在**网关未启动**时也能登录 Qoder —— 登录是配置阶段的事，
// 而网关可能因为配置不完整还没跑起来。子命令无状态依赖，最可靠。
//
// ## 为什么会话要落盘
//
// 发起登录与轮询结果是**两个独立进程**：
//
//	宿主 → gateway.exe qoder-login url  --region cn      （进程 A，退出）
//	宿主 → gateway.exe qoder-login poll --session <id>   （进程 B，新进程）
//
// 进程 A 生成的 PKCE verifier 与机器指纹必须被进程 B 拿到，
// 否则轮询会因 verifier 不匹配而失败。故进程 A 把会话写进临时文件，
// 进程 B 按 sessionId 读回。

import (
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"time"
)

// DefaultAuthDir 宿主未显式指定时的凭证目录。
//
// 与宿主侧 qoder_account::auth_dir() 必须一致（~/.wb-switch/qoder/auths），
// 否则「用子命令登录」与「界面读凭证」会看到不同的目录 ——
// 表现为"登录成功了但账号列表里没有"。
func DefaultAuthDir() string {
	home, err := os.UserHomeDir()
	if err != nil || home == "" {
		return filepath.Join(".wb-switch", "qoder", "auths")
	}
	return filepath.Join(home, ".wb-switch", "qoder", "auths")
}

// sessionTTL 会话文件的最长存活时间。
//
// 用户在浏览器里授权可能要几分钟（登录、二次验证、切换账号），
// 但也不该无限期留着 —— 过期文件会越积越多。
const sessionTTL = 30 * time.Minute

// sessionsDir 会话文件的存放目录。
//
// 放在 authDir 的**同级**（不是 authDir 里面）：authDir 会被 Go 侧用
// glob `qoder*.json` 扫描，会话文件混进去会被当成凭证解析并报错。
func sessionsDir(authDir string) string {
	return filepath.Join(filepath.Dir(authDir), "sessions")
}

// sessionFile 某个会话的文件路径。
func sessionFile(authDir, sessionID string) string {
	return filepath.Join(sessionsDir(authDir), sanitizeUID(sessionID)+".json")
}

// saveSession 把登录会话写入临时文件。
func saveSession(authDir string, s *LoginSession) error {
	dir := sessionsDir(authDir)
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return fmt.Errorf("创建会话目录失败: %w", err)
	}
	// 记录创建时间，供过期清理使用
	doc := map[string]any{
		"verifier":   s.Verifier,
		"nonce":      s.Nonce,
		"machine_id": s.MachineID,
		"auth_url":   s.AuthURL,
		"region":     string(s.Region),
		"created_at": time.Now().Unix(),
	}
	raw, err := json.MarshalIndent(doc, "", "  ")
	if err != nil {
		return err
	}
	// 0600：会话里含 PKCE verifier，泄漏它等于让别人能抢先完成授权
	return os.WriteFile(sessionFile(authDir, s.Nonce), raw, 0o600)
}

// loadSession 读回会话；不存在或已过期时返回可读错误。
func loadSession(authDir, sessionID string) (*LoginSession, error) {
	path := sessionFile(authDir, sessionID)
	raw, err := os.ReadFile(path)
	if err != nil {
		if os.IsNotExist(err) {
			return nil, fmt.Errorf("登录会话不存在或已过期（%s）。请重新点击「登录」发起一次授权", sessionID)
		}
		return nil, err
	}
	var doc struct {
		Verifier  string `json:"verifier"`
		Nonce     string `json:"nonce"`
		MachineID string `json:"machine_id"`
		AuthURL   string `json:"auth_url"`
		Region    string `json:"region"`
		CreatedAt int64  `json:"created_at"`
	}
	if err := json.Unmarshal(raw, &doc); err != nil {
		return nil, fmt.Errorf("会话文件损坏: %w", err)
	}
	if doc.CreatedAt > 0 && time.Since(time.Unix(doc.CreatedAt, 0)) > sessionTTL {
		_ = os.Remove(path)
		return nil, fmt.Errorf("登录会话已超过 %d 分钟，请重新发起登录", int(sessionTTL.Minutes()))
	}
	return &LoginSession{
		Verifier:  doc.Verifier,
		Nonce:     doc.Nonce,
		MachineID: doc.MachineID,
		AuthURL:   doc.AuthURL,
		Region:    Region(doc.Region),
	}, nil
}

// RunLoginCLI 执行 qoder-login 子命令。
//
// 用法：
//
//	gateway qoder-login url  --region cn|intl [--auth-dir <dir>]
//	gateway qoder-login poll --session <id> --auth-dir <dir>
//
// 输出一律是**单行 JSON**（宿主按行解析），错误写 stderr 并以非零码退出。
//
// 返回值即进程退出码。
func RunLoginCLI(args []string, defaultAuthDir string) int {
	if len(args) == 0 {
		fmt.Fprintln(os.Stderr, "用法: qoder-login <url|poll> [选项]")
		return 2
	}
	sub := args[0]

	switch sub {
	case "url":
		return runLoginURL(args[1:], defaultAuthDir)
	case "poll":
		return runLoginPoll(args[1:], defaultAuthDir)
	default:
		fmt.Fprintf(os.Stderr, "未知子命令 %q（应为 url 或 poll）\n", sub)
		return 2
	}
}

func runLoginURL(args []string, defaultAuthDir string) int {
	fs := flag.NewFlagSet("qoder-login url", flag.ContinueOnError)
	region := fs.String("region", "", "区域：cn 或 intl")
	authDir := fs.String("auth-dir", defaultAuthDir, "凭证目录（会话文件存在其同级 sessions/ 下）")
	if err := fs.Parse(args); err != nil {
		return 2
	}

	r := Region(*region)
	if r == RegionUnknown {
		// 不默认成国服：两区授权页不同，猜错会让用户打开错误区域的页面
		fmt.Fprintln(os.Stderr, "必须指定区域（--region cn 或 --region intl）")
		return 2
	}

	s, err := LoginStart(r)
	if err != nil {
		fmt.Fprintf(os.Stderr, "%v\n", err)
		return 1
	}
	if err := saveSession(*authDir, s); err != nil {
		fmt.Fprintf(os.Stderr, "保存登录会话失败: %v\n", err)
		return 1
	}

	// sessionId 用 nonce（唯一且已在会话文件里）
	writeJSON(map[string]any{
		"authUrl":   s.AuthURL,
		"sessionId": s.Nonce,
		"region":    string(r),
	})
	return 0
}

func runLoginPoll(args []string, defaultAuthDir string) int {
	fs := flag.NewFlagSet("qoder-login poll", flag.ContinueOnError)
	sessionID := fs.String("session", "", "会话标识（url 子命令返回的 sessionId）")
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
	cr, err := LoginPoll(context.Background(), c.HTTP, s, *authDir)
	if err != nil {
		// 未授权：这是**正常中间态**，不是错误 —— 宿主据此显示"请继续在浏览器完成授权"
		if _, ok := err.(LoginPending); ok {
			writeJSON(map[string]any{"status": "pending"})
			return 0
		}
		// 其它错误：授权已被上游拒绝（如用户点了取消），清掉会话
		_ = os.Remove(sessionFile(*authDir, *sessionID))
		fmt.Fprintf(os.Stderr, "%v\n", err)
		return 1
	}

	// 成功：清掉会话文件（用完即删，避免残留）
	_ = os.Remove(sessionFile(*authDir, *sessionID))

	// 补齐昵称（尽力而为：拿不到不影响登录成功）
	nickname := ""
	if info, err := c.FetchUserInfo(context.Background(), cr); err == nil {
		nickname = firstNonEmpty(info.Name, info.Username)
		if nickname != "" && cr.Nickname != nickname {
			cr.Nickname = nickname
			_ = cr.SaveAtomic()
		}
	}

	writeJSON(map[string]any{
		"status":   "ok",
		"uid":      cr.UID,
		"nickname": nickname,
		"region":   string(cr.Region),
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
