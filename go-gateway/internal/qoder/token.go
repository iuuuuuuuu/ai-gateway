package qoder

// token.go 令牌刷新与 OAuth 设备流登录。
//
// 两条凭证来源（所有者要求「两个方式都要支持」）：
//
//	方式 A 软件内登录：OAuth PKCE 设备流 —— 发起（拿授权链接）→ 用户在浏览器确认
//	                   → 轮询换令牌。见 LoginStart / LoginPoll。
//	方式 B 导入已有凭证：直接读用户自备的 auths 文件。见 LoadDir / LoadFile。
//
// 两者最终写入**同一套账号存储**，登录后行为完全一致。
//
// 令牌生命周期（参考实现口径，已在本机真实凭证上验证）：
//	DT  约 30 天，临近过期（< 2 小时）时用 DRT 换新
//	DRT 约 1 年，**每次刷新都会轮换**，必须把新的写回磁盘

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"time"
)

// refreshSkew 提前刷新窗口：DT 剩余不足这个时长就刷新。
//
// 取 2 小时：上游签发的是 30 天量级的令牌，2 小时的余量足够覆盖一次网络重试，
// 又不会因为窗口过大而频繁刷新（每次刷新都会轮换 DRT，过于频繁没有好处）。
const refreshSkew = 2 * time.Hour

// NeedsRefresh 报告 DT 是否需要在 within 内刷新。
//
// 到期时间未知（0）时**返回 true** —— 宁可按需刷新一次，也不要拿着一个
// 可能已过期的令牌去打上游（那样失败信息会指向"登录失效"，误导排查）。
func (c *Cred) NeedsRefresh(within time.Duration) bool {
	if c.DT == "" {
		return true
	}
	if c.DTExpiresAt <= 0 {
		return true
	}
	return time.Now().Add(within).Unix() >= c.DTExpiresAt
}

// refreshResponse 刷新接口的返回体。
type refreshResponse struct {
	Token        string `json:"token"`
	DeviceToken  string `json:"device_token"`
	RefreshToken string `json:"refresh_token"`
	ExpiresIn    int64  `json:"expires_in"` // 毫秒（参考实现注释如此，实测按毫秒处理）
	ExpiresAt    string `json:"expires_at"`
}

// RefreshToken 用 DRT 换新 DT，并**原子写回磁盘**。
//
// 必须写回的原因：DRT 每次刷新都会轮换。只更新内存不落盘的话，
// 下次启动读到的还是旧 DRT —— 而旧 DRT 在轮换后即失效，
// 表现为「重启后账号就登录失效了」，极难排查。
func (c *Cred) RefreshToken(ctx context.Context, hc *http.Client) error {
	c.Lock()
	defer c.Unlock()

	if c.DRT == "" {
		return &AuthInvalidError{Msg: "没有 refreshToken，无法刷新（需要重新登录）"}
	}

	base := c.Region.OpenAPI()
	// 未知区域时按国服试（endpointsOf 的默认行为），但错误信息里要说明，
	// 否则用户会以为自己的国际版账号被当成国服处理了。
	endpoint := base + "/api/v1/deviceToken/refresh"

	body, _ := json.Marshal(map[string]string{"refresh_token": c.DRT})
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, endpoint, bytes.NewReader(body))
	if err != nil {
		return err
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Accept", "application/json")
	req.Header.Set("User-Agent", "Go-http-client/2.0")

	resp, err := hc.Do(req)
	if err != nil {
		return fmt.Errorf("刷新令牌请求失败: %w", err)
	}
	defer resp.Body.Close()

	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if resp.StatusCode == http.StatusUnauthorized || resp.StatusCode == http.StatusForbidden {
		return &AuthInvalidError{Msg: fmt.Sprintf("上游拒绝刷新（HTTP %d）：%s", resp.StatusCode, truncate(string(raw), 200))}
	}
	if resp.StatusCode >= 400 {
		return fmt.Errorf("刷新令牌失败（HTTP %d）：%s", resp.StatusCode, truncate(string(raw), 200))
	}

	var out refreshResponse
	if err := json.Unmarshal(raw, &out); err != nil {
		return fmt.Errorf("刷新响应解析失败: %w（原始内容：%s）", err, truncate(string(raw), 160))
	}

	newDT := firstNonEmpty(out.Token, out.DeviceToken)
	if newDT == "" {
		return fmt.Errorf("刷新响应里没有 token 字段（原始内容：%s）", truncate(string(raw), 160))
	}

	c.DT = newDT
	// DRT 轮换：上游给了新的就用新的；没给则保留旧的（有些实现不轮换）。
	if out.RefreshToken != "" {
		c.DRT = out.RefreshToken
	}
	// 到期时间：优先用 expires_in（毫秒），其次 expires_at（RFC3339）。
	if out.ExpiresIn > 0 {
		c.DTExpiresAt = time.Now().Add(time.Duration(out.ExpiresIn) * time.Millisecond).Unix()
	} else if out.ExpiresAt != "" {
		if t, err := time.Parse(time.RFC3339, out.ExpiresAt); err == nil {
			c.DTExpiresAt = t.Unix()
		}
	}

	return c.SaveAtomic()
}

