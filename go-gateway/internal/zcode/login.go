package zcode

// login.go OAuth 设备流登录 + 凭证兑换。
//
// ## 两条路（都要支持）
//
//	方式 A：OAuth 设备流（本文件）
//	  发起 → 用户在浏览器授权 → 轮询取令牌 → 兑换成最终凭证
//	方式 B：导入已有凭证（cred.go 的 Parse）
//	  用户从控制台复制 `{apiKey}.{secret}` 直接粘贴
//
// **ZCode 与 Qoder 的差异**：Qoder 的凭证只能靠 OAuth 拿到（用户无法手抄），
// 而 ZCode 的凭证是**用户可复制的字符串** —— 故"导入"是主路径，
// OAuth 是备选。界面应把"粘贴凭证"放在显眼位置。
//
// ## 流程（参考实现 oauth.ts + resolver.ts，已逐个字段核对）
//
//	① POST zcode.z.ai/api/v1/oauth/cli/init   {provider}
//	     → flow_id / authorize_url / expires_at / poll_interval_sec
//	② 用户在浏览器打开 authorize_url 完成授权
//	③ GET  zcode.z.ai/api/v1/oauth/cli/poll/{flow_id}
//	     → status: pending / ready / failed
//	       ready 时给 data.token（JWT）与 data.{provider}.access_token
//	④ 兑换（**两服务商不同**）：
//	     zai:      POST api.z.ai/api/auth/z/login → bizToken
//	               → getCustomerInfo → api_keys → copy → secretKey
//	               → 最终凭证 {apiKey}.{secretKey}
//	     bigmodel: **没有 z/login 这一步**，直接用 accessToken 当 biz token
//	               → getCustomerInfo → api_keys → copy → secretKey（可选）
//	               → 最终凭证 {apiKey}[.{secretKey}]

import (
	"bytes"
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"
	"time"
)

// ZCode 的 OAuth 端点（与 provider 无关，都打 zcode.z.ai）。
const (
	// OAuthInitPath 发起登录。
	OAuthInitPath = "/api/v1/oauth/cli/init"
	// OAuthPollPath 轮询（后接 flow_id）。
	OAuthPollPath = "/api/v1/oauth/cli/poll/"

	// ZaiLoginPath zai 的业务令牌兑换。
	ZaiLoginPath = "/api/auth/z/login"

	// CustomerInfoPath 查机构与项目。
	CustomerInfoPath = "/api/biz/customer/getCustomerInfo"
	// APIKeysPathFmt 密钥列表/创建（%s = orgId, %s = projectId）。
	APIKeysPathFmt = "/api/biz/v1/organization/%s/projects/%s/api_keys"
	// APIKeyCopyPathFmt 取 secretKey（%s/%s = org/proj，%s = apiKey）。
	APIKeyCopyPathFmt = "/api/biz/v1/organization/%s/projects/%s/api_keys/copy/%s"

	// APIKeyName 我们创建的密钥名（参考实现用同名，便于复用已存在的）。
	//
	// 为什么固定名字：这样重复登录会**复用**同一个密钥，而不是每次都新建 ——
	// 否则用户控制台里会堆积一堆 `zcode-api-key`。
	APIKeyName = "zcode-api-key"
)

// defaultOrgMarker / defaultProjectMarker 优先选择的机构/项目名。
//
// 上游账号可能属于多个机构。参考实现优先选名字含"默认机构"的，
// 因为编码套餐通常挂在那里。找不到则取第一个。
const (
	defaultOrgMarker     = "默认机构"
	defaultProjectMarker = "默认项目"
)

// LoginSession 一次登录过程的状态（发起与轮询之间传递）。
//
// 必须能被**序列化**：发起与轮询是两个独立的进程/请求
//（宿主调子命令，或前端轮询 HTTP 接口），会话要跨调用传递。
type LoginSession struct {
	// Provider 服务商。
	Provider Provider `json:"provider"`
	// FlowID 上游给的流程标识（轮询路径参数）。
	FlowID string `json:"flow_id"`
	// PollToken 客户端生成的 Bearer（**轮询时用自己生成的那个**，
	// 不是上游返回的 poll_token —— 参考实现明确写了这点）。
	PollToken string `json:"poll_token"`
	// AuthorizeURL 用户在浏览器打开的链接。
	AuthorizeURL string `json:"authorize_url"`
	// ExpiresAt 会话过期时刻（Unix 秒）。
	ExpiresAt int64 `json:"expires_at"`
	// PollIntervalSec 轮询间隔（秒）。
	PollIntervalSec int `json:"poll_interval_sec"`
	// CreatedAt 本地创建时刻（Unix 秒），用于清理过期会话文件。
	CreatedAt int64 `json:"created_at"`
}

