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
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"time"

	"workbuddy2api/internal/qoderclient"
)

// parseRFC3339 解析客户端给的到期时刻（如 `2026-10-18T22:53:53Z`）。
//
// 解不开时返回 0（未知）—— 上层会把"未知"当"需要刷新"，
// 那是安全的（宁可多刷一次，也不要用过期令牌去打上游）。
func parseRFC3339(s string) int64 {
	s = strings.TrimSpace(s)
	if s == "" {
		return 0
	}
	for _, layout := range []string{time.RFC3339, "2006-01-02T15:04:05Z", "2006-01-02T15:04:05.000Z"} {
		if t, err := time.Parse(layout, s); err == nil {
			return t.Unix()
		}
	}
	return 0
}

// deterministicUUID 由字符串派生一个**稳定的** UUID 形态标识。
//
// 用途：客户端没落盘 machine-id 时给 COSY 签名一个稳定指纹。
// **稳定**是关键 —— 每次随机的话，上游会看到"同一账号来自大量不同设备"。
func deterministicUUID(seed string) string {
	sum := sha256.Sum256([]byte("qoder-machine:" + seed))
	h := hex.EncodeToString(sum[:16])
	// 摆成 8-4-4-4-12，并把版本位设成 4
	return h[0:8] + "-" + h[8:12] + "-4" + h[13:16] + "-" + h[16:20] + "-" + h[20:32]
}

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
		// ⚠ 帮助文案必须与**实际派发的子命令**一致。
		//
		// 旧文案只列了 url|poll|import-client，而 switch 里还有
		// quota / models / campaigns / claim-campaign / job-token。
		// 于是排查时看到这条 usage 会误判「新子命令没注册」——
		// 我自己就被它误导过，白查了一轮二进制版本问题。
		// 漏列的子命令补齐，别让帮助文案成为误导源。
		fmt.Fprintln(os.Stderr, "用法: qoder-login <url|poll|import-client|quota|models|campaigns|claim-campaign|job-token> [选项]")
		return 2
	}
	sub := args[0]

	switch sub {
	case "url":
		return runLoginURL(args[1:], defaultAuthDir)
	case "poll":
		return runLoginPoll(args[1:], defaultAuthDir)
	case "import-client":
		// **主路径**：直接读 Qoder 客户端已登录的凭证。
		//
		// ## 为什么这才是主路径（而不是上面的浏览器授权）
		//
		// 授权链接的 `redirect_uri` 是 `qoder-work-cn://` —— 一个**自定义
		// 协议**，只有真正的 Qoder 客户端才会注册它。我们不是它，浏览器
		// 授权完成后**无处回调**，所以那条路在本机走不通。
		//
		// 而客户端已经登录了，登录态就在它的数据目录里（Electron
		// safeStorage 加密）。见 internal/qoderclient 的包注释。
		return runImportClient(args[1:], defaultAuthDir)
	case "quota":
		// 查额度并输出 JSON。
		//
		// `Client.FetchQuota` 早已实现，但**从来没有生产者调用它** ——
		// 于是界面上额度恒为 0。这个子命令补上那个缺口。
		return runQuota(args[1:], defaultAuthDir)
	case "models":
		// 查该账号**实际可用**的模型清单（按账号，不是全局）。
		return runModels(args[1:], defaultAuthDir)
	case "campaigns":
		// 查权益活动（「每天领 100 Credits」那类）。
		//
		// ⚠ 只查询。领取见 `claim-campaign`。
		return runCampaigns(args[1:], defaultAuthDir)
	case "claim-campaign":
		// 领取一个权益活动。
		//
		// ⚠ 这是**写操作**，只能由用户在界面上显式点击触发。
		// 不写定时任务替用户自动领 —— 那与"用户点一下"不是一回事，
		// 且会让账号表现出非人类的活动模式。见 campaign.go 的说明。
		return runClaimCampaign(args[1:], defaultAuthDir)
	case "job-token":
		// 换取短期作业令牌（jt-）。用于诊断与验证。
		return runJobToken(args[1:], defaultAuthDir)
	default:
		fmt.Fprintf(os.Stderr, "未知子命令 %q（应为 url / poll / import-client）\n", sub)
		return 2
	}
}

// runQuota 查一个账号的额度并输出 JSON。
//
// 用法：`qoder-login quota --uid <uid> --auth-dir <dir>`
//
// 输出：`{"status":"ok","uid":...,"remaining":N,"total":N,"planTierName":"...","exceeded":false}`
// 失败：`{"status":"error","uid":...,"message":"..."}`（退出码仍为 0）
//
// ## 为什么失败也返回 0
//
// "查不到额度"不是命令执行失败 —— 宿主把它当成一次正常的"没数据"，
// 而不是崩溃。若用非零退出码，宿主会当成子进程故障并丢掉 message，
// 用户看到的就是没有原因的空白。
func runQuota(args []string, defaultAuthDir string) int {
	fs := flag.NewFlagSet("qoder-login quota", flag.ContinueOnError)
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
	q, err := cli.FetchQuota(context.Background(), c)
	if err != nil {
		writeJSON(map[string]any{"status": "error", "uid": c.UID, "message": err.Error()})
		return 0
	}

	writeJSON(map[string]any{
		"status":       "ok",
		"uid":          c.UID,
		"remaining":    q.Remaining,
		"total":        q.Total,
		"used":         q.Used,
		"exceeded":     q.Exceeded,
		"planTierName": q.PlanTierName,
		"usagePercent": q.UsagePercent,
		// 到期时刻（Unix 秒）；0 = 未知或永不过期
		"expiresAt": q.ExpiresAt,
	})
	return 0
}