// ---------------------------------------------------------------------------
// 方式 A：OAuth PKCE 设备流登录
// ---------------------------------------------------------------------------

// oauthClientID Qoder 官方客户端的 client_id（**从官方 app.asar 源码读出**）。
//
// # ⚠⚠ 这里此前是错的，导致国际版登录失败（2026-09-22 修）
//
// 旧值 `1c5e33e1-364d-4ce6-b02c-acaa81274a5c` 的注释写着
// 「实测同一个 client_id 在国服与国际版都被接受（两区授权页均 302）」——
// **那个实测方法本身是错的**：`/device/selectAccounts` 对**任何**
// client_id 都回 302 进登录页，**不校验**。真正的校验发生在
// 用户在授权页点「确认」那一刻，而那时才返回「参数无效」。
//
// 于是这条"实测"把两个区都判成了通过，掩盖了真实差异。
//
// # 真正的值（来源可靠：官方客户端自己的代码）
//
// 从**两个**官方客户端的 `resources/app.asar` 里读出，**完全一致**：
//
//	%LOCALAPPDATA%\Programs\Qoder\resources\app.asar      （国际版）
//	%LOCALAPPDATA%\Programs\Qoder CN\resources\app.asar   （国服）
//	    authClientIds: { prod: "732aef47-…", test: "732aef47-…" }
//
// 取证方式：在该 asar 里搜 `authClientIds`。**两个客户端用同一个
// client_id**，所以这里不需要按区域分。
//
// ⚠ 这个值可以用环境变量 `QODER_AUTH_CLIENT_ID` 覆盖（官方客户端
// 就是这么写的，见其 `Kje()` 函数）—— 保留同样的能力，便于将来上游改值
// 时不必重新打包。见 `clientID()`。
const oauthClientIDDefault = "732aef47-9cf2-46a2-95fe-4cebb5d0d1fa"

// oauthEnvClientID 允许用环境变量覆盖 client_id（对齐官方客户端行为）。
const oauthEnvClientID = "QODER_AUTH_CLIENT_ID"

// clientID 返回本次登录该用的 client_id。
//
// 优先环境变量（便于上游改值时热修），否则用从官方客户端读出的默认值。
func clientID() string {
	if v := strings.TrimSpace(os.Getenv(oauthEnvClientID)); v != "" {
		return v
	}
	return oauthClientIDDefault
}

// oauthRedirectURIDefault 授权回调 scheme。
//
// # ⚠ 同样是按官方源码改正的（2026-09-22）
//
// 官方两个客户端**不同**：
//
//	国际版 app.asar: authRedirectUris: { stable: "qoder-app://", canary: "qoder-canary://" }
//	国服   app.asar: authRedirectUris: { stable: null,           canary: null }
//
// 即**国服根本不传 redirect_uri**，国际版传 `qoder-app://`。
//
// 旧代码两区都用 `qoder-work-cn://` —— 那是个**从未注册过**的协议
//（实测本机 HKCR 只有 `qoder` 与 `qoder-cn`）。
//
// ⚠ 注意：设备流的 token 是靠 **poll** 拿的，浏览器回调失败**不影响**
// 登录结果（所有者实测：回调没打开客户端，登录照样成功）。
// 但 `redirect_uri` 仍要跟官方一致 —— 上游在**授权提交**时校验它，
// 传一个未注册的 scheme 正是"参数无效"的来源之一。
const oauthRedirectURIStable = "qoder-app://"