// PollInterval 轮询间隔（带下限 1 秒）。
//
// 上游可能返回 0 或很小的值；没有下限会导致高频轮询（打上游）。
func (s *LoginSession) PollInterval() time.Duration {
	sec := s.PollIntervalSec
	if sec < 1 {
		sec = 1
	}
	return time.Duration(sec) * time.Second
}

// LoginPending 表示授权尚未完成（正常中间态，**不是错误**）。
//
// 单独成型是为了让调用方能区分"继续等"与"真失败" ——
// 二者都返回 error 的话，界面只能笼统报错，用户不知道是该再等等还是重新登录。
type LoginPending struct {
	// Reason 可选的说明（如"上游 5xx，当作未完成重试"）。
	Reason string
}

func (e LoginPending) Error() string {
	if e.Reason != "" {
		return "授权尚未完成：" + e.Reason
	}
	return "授权尚未完成，请在浏览器中完成授权后重试"
}

// StartLogin 发起 OAuth 登录。
//
// 会发一次网络请求（拿 authorize_url）。
func (c *Client) StartLogin(ctx context.Context, provider Provider) (*LoginSession, error) {
	if provider == ProviderUnknown {
		return nil, fmt.Errorf("未指定服务商：请选择 Z.AI 或智谱（两者的授权页不同）")
	}

	// pollToken 是**客户端生成的 32 随机字节的 hex**（64 字符）。
	// 参考实现：`randomBytes(32).toString("hex")`。
	tok, err := randomHex(32)
	if err != nil {
		return nil, fmt.Errorf("生成轮询令牌失败: %w", err)
	}

	body, _ := json.Marshal(map[string]string{"provider": string(provider)})
	ctx, cancel := context.WithTimeout(ctx, c.timeout())
	defer cancel()

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, ConfigOrigin+OAuthInitPath, bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Authorization", "Bearer "+tok)
	for k, v := range c.Identity.Headers() {
		if v != "" {
			req.Header.Set(k, v)
		}
	}

	resp, err := c.http().Do(req)
	if err != nil {
		return nil, fmt.Errorf("发起 ZCode 登录失败: %w", err)
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if resp.StatusCode >= 400 {
		return nil, NewError(resp.StatusCode, string(raw))
	}

	// 信封：{code, msg, data}
	var doc struct {
		Code int    `json:"code"`
		Msg  string `json:"msg"`
		Data struct {
			FlowID          string `json:"flow_id"`
			AuthorizeURL    string `json:"authorize_url"`
			ExpiresAt       int64  `json:"expires_at"`
			PollIntervalSec int    `json:"poll_interval_sec"`
		} `json:"data"`
	}
	if err := json.Unmarshal(raw, &doc); err != nil {
		return nil, fmt.Errorf("登录响应解析失败: %w（原始内容：%s）", err, truncate(string(raw), 160))
	}
	if doc.Code != 0 {
		return nil, &Error{Status: resp.StatusCode, Code: fmt.Sprintf("%d", doc.Code), Msg: doc.Msg}
	}

	// 形状守卫（参考实现也做）：缺字段会让后续轮询必然失败，
	// 而错误会显示成"轮询失败"，看不出是发起时就缺了东西。
	if strings.TrimSpace(doc.Data.FlowID) == "" {
		return nil, fmt.Errorf("登录响应缺少 flow_id（原始内容：%s）", truncate(string(raw), 160))
	}
	if strings.TrimSpace(doc.Data.AuthorizeURL) == "" {
		return nil, fmt.Errorf("登录响应缺少 authorize_url（原始内容：%s）", truncate(string(raw), 160))
	}
	if doc.Data.ExpiresAt <= 0 {
		return nil, fmt.Errorf("登录响应缺少有效的 expires_at（原始内容：%s）", truncate(string(raw), 160))
	}

	return &LoginSession{
		Provider:        provider,
		FlowID:          doc.Data.FlowID,
		PollToken:       tok,
		AuthorizeURL:    doc.Data.AuthorizeURL,
		ExpiresAt:       doc.Data.ExpiresAt,
		PollIntervalSec: doc.Data.PollIntervalSec,
		CreatedAt:       time.Now().Unix(),
	}, nil
}

// PollOnce 轮询一次授权结果。
//
// 返回：
//
//	(*LoginTokens, nil)        授权成功
//	(nil, LoginPending{})      尚未完成（**正常中间态**）—— 调用方应稍后重试
//	(nil, err)                 真失败（4xx / code≠0 / status=failed）
//
// ## 错误语义（照抄参考实现，实测确认）
//
//	网络异常            → pending（重试）
//	HTTP 4xx（除 408/429）→ **致命**
//	HTTP 408 / 429 / 5xx → pending（重试）
//	非 JSON body        → pending（重试）
//	code 不是数字       → pending（重试）
//	code ≠ 0            → **致命**
//	status = ready      → 成功
//	status = failed     → **致命**
//	其它 status          → **致命**
//
// "哪些当致命、哪些当重试"是刻意的：把临时故障当致命会让用户白重登一次，
// 把真失败当重试会让用户无限等下去。
func (c *Client) PollOnce(ctx context.Context, s *LoginSession) (*LoginTokens, error) {
	if s == nil {
		return nil, fmt.Errorf("登录会话为空")
	}
	ctx, cancel := context.WithTimeout(ctx, c.timeout())
	defer cancel()

	rawURL := ConfigOrigin + OAuthPollPath + url.PathEscape(s.FlowID)
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, rawURL, nil)
	if err != nil {
		return nil, err
	}
	// 轮询用**我们自己生成**的 pollToken（不是上游返回的 poll_token）
	req.Header.Set("Authorization", "Bearer "+s.PollToken)
	req.Header.Set("Accept", "application/json")
	for k, v := range c.Identity.Headers() {
		if v != "" {
			req.Header.Set(k, v)
		}
	}

	resp, err := c.http().Do(req)
	if err != nil {
		// 网络异常 → 当作未完成重试（参考实现语义）
		return nil, LoginPending{Reason: "网络异常，将重试"}
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))

	// HTTP 层判定
	switch {
	case resp.StatusCode == 408 || resp.StatusCode == 429 || resp.StatusCode >= 500:
		// 可重试
		return nil, LoginPending{Reason: fmt.Sprintf("上游 HTTP %d，将重试", resp.StatusCode)}
	case resp.StatusCode >= 400:
		// 4xx（除 408/429）→ 致命
		return nil, fmt.Errorf("轮询授权失败（HTTP %d）：%s", resp.StatusCode, truncate(string(raw), 200))
	}

	// 解析信封
	var doc struct {
		Code any             `json:"code"`
		Msg  string          `json:"msg"`
		Data json.RawMessage `json:"data"`
	}
	if err := json.Unmarshal(raw, &doc); err != nil {
		// 非 JSON → 当作未完成重试
		return nil, LoginPending{Reason: "上游返回了非 JSON 响应，将重试"}
	}

	// code 不是数字 → 当作未完成重试
	codeNum, ok := doc.Code.(float64)
	if !ok {
		return nil, LoginPending{Reason: "上游响应缺少数字 code，将重试"}
	}
	if int(codeNum) != 0 {
		// code ≠ 0 → 致命
		return nil, &Error{Status: resp.StatusCode, Code: fmt.Sprintf("%.0f", codeNum), Msg: doc.Msg}
	}

	// 解析 data（**provider token 的 key 就是 provider 字符串本身**）
	var data struct {
		Status string `json:"status"`
		Token  string `json:"token"`
		User   *struct {
			UserID string `json:"user_id"`
		} `json:"user"`
		// 两个 provider 的 token 都可能有；用 map 读，避免硬编码两个字段
		Extra map[string]json.RawMessage `json:"-"`
	}
	if err := json.Unmarshal(doc.Data, &data); err != nil {
		return nil, LoginPending{Reason: "上游 data 结构异常，将重试"}
	}
	// 用 map 再解一次，取 data[provider].access_token
	var dataMap map[string]json.RawMessage
	_ = json.Unmarshal(doc.Data, &dataMap)

	switch data.Status {
	case "pending":
		return nil, LoginPending{}
	case "failed":
		return nil, fmt.Errorf("授权失败（上游 status=failed）。请重新发起登录")
	case "ready":
		// 继续取 token
	default:
		return nil, fmt.Errorf("上游返回了未知的授权状态 %q", data.Status)
	}

	// status=ready：必须能取到该 provider 的 access_token
	providerKey := string(s.Provider)
	rawProv, ok := dataMap[providerKey]
	if !ok {
		return nil, fmt.Errorf("授权成功但响应里没有 %q 的令牌（原始内容：%s）",
			providerKey, truncate(string(raw), 200))
	}
	var prov struct {
		AccessToken  string `json:"access_token"`
		RefreshToken string `json:"refresh_token"`
	}
	if err := json.Unmarshal(rawProv, &prov); err != nil || strings.TrimSpace(prov.AccessToken) == "" {
		return nil, fmt.Errorf("授权成功但 %q 的 access_token 为空（原始内容：%s）",
			providerKey, truncate(string(raw), 200))
	}

	tokens := &LoginTokens{
		Provider:     s.Provider,
		AccessToken:  prov.AccessToken,
		RefreshToken: prov.RefreshToken,
		JWT:          data.Token,
	}
	if data.User != nil {
		tokens.UserID = data.User.UserID
	}
	if tokens.JWT != "" {
		tokens.JWTIssuedAt = time.Now().Unix()
	}
	return tokens, nil
}