// runModels 查一个账号实际可用的模型并输出 JSON。
//
// 用法：`qoder-login models --uid <uid> --auth-dir <dir>`
//
// ⚠ 是**按账号**查（走该账号的凭证）：Qoder 不同区域的可用模型不同，
// 全局列表会误导用户以为自己能用某个模型。
func runModels(args []string, defaultAuthDir string) int {
	fs := flag.NewFlagSet("qoder-login models", flag.ContinueOnError)
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
			"key":            m.Key,
			"displayName":    m.DisplayName,
			"enable":         m.Enable,
			"isReasoning":    m.IsReasoning,
			"isVL":           m.IsVL,
			"maxInputTokens": m.MaxInputTokens,
			"priceFactor":    m.PriceFactor,
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

// runCampaigns 查一个账号的权益活动并输出 JSON（**只读**）。
//
// 用法：`qoder-login campaigns --uid <uid> --auth-dir <dir>`
//
// 输出：`{"status":"ok","uid":...,"showCampaign":true,"claimable":true,
//        "campaignUrl":"...","campaigns":[{...}]}`
//
// ⚠ 只查询、**不领取**。领取要阿里云验证码，见 campaign.go 的边界说明。
func runCampaigns(args []string, defaultAuthDir string) int {
	fs := flag.NewFlagSet("qoder-login campaigns", flag.ContinueOnError)
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
	st, err := cli.FetchCampaigns(context.Background(), c)
	if err != nil {
		writeJSON(map[string]any{"status": "error", "uid": c.UID, "message": err.Error()})
		return 0
	}

	writeJSON(map[string]any{
		"status":       "ok",
		"uid":          c.UID,
		"showCampaign": st.ShowCampaign,
		"claimable":    st.Claimable,
		"campaignUrl":  st.CampaignURL,
		"campaigns":    st.Campaigns,
		"count":        len(st.Campaigns),
	})
	return 0
}

// runClaimCampaign 领取一个权益活动。
//
// 用法：`qoder-login claim-campaign --uid <uid> --campaign-id <id> --auth-dir <dir>`
//
// 输出：`{"status":"ok","uid":...,"grantId":...,"claimed":true,"replayed":false}`
//
// ⚠ `replayed:true` 表示这次是**重放**（之前已领过）——
// 界面必须区分，否则用户重复点会以为又领了一份。
func runClaimCampaign(args []string, defaultAuthDir string) int {
	fs := flag.NewFlagSet("qoder-login claim-campaign", flag.ContinueOnError)
	uid := fs.String("uid", "", "账号 uid")
	campaignID := fs.String("campaign-id", "", "活动 ID（campaignId，不是 campaignKey）")
	authDir := fs.String("auth-dir", defaultAuthDir, "凭证目录")
	if err := fs.Parse(args); err != nil {
		return 2
	}
	if strings.TrimSpace(*uid) == "" {
		fmt.Fprintln(os.Stderr, "缺少 --uid 参数")
		return 2
	}
	if strings.TrimSpace(*campaignID) == "" {
		fmt.Fprintln(os.Stderr, "缺少 --campaign-id 参数")
		return 2
	}

	c, err := loadCredByUID(*authDir, *uid)
	if err != nil {
		writeJSON(map[string]any{"status": "error", "uid": *uid, "message": err.Error()})
		return 0
	}

	cli := New()
	r, err := cli.ClaimCampaign(context.Background(), c, *campaignID)
	if err != nil {
		writeJSON(map[string]any{"status": "error", "uid": c.UID, "message": err.Error()})
		return 0
	}

	writeJSON(map[string]any{
		"status":      "ok",
		"uid":         c.UID,
		"grantId":     r.GrantID,
		"campaignId":  r.CampaignID,
		"campaignKey": r.CampaignKey,
		"claimed":     true,
		// 重放 = 之前已领过。界面据此说"已领过"而不是"领取成功"。
		"replayed":  r.Replayed,
		"claimedAt": r.ClaimedAt,
		"grantedAt": r.GrantedAt,
	})
	return 0
}

// runJobToken 换取短期作业令牌并输出 JSON。
//
// 用法：`qoder-login job-token --uid <uid> --auth-dir <dir>`
//
// ⚠ 输出里**不含**令牌本体（那是秘密）—— 只报长度与到期时间，
// 便于确认"能不能换到"而不把凭据泄到日志/终端历史里。
func runJobToken(args []string, defaultAuthDir string) int {
	fs := flag.NewFlagSet("qoder-login job-token", flag.ContinueOnError)
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
	t, err := cli.FetchJobToken(context.Background(), c)
	if err != nil {
		writeJSON(map[string]any{"status": "error", "uid": c.UID, "message": err.Error()})
		return 0
	}

	writeJSON(map[string]any{
		"status": "ok",
		"uid":    c.UID,
		// 只报长度，不报令牌本体
		"tokenLength":        len(t.Token),
		"refreshTokenLength": len(t.RefreshToken),
		"expiresAt":          t.ExpiresAt,
		// 实测是**毫秒**（86400000 = 1 天）；同时给出换算后的小时数便于人读
		"expiresInMs":     t.ExpiresIn,
		"expiresInHours":  float64(t.ExpiresIn) / 3600000,
		"refreshExpiresAt": t.RefreshTokenExpiresAt,
	})
	return 0
}

// loadCredByUID 在凭证目录里按 uid 找凭证。
//
// 文件名规则见 `DefaultAuthDir` 的说明（`qoder-<uid>.json`）。
// 找不到时回退到逐个加载比对 —— 用户手工放的文件名可能不同。
func loadCredByUID(dir, uid string) (*Cred, error) {
	cands := []string{
		filepath.Join(dir, "qoder-"+sanitizeUID(uid)+".json"),
		filepath.Join(dir, sanitizeUID(uid)+".json"),
	}
	for _, p := range cands {
		if _, err := os.Stat(p); err == nil {
			return LoadFile(p)
		}
	}
	creds, _, _ := LoadDir(dir)
	for _, c := range creds {
		if c.UID == uid {
			return c, nil
		}
	}
	return nil, fmt.Errorf("在 %s 里找不到 uid=%s 的凭证", dir, uid)
}

// runImportClient 读客户端登录态并落成我们的凭证文件。
//
// 这是 Qoder 的**主路径**：用户只要在客户端里登录过，就不需要再做任何事。
func runImportClient(args []string, defaultAuthDir string) int {
	fs := flag.NewFlagSet("qoder-login import-client", flag.ContinueOnError)
	authDir := fs.String("auth-dir", defaultAuthDir, "凭证目录")
	// 客户端数据目录（默认自动探测；显式传入便于测试与多版本共存）
	clientDir := fs.String("client-dir", "", "客户端数据目录（默认自动探测）")
	nickname := fs.String("nickname", "", "昵称（默认用客户端里的账号名）")
	if err := fs.Parse(args); err != nil {
		return 2
	}

	dir := strings.TrimSpace(*clientDir)
	if dir == "" {
		found, ok := qoderclient.FindClientAppDir()
		if !ok {
			fmt.Fprintln(os.Stderr,
				"找不到 Qoder 客户端的登录数据。请先在 Qoder 客户端里登录一次"+
					"（会自动探测 %APPDATA%\\com.qodercn.app.stable 等目录）。")
			return 1
		}
		dir = found
	}

	auth, err := qoderclient.ReadClientAuthFrom(dir)
	if err != nil {
		fmt.Fprintf(os.Stderr, "读取 Qoder 客户端凭证失败: %v\n", err)
		return 1
	}

	r := Region(qoderclient.Region(dir))
	if r == RegionUnknown {
		r = RegionCN
	}

	c := &Cred{
		UID:          auth.User.ID,
		Nickname:     firstNonEmpty(strings.TrimSpace(*nickname), auth.User.Name),
		DT:           auth.Token,
		DRT:          auth.RefreshToken,
		DTExpiresAt:  parseRFC3339(auth.ExpiresAt),
		MachineID:    qoderclient.MachineID(dir),
		MachineToken: "", // 客户端不落盘 machineToken，首次请求时上游会下发
		MachineType:  "windows",
		Region:       r,
	}
	if c.UID == "" {
		fmt.Fprintln(os.Stderr, "客户端凭证里没有 user.id，无法确定账号标识")
		return 1
	}
	if c.MachineID == "" {
		// COSY 签名需要机器指纹。客户端没落盘时造一个稳定的
		//（由 uid 派生，保证同一账号每次相同）。
		c.MachineID = deterministicUUID(c.UID)
	}

	if err := os.MkdirAll(*authDir, 0o700); err != nil {
		fmt.Fprintf(os.Stderr, "创建凭证目录失败: %v\n", err)
		return 1
	}
	c.FilePath = filepath.Join(*authDir, "qoder-"+sanitizeUID(c.UID)+".json")
	if err := c.SaveAtomic(); err != nil {
		fmt.Fprintf(os.Stderr, "保存凭证失败: %v\n", err)
		return 1
	}

	writeJSON(map[string]any{
		"status":    "ok",
		"uid":       c.UID,
		"nickname":  c.Nickname,
		"region":    string(c.Region),
		"file":      filepath.Base(c.FilePath),
		"clientDir": dir,
		"expiresAt": auth.ExpiresAt,
		// 头像：使用者的反馈是"已授权后不显示头像"，
		// 而客户端登录态里本来就有 `user.avatarUrl`。
		"avatarUrl": auth.User.AvatarURL,
	})
	return 0
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