// oauthEnvRedirectURI 允许用环境变量覆盖 redirect_uri。
const oauthEnvRedirectURI = "QODER_AUTH_REDIRECT_URI"

// redirectURIFor 返回该区域该用的 redirect_uri；空串 = **不传该参数**。
//
// 国服返回空（官方就是 null），国际版返回 `qoder-app://`。
func redirectURIFor(r Region) string {
	if v := strings.TrimSpace(os.Getenv(oauthEnvRedirectURI)); v != "" {
		return v
	}
	if r == RegionIntl {
		return oauthRedirectURIStable
	}
	// 国服：官方不传。传了反而可能被判参数不符。
	return ""
}

// LoginSession 一次登录过程的状态（发起与轮询之间传递）。
type LoginSession struct {
	// Verifier PKCE 校验串（轮询时提交）。
	Verifier string `json:"verifier"`
	// Nonce 本次登录的唯一标识。
	Nonce string `json:"nonce"`
	// MachineID 本次登录生成的机器指纹（登录成功后要写进凭证）。
	MachineID string `json:"machine_id"`
	// AuthURL 用户在浏览器打开的授权链接。
	AuthURL string `json:"auth_url"`
	// Region 本次登录的区域。
	Region Region `json:"region"`
}

// LoginStart 发起登录：生成 PKCE 与机器指纹，返回授权链接。
//
// 不落盘、不发网络请求 —— 纯计算。调用方把 AuthURL 展示给用户（或直接打开浏览器）。
func LoginStart(region Region) (*LoginSession, error) {
	if region == RegionUnknown {
		return nil, fmt.Errorf("未指定区域：请选择国服或国际版（两区的授权页不同）")
	}
	verifier, challenge := makePKCE()
	nonce := NewUUID()
	machineID := NewUUID()

	// 参数顺序与官方 `zje()` 一致：
	//	challenge, challenge_method, nonce, machine_id, client_id[, redirect_uri]
	//
	// ⚠ `redirect_uri` **只在非空时才拼**（官方是
	// `...e.redirectUri ? { redirect_uri: e.redirectUri } : {}`）——
	// 国服传一个空参数与"不传"在上游看来可能不同，按官方来。
	q := url.Values{}
	q.Set("challenge", challenge)
	q.Set("challenge_method", "S256")
	q.Set("nonce", nonce)
	q.Set("machine_id", machineID)
	q.Set("client_id", clientID())
	if ru := redirectURIFor(region); ru != "" {
		q.Set("redirect_uri", ru)
	}
	authURL := region.Website() + "/device/selectAccounts?" + q.Encode()
	return &LoginSession{
		Verifier:  verifier,
		Nonce:     nonce,
		MachineID: machineID,
		AuthURL:   authURL,
		Region:    region,
	}, nil
}

// LoginPending 表示授权尚未完成（用户还没在浏览器点确认）。
//
// 单独成型是为了让调用方能把「继续等」与「真失败」区分开 ——
// 二者都返回 error 的话，界面只能笼统报错，用户不知道是该再等等还是重新登录。
type LoginPending struct{}

func (LoginPending) Error() string { return "授权尚未完成，请在浏览器中完成授权后重试" }