// LoginTokens OAuth 拿到的中间令牌（还需兑换成最终凭证）。
type LoginTokens struct {
	Provider     Provider
	AccessToken  string
	RefreshToken string
	// JWT ZCode plan 令牌（额度查询要用）。
	JWT string
	// JWTIssuedAt JWT 签发时刻（本地记录）。
	JWTIssuedAt int64
	UserID      string
}

// ExchangeCredential 把 OAuth 令牌兑换成最终凭证。
//
// ## 两个服务商的流程**不同**（这是最容易写错的地方）
//
//	zai:      accessToken --(POST /api/auth/z/login)--> bizToken
//	          然后用 **Bearer bizToken** 调业务接口
//	bigmodel: **没有 z/login 这一步**，直接用 accessToken 当 biz token，
//	          且认证头是**裸值**（不带 "Bearer " 前缀！）
//
// 最终凭证：
//
//	zai:      {apiKey}.{secretKey}   —— secretKey **缺失即失败**
//	bigmodel: {apiKey}[.{secretKey}] —— secretKey 拿不到则降级为单段
func (c *Client) ExchangeCredential(ctx context.Context, t *LoginTokens) (*Cred, error) {
	if t == nil || strings.TrimSpace(t.AccessToken) == "" {
		return nil, fmt.Errorf("OAuth 令牌为空，无法兑换凭证")
	}

	host := t.Provider.BizHost()
	var authHeader string

	if t.Provider == ProviderZAI {
		// ① 换业务令牌
		bizToken, err := c.zaiBizToken(ctx, t.AccessToken)
		if err != nil {
			return nil, err
		}
		authHeader = "Bearer " + bizToken
	} else {
		// bigmodel：**裸值**，不带 Bearer 前缀
		authHeader = t.AccessToken
	}

	// ② 机构与项目
	orgID, projectID, err := c.customerInfo(ctx, host, authHeader)
	if err != nil {
		return nil, err
	}

	// ③ 找或建 api_key
	apiKey, err := c.findOrCreateAPIKey(ctx, host, authHeader, orgID, projectID)
	if err != nil {
		return nil, err
	}

	// ④ 取 secretKey
	secret, err := c.copySecretKey(ctx, host, authHeader, orgID, projectID, apiKey)
	if err != nil {
		// zai 的 secretKey **必需**（参考实现 requireSecretKey=true）
		if t.Provider == ProviderZAI {
			return nil, fmt.Errorf("Z.AI 的密钥缺少 secretKey，无法完成登录（该服务商要求两段式凭证）: %w", err)
		}
		// bigmodel 可选：降级为单段
		secret = ""
	}

	credential := apiKey
	if secret != "" {
		credential = apiKey + "." + secret
	}

	// 注意：不能用 c（它是 *Client 接收者），用 cred 避免遮蔽
	cred, err := Parse(credential)
	if err != nil {
		return nil, err
	}
	cred.Provider = t.Provider
	cred.JWT = t.JWT
	cred.JWTIssuedAt = t.JWTIssuedAt
	return cred, nil
}