// LoginPoll 轮询一次授权结果。
//
// 返回：
//
//	(*Cred, nil)          授权成功，凭证已填充（机器指纹来自 session）
//	(nil, LoginPending{}) 用户还没确认 —— 调用方应稍后重试
//	(nil, err)            真失败
//
// 生成的文件名是 qoder-<uid>.json（与 LoadDir 的 glob 一致）。
func LoginPoll(ctx context.Context, hc *http.Client, s *LoginSession, authDir string) (*Cred, error) {
	if s == nil {
		return nil, fmt.Errorf("登录会话为空")
	}
	endpoint := fmt.Sprintf(
		"%s/api/v1/deviceToken/poll?nonce=%s&verifier=%s&challenge_method=S256",
		s.Region.OpenAPI(), url.QueryEscape(s.Nonce), url.QueryEscape(s.Verifier),
	)

	req, err := http.NewRequestWithContext(ctx, http.MethodGet, endpoint, nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("User-Agent", "Go-http-client/2.0")
	req.Header.Set("Accept", "application/json")

	resp, err := hc.Do(req)
	if err != nil {
		return nil, fmt.Errorf("轮询授权结果失败: %w", err)
	}
	defer resp.Body.Close()

	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))

	// 404 / 202 = 尚未授权（上游用这两种状态表示"还没好"）。
	if resp.StatusCode == http.StatusNotFound || resp.StatusCode == http.StatusAccepted {
		return nil, LoginPending{}
	}
	if resp.StatusCode >= 400 {
		return nil, fmt.Errorf("轮询授权失败（HTTP %d）：%s", resp.StatusCode, truncate(string(raw), 200))
	}

	var tok struct {
		Token        string `json:"token"`
		DeviceToken  string `json:"device_token"`
		RefreshToken string `json:"refresh_token"`
		UserID       string `json:"user_id"`
		ExpiresIn    int64  `json:"expires_in"`
	}
	if err := json.Unmarshal(raw, &tok); err != nil {
		return nil, fmt.Errorf("授权响应解析失败: %w（原始内容：%s）", err, truncate(string(raw), 160))
	}

	dt := firstNonEmpty(tok.Token, tok.DeviceToken)
	if dt == "" {
		// 200 但没有 token：仍视为未完成（上游偶尔这么回）。
		return nil, LoginPending{}
	}
	if tok.UserID == "" {
		return nil, fmt.Errorf("授权成功但响应里没有 user_id，无法确定账号标识（原始内容：%s）", truncate(string(raw), 160))
	}

	c := &Cred{
		UID:          tok.UserID,
		DT:           dt,
		DRT:          tok.RefreshToken,
		MachineID:    s.MachineID,
		MachineToken: hexShort(32),
		MachineType:  "5",
		Region:       s.Region,
	}
	if tok.ExpiresIn > 0 {
		c.DTExpiresAt = time.Now().Add(time.Duration(tok.ExpiresIn) * time.Millisecond).Unix()
	}

	// 落盘：文件名与 LoadDir 的 glob 对齐。
	if authDir != "" {
		if err := os.MkdirAll(authDir, 0o700); err != nil {
			return nil, fmt.Errorf("创建凭证目录失败: %w", err)
		}
		c.FilePath = filepath.Join(authDir, "qoder-"+sanitizeUID(c.UID)+".json")
		if err := c.SaveAtomic(); err != nil {
			return nil, fmt.Errorf("保存凭证失败: %w", err)
		}
	}
	return c, nil
}

// makePKCE 生成 PKCE 校验串与挑战。
//
// ⚠ 与参考实现**逐字一致**（含按字节取模 66 的采样方式）。
// 看起来"可以优化"成 crypto/rand 直接取模，但那是不同的分布 ——
// 保持与已验证可用的实现一致，不要顺手改。
func makePKCE() (verifier, challenge string) {
	const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~"
	buf := make([]byte, 64)
	_, _ = randRead(buf)
	var sb strings.Builder
	sb.Grow(64)
	for _, b := range buf {
		sb.WriteByte(alphabet[int(b)%len(alphabet)])
	}
	verifier = sb.String()
	challenge = base64RawURL(sha256Sum([]byte(verifier)))
	return verifier, challenge
}

// sanitizeUID 把 uid 变成安全的文件名片段（防路径穿越）。
func sanitizeUID(uid string) string {
	r := strings.NewReplacer("/", "_", "\\", "_", "..", "_", ":", "_", "*", "_", "?", "_", "\"", "_", "<", "_", ">", "_", "|", "_")
	out := r.Replace(strings.TrimSpace(uid))
	if out == "" {
		out = "unknown"
	}
	return out
}

func truncate(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n] + "…"
}