// zaiBizToken 用 OAuth accessToken 换 Z.AI 的业务令牌。
//
// ⚠ 这个端点**不用信封的 code===0 判定** —— 它直接读顶层字段
//（参考实现明确写了这点，实测无效 token 返回 200 + `{"code":500,...}`）。
func (c *Client) zaiBizToken(ctx context.Context, accessToken string) (string, error) {
	ctx, cancel := context.WithTimeout(ctx, c.timeout())
	defer cancel()

	body, _ := json.Marshal(map[string]string{"token": accessToken})
	req, err := http.NewRequestWithContext(ctx, http.MethodPost,
		ProviderZAI.BizHost()+ZaiLoginPath, bytes.NewReader(body))
	if err != nil {
		return "", err
	}
	req.Header.Set("Content-Type", "application/json")
	for k, v := range c.Identity.Headers() {
		if v != "" {
			req.Header.Set(k, v)
		}
	}

	resp, err := c.http().Do(req)
	if err != nil {
		return "", fmt.Errorf("兑换 Z.AI 业务令牌失败: %w", err)
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if resp.StatusCode >= 400 {
		return "", NewError(resp.StatusCode, string(raw))
	}

	// 三种形状都兼容（参考实现的 ?? 链）
	var doc struct {
		AccessToken  string `json:"access_token"`
		AccessToken2 string `json:"accessToken"`
		Data         *struct {
			AccessToken string `json:"access_token"`
		} `json:"data"`
	}
	if err := json.Unmarshal(raw, &doc); err != nil {
		return "", fmt.Errorf("业务令牌响应解析失败: %w（原始内容：%s）", err, truncate(string(raw), 160))
	}
	tok := firstNonEmpty(doc.AccessToken, doc.AccessToken2)
	if tok == "" && doc.Data != nil {
		tok = doc.Data.AccessToken
	}
	// 形状守卫：静默返回空会让后续请求全部 401，而错误显示成"凭证无效"
	if strings.TrimSpace(tok) == "" {
		return "", fmt.Errorf("业务令牌响应里没有 access_token（原始内容：%s）", truncate(string(raw), 200))
	}
	return tok, nil
}

// customerInfo 查机构与项目（两个服务商同一套路径，只是 host 与认证头不同）。
func (c *Client) customerInfo(ctx context.Context, host, authHeader string) (orgID, projectID string, err error) {
	data, err := c.bizGet(ctx, host+CustomerInfoPath, authHeader)
	if err != nil {
		return "", "", fmt.Errorf("查询机构信息失败: %w", err)
	}

	var doc struct {
		Organizations []struct {
			OrganizationID   string `json:"organizationId"`
			OrganizationName string `json:"organizationName"`
			Name             string `json:"name"`
			ID               string `json:"id"`
			OrgID            string `json:"orgId"`
			Projects         []struct {
				ProjectID   string `json:"projectId"`
				ProjectName string `json:"projectName"`
				Name        string `json:"name"`
				ID          string `json:"id"`
			} `json:"projects"`
		} `json:"organizations"`
		Orgs []struct {
			OrganizationID   string `json:"organizationId"`
			OrganizationName string `json:"organizationName"`
			Name             string `json:"name"`
			ID               string `json:"id"`
			OrgID            string `json:"orgId"`
			Projects         []struct {
				ProjectID   string `json:"projectId"`
				ProjectName string `json:"projectName"`
				Name        string `json:"name"`
				ID          string `json:"id"`
			} `json:"projects"`
		} `json:"orgs"`
	}
	if err := json.Unmarshal(data, &doc); err != nil {
		return "", "", fmt.Errorf("机构信息解析失败: %w（原始内容：%s）", err, truncate(string(data), 160))
	}

	orgs := doc.Organizations
	if len(orgs) == 0 {
		orgs = doc.Orgs
	}
	if len(orgs) == 0 {
		return "", "", fmt.Errorf("账号下没有任何机构（原始内容：%s）", truncate(string(data), 200))
	}

	// 优先选名字含"默认机构"的（编码套餐通常挂在那里），否则取第一个
	chosen := orgs[0]
	for _, o := range orgs {
		if strings.Contains(firstNonEmpty(o.OrganizationName, o.Name), defaultOrgMarker) {
			chosen = o
			break
		}
	}
	orgID = firstNonEmpty(chosen.OrganizationID, chosen.ID, chosen.OrgID)
	if orgID == "" {
		return "", "", fmt.Errorf("机构缺少 id 字段（原始内容：%s）", truncate(string(data), 200))
	}

	if len(chosen.Projects) == 0 {
		return "", "", fmt.Errorf("默认机构下没有任何项目（原始内容：%s）", truncate(string(data), 200))
	}
	proj := chosen.Projects[0]
	for _, p := range chosen.Projects {
		if strings.Contains(firstNonEmpty(p.ProjectName, p.Name), defaultProjectMarker) {
			proj = p
			break
		}
	}
	projectID = firstNonEmpty(proj.ProjectID, proj.ID)
	if projectID == "" {
		return "", "", fmt.Errorf("项目缺少 id 字段（原始内容：%s）", truncate(string(data), 200))
	}
	return orgID, projectID, nil
}

// findOrCreateAPIKey 找已存在的密钥，找不到就创建。
//
// 列表失败**静默忽略**直接创建（参考实现的做法）—— 列不出不代表建不了。
func (c *Client) findOrCreateAPIKey(ctx context.Context, host, authHeader, orgID, projectID string) (string, error) {
	listURL := host + fmt.Sprintf(APIKeysPathFmt, url.PathEscape(orgID), url.PathEscape(projectID))

	// 先尝试列出
	if data, err := c.bizGet(ctx, listURL, authHeader); err == nil {
		var keys []struct {
			Name   string `json:"name"`
			APIKey string `json:"apiKey"`
		}
		if json.Unmarshal(data, &keys) == nil {
			for _, k := range keys {
				if k.Name == APIKeyName && strings.TrimSpace(k.APIKey) != "" {
					return k.APIKey, nil
				}
			}
		}
	}

	// 创建
	body, _ := json.Marshal(map[string]string{"name": APIKeyName})
	ctx2, cancel := context.WithTimeout(ctx, c.timeout())
	defer cancel()
	req, err := http.NewRequestWithContext(ctx2, http.MethodPost, listURL, bytes.NewReader(body))
	if err != nil {
		return "", err
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Authorization", authHeader)
	req.Header.Set("Accept", "application/json")
	for k, v := range c.Identity.Headers() {
		if v != "" {
			req.Header.Set(k, v)
		}
	}

	resp, err := c.http().Do(req)
	if err != nil {
		return "", fmt.Errorf("创建 API 密钥失败: %w", err)
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if resp.StatusCode >= 400 {
		return "", NewError(resp.StatusCode, string(raw))
	}

	// ⚠ 响应有**信封**（`{"code":0,"data":{"apiKey":"..."}}`），必须解开。
	//
	// 实测踩到：直接读顶层 `apiKey` 会拿不到值 —— 因为真正的 key 在 `data` 里。
	// 这个 bug 的表现是"创建密钥的响应里没有 apiKey"，看起来像上游改了字段，
	// 实际是我们没解信封。
	//
	// 兼容两种形状（有的接口直接给顶层）：先试 data，再试顶层。
	var env struct {
		Code int `json:"code"`
		Data struct {
			APIKey string `json:"apiKey"`
		} `json:"data"`
		// 顶层兜底
		APIKey string `json:"apiKey"`
	}
	if err := json.Unmarshal(raw, &env); err != nil {
		return "", fmt.Errorf("创建密钥响应解析失败: %w（原始内容：%s）", err, truncate(string(raw), 200))
	}

	// 形状守卫（参考实现 CL-06）：上游字段改名时必须报错，
	// 而不是存下 "undefined" 让后续每个请求都 401。
	apiKey := firstNonEmpty(env.Data.APIKey, env.APIKey)
	if strings.TrimSpace(apiKey) == "" {
		return "", fmt.Errorf("创建密钥的响应里没有 apiKey（原始内容：%s）", truncate(string(raw), 200))
	}
	return apiKey, nil
}

// copySecretKey 取密钥的 secretKey。
//
// ## 为什么"取不到"要返回**错误**而不是空串
//
// 上游可能不返回 secretKey（返回 `{"code":0,"data":{}}`）。若这里静默返回
// `("", nil)`，调用方就**无法区分**"取不到"与"取到了空值" —— 于是
// zai 会拿一个单段凭证去用，而它**永远签不了名**（参考实现：
// `requireSecretKey=true` 对 zai 是硬要求）。
//
// 故这里把"取不到"当成错误返回，由**调用方**决定严重性：
//
//	zai      → 致命（必须两段式）
//	bigmodel → 降级为单段（secret 可选）
//
// 实测踩到：我第一版返回 `("", nil)`，导致 `TestExchangeZaiRequiresSecretKey`
// 失败 —— 测试抓到了这个真实缺陷。
func (c *Client) copySecretKey(ctx context.Context, host, authHeader, orgID, projectID, apiKey string) (string, error) {
	copyURL := host + fmt.Sprintf(APIKeyCopyPathFmt,
		url.PathEscape(orgID), url.PathEscape(projectID), url.PathEscape(apiKey))

	data, err := c.bizGet(ctx, copyURL, authHeader)
	if err != nil {
		return "", err
	}
	var doc struct {
		SecretKey    string `json:"secretKey"`
		SecretKeyAlt string `json:"secret_key"`
	}
	if err := json.Unmarshal(data, &doc); err != nil {
		return "", fmt.Errorf("secretKey 响应解析失败: %w（原始内容：%s）", err, truncate(string(data), 160))
	}
	secret := firstNonEmpty(doc.SecretKey, doc.SecretKeyAlt)
	if secret == "" {
		return "", fmt.Errorf("响应里没有 secretKey（原始内容：%s）", truncate(string(data), 160))
	}
	return secret, nil
}

// bizGet 业务接口的 GET（自动解信封）。
//
// 信封规则（参考实现 requestBizApi）：
//
//	code = body.code ?? body.status
//	code 非空且不是 0/200/"0"/"200" → 报错
//	返回 body.data ?? body
func (c *Client) bizGet(ctx context.Context, rawURL, authHeader string) (json.RawMessage, error) {
	ctx, cancel := context.WithTimeout(ctx, c.timeout())
	defer cancel()

	req, err := http.NewRequestWithContext(ctx, http.MethodGet, rawURL, nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Authorization", authHeader)
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Accept", "application/json")
	for k, v := range c.Identity.Headers() {
		if v != "" {
			req.Header.Set(k, v)
		}
	}

	resp, err := c.http().Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 4<<20))
	if resp.StatusCode >= 400 {
		return nil, NewError(resp.StatusCode, string(raw))
	}

	var env struct {
		Code any             `json:"code"`
		Msg  string          `json:"msg"`
		Data json.RawMessage `json:"data"`
		// 有些接口把状态放在 status
		Status any `json:"status"`
	}
	if err := json.Unmarshal(raw, &env); err != nil {
		// 不是信封形状：原样返回（有些接口直接给数据）
		return raw, nil
	}

	code := env.Code
	if code == nil {
		code = env.Status
	}
	if code != nil && !isOKCode(code) {
		return nil, fmt.Errorf("业务接口返回错误码 %v：%s", code, env.Msg)
	}
	if len(env.Data) > 0 && string(env.Data) != "null" {
		return env.Data, nil
	}
	return raw, nil
}

// isOKCode 判定信封的 code 是否表示成功。
//
// 接受 0 / 200 的**数字与字符串**两种形态（参考实现的做法）——
// 上游不同接口的表示不一致。
func isOKCode(v any) bool {
	switch x := v.(type) {
	case float64:
		return x == 0 || x == 200
	case int:
		return x == 0 || x == 200
	case string:
		s := strings.TrimSpace(x)
		return s == "0" || s == "200" || s == ""
	}
	return false
}

// randomHex 生成 n 字节的随机 hex 字符串（2n 个字符）。
func randomHex(n int) (string, error) {
	b := make([]byte, n)
	if _, err := rand.Read(b); err != nil {
		return "", err
	}
	return hex.EncodeToString(b), nil
}
