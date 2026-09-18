package zcode

// Z3 单元测试：OAuth 设备流、凭证兑换、会话管理。
//
// ## 测试重点
//
// 登录流程有 **11 个容易写错的细节**（字段名、错误语义、两个服务商的差异），
// 每个写错都会让登录失败且错误信息不指向真正原因。故逐条覆盖。
//
// 用 httptest 模拟上游，验证**请求形状**与**响应解析**；
// 真实链路验证在 live_test.go。

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// rewriteTo 把请求重定向到测试服务器的 transport。
type rewriteTo struct {
	base   http.RoundTripper
	target string
}

func (t rewriteTo) RoundTrip(req *http.Request) (*http.Response, error) {
	u, err := req.URL.Parse(t.target)
	if err != nil {
		return nil, err
	}
	req.URL.Scheme = u.Scheme
	req.URL.Host = u.Host
	base := t.base
	if base == nil {
		base = http.DefaultTransport
	}
	return base.RoundTrip(req)
}

// clientFor 构造一个把所有请求重定向到 srv 的 Client。
func clientFor(srv *httptest.Server) *Client {
	c := New()
	hc := srv.Client()
	hc.Transport = rewriteTo{base: srv.Client().Transport, target: srv.URL}
	c.HTTP = hc
	return c
}

// ---------------------------------------------------------------------------
// 发起登录
// ---------------------------------------------------------------------------

// TestStartLoginRejectsUnknownProvider 未指定服务商必须报错。
//
// 两个服务商的授权页不同（chat.z.ai vs bigmodel.cn），猜错会让用户
// 打开错误服务商的页面。
func TestStartLoginRejectsUnknownProvider(t *testing.T) {
	c := New()
	if _, err := c.StartLogin(context.Background(), ProviderUnknown); err == nil {
		t.Error("未指定服务商时应报错")
	}
}

// TestStartLoginRequestShape 发起请求的形状。
func TestStartLoginRequestShape(t *testing.T) {
	var gotBody map[string]string
	var gotAuth string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if !strings.HasSuffix(r.URL.Path, OAuthInitPath) {
			t.Errorf("应打 %s，实际 %s", OAuthInitPath, r.URL.Path)
		}
		gotAuth = r.Header.Get("Authorization")
		_ = json.NewDecoder(r.Body).Decode(&gotBody)
		_ = json.NewEncoder(w).Encode(map[string]any{
			"code": 0,
			"data": map[string]any{
				"flow_id":          "flow-123",
				"authorize_url":    "https://example.com/authorize?x=1",
				"expires_at":       1800000000,
				"poll_interval_sec": 2,
			},
		})
	}))
	defer srv.Close()

	c := clientFor(srv)
	s, err := c.StartLogin(context.Background(), ProviderZAI)
	if err != nil {
		t.Fatal(err)
	}

	// body 必须是 {"provider":"zai"}
	if gotBody["provider"] != "zai" {
		t.Errorf("body 里 provider 应为 zai，实际 %q", gotBody["provider"])
	}
	// pollToken 必须是 64 位 hex（32 字节）
	if !strings.HasPrefix(gotAuth, "Bearer ") {
		t.Errorf("Authorization 应为 Bearer 前缀，实际 %q", gotAuth)
	}
	tok := strings.TrimPrefix(gotAuth, "Bearer ")
	if len(tok) != 64 {
		t.Errorf("pollToken 应为 64 字符 hex（32 字节），实际 %d 字符", len(tok))
	}
	if _, err := hexDecode(tok); err != nil {
		t.Errorf("pollToken 应为合法 hex: %v", err)
	}

	// 会话字段
	if s.FlowID != "flow-123" || s.AuthorizeURL == "" {
		t.Errorf("会话字段解析错误: %+v", s)
	}
	if s.PollToken != tok {
		t.Error("会话里应保存我们生成的 pollToken（轮询要用它）")
	}
	if s.ExpiresAt != 1800000000 {
		t.Errorf("expires_at 应为 1800000000，实际 %d", s.ExpiresAt)
	}
}

// TestStartLoginShapeGuards 缺关键字段时必须报错。
//
// 缺字段会让后续轮询必然失败，而错误会显示成"轮询失败" ——
// 看不出是发起时就缺了东西。
func TestStartLoginShapeGuards(t *testing.T) {
	cases := []struct {
		name string
		data map[string]any
	}{
		{"缺 flow_id", map[string]any{"authorize_url": "https://x", "expires_at": 1800000000, "poll_interval_sec": 2}},
		{"缺 authorize_url", map[string]any{"flow_id": "f", "expires_at": 1800000000, "poll_interval_sec": 2}},
		{"缺 expires_at", map[string]any{"flow_id": "f", "authorize_url": "https://x", "poll_interval_sec": 2}},
		{"expires_at 为 0", map[string]any{"flow_id": "f", "authorize_url": "https://x", "expires_at": 0, "poll_interval_sec": 2}},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				_ = json.NewEncoder(w).Encode(map[string]any{"code": 0, "data": tc.data})
			}))
			defer srv.Close()
			c := clientFor(srv)
			if _, err := c.StartLogin(context.Background(), ProviderZAI); err == nil {
				t.Error("缺关键字段时应报错（形状守卫）")
			}
		})
	}
}

// TestStartLoginNonZeroCode 信封 code ≠ 0 必须报错。
func TestStartLoginNonZeroCode(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_ = json.NewEncoder(w).Encode(map[string]any{"code": 3001, "msg": "invalid provider"})
	}))
	defer srv.Close()
	c := clientFor(srv)
	if _, err := c.StartLogin(context.Background(), ProviderZAI); err == nil {
		t.Error("code≠0 应报错")
	}
}

// ---------------------------------------------------------------------------
// 轮询
// ---------------------------------------------------------------------------

// TestPollPendingIsNotError pending 必须是**可区分的中间态**。
//
// 若与真失败混在一起，界面只能笼统报错，用户不知道该再等等还是重新登录。
func TestPollPendingIsNotError(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_ = json.NewEncoder(w).Encode(map[string]any{"code": 0, "data": map[string]any{"status": "pending"}})
	}))
	defer srv.Close()

	c := clientFor(srv)
	s := &LoginSession{Provider: ProviderZAI, FlowID: "f", PollToken: "t"}
	_, err := c.PollOnce(context.Background(), s)
	if err == nil {
		t.Fatal("pending 时应返回错误（表示「还没好」）")
	}
	if _, ok := err.(LoginPending); !ok {
		t.Errorf("pending 应返回 LoginPending（供界面区分），实际 %T: %v", err, err)
	}
}

// TestPollErrorSemantics 轮询的错误语义（照抄参考实现，实测确认）。
//
// 哪些当致命、哪些当重试是刻意的：
//   · 把临时故障当致命 → 用户白重登一次
//   · 把真失败当重试   → 用户无限等下去
func TestPollErrorSemantics(t *testing.T) {
	cases := []struct {
		name       string
		status     int
		body       string
		wantFatal  bool
		wantReason string
	}{
		{"404 致命", 404, `{"code":1,"msg":"not found"}`, true, "4xx 除 408/429 外应致命"},
		{"400 致命", 400, `{"code":1,"msg":"bad"}`, true, "4xx 应致命"},
		{"408 可重试", 408, ``, false, "408 应重试"},
		{"429 可重试", 429, ``, false, "429 应重试"},
		{"500 可重试", 500, ``, false, "5xx 应重试"},
		{"503 可重试", 503, ``, false, "5xx 应重试"},
		{"非 JSON 可重试", 200, `not json`, false, "非 JSON 应重试"},
		{"code 非数字可重试", 200, `{"code":"x","data":{}}`, false, "code 非数字应重试"},
		{"code≠0 致命", 200, `{"code":3001,"msg":"bad"}`, true, "code≠0 应致命"},
		{"status=failed 致命", 200, `{"code":0,"data":{"status":"failed"}}`, true, "failed 应致命"},
		{"status 未知 致命", 200, `{"code":0,"data":{"status":"weird"}}`, true, "未知 status 应致命"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				w.WriteHeader(tc.status)
				_, _ = w.Write([]byte(tc.body))
			}))
			defer srv.Close()

			c := clientFor(srv)
			s := &LoginSession{Provider: ProviderZAI, FlowID: "f", PollToken: "t"}
			_, err := c.PollOnce(context.Background(), s)
			if err == nil {
				t.Fatal("应返回错误")
			}
			_, isPending := err.(LoginPending)
			gotFatal := !isPending
			if gotFatal != tc.wantFatal {
				t.Errorf("%s：期望致命=%v，实际致命=%v（err=%v）", tc.wantReason, tc.wantFatal, gotFatal, err)
			}
		})
	}
}

// TestPollUsesOwnPollToken 轮询必须用**我们自己生成**的 pollToken。
//
// 参考实现明确写了：上游返回的 poll_token 仅供参考，
// 轮询时客户端仍然发自己生成的那个。发错会 401。
func TestPollUsesOwnPollToken(t *testing.T) {
	var gotAuth string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotAuth = r.Header.Get("Authorization")
		_ = json.NewEncoder(w).Encode(map[string]any{"code": 0, "data": map[string]any{"status": "pending"}})
	}))
	defer srv.Close()

	c := clientFor(srv)
	s := &LoginSession{Provider: ProviderZAI, FlowID: "f", PollToken: "my-own-token"}
	_, _ = c.PollOnce(context.Background(), s)

	if gotAuth != "Bearer my-own-token" {
		t.Errorf("轮询应发自己生成的 pollToken，实际 %q", gotAuth)
	}
}

// TestPollSuccessExtractsProviderToken provider token 的 key 就是 provider 名。
//
// 这是最容易写错的一处：`data.zai.access_token` / `data.bigmodel.access_token`
// —— key 是 provider 字符串本身，不是固定的 "access_token" 字段。
func TestPollSuccessExtractsProviderToken(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_ = json.NewEncoder(w).Encode(map[string]any{
			"code": 0,
			"data": map[string]any{
				"status": "ready",
				"token":  "jwt-final",
				"user":   map[string]any{"user_id": "user_9"},
				"zai":    map[string]any{"access_token": "zai-at"},
				"bigmodel": map[string]any{
					"access_token":  "bm-at",
					"refresh_token": "bm-rt",
				},
			},
		})
	}))
	defer srv.Close()

	c := clientFor(srv)

	// zai
	s := &LoginSession{Provider: ProviderZAI, FlowID: "f", PollToken: "t"}
	tk, err := c.PollOnce(context.Background(), s)
	if err != nil {
		t.Fatal(err)
	}
	if tk.AccessToken != "zai-at" {
		t.Errorf("zai 的 access_token 应为 zai-at，实际 %q", tk.AccessToken)
	}
	if tk.JWT != "jwt-final" {
		t.Errorf("JWT 应为 jwt-final，实际 %q", tk.JWT)
	}
	if tk.UserID != "user_9" {
		t.Errorf("user_id 应为 user_9，实际 %q", tk.UserID)
	}

	// bigmodel
	s2 := &LoginSession{Provider: ProviderBigmodel, FlowID: "f", PollToken: "t"}
	tk2, err := c.PollOnce(context.Background(), s2)
	if err != nil {
		t.Fatal(err)
	}
	if tk2.AccessToken != "bm-at" || tk2.RefreshToken != "bm-rt" {
		t.Errorf("bigmodel 令牌解析错误: %+v", tk2)
	}
}

// TestPollReadyWithoutProviderTokenFails ready 但缺 provider token 必须报错。
//
// 静默接受会让后续请求全部失败，而错误显示成"凭证无效"。
func TestPollReadyWithoutProviderTokenFails(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_ = json.NewEncoder(w).Encode(map[string]any{
			"code": 0,
			"data": map[string]any{
				"status": "ready",
				"token":  "jwt",
				// 没有 zai 字段
			},
		})
	}))
	defer srv.Close()

	c := clientFor(srv)
	s := &LoginSession{Provider: ProviderZAI, FlowID: "f", PollToken: "t"}
	if _, err := c.PollOnce(context.Background(), s); err == nil {
		t.Error("ready 但缺 provider token 时应报错")
	}
}

// ---------------------------------------------------------------------------
// 凭证兑换
// ---------------------------------------------------------------------------

// TestExchangeZaiFullFlow zai 的完整兑换链。
//
//	z/login → getCustomerInfo → api_keys → copy → {apiKey}.{secretKey}
func TestExchangeZaiFullFlow(t *testing.T) {
	var paths []string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		paths = append(paths, r.URL.Path)
		switch {
		case strings.HasSuffix(r.URL.Path, ZaiLoginPath):
			// ⚠ 这个端点不用信封判定，直接读顶层字段
			_ = json.NewEncoder(w).Encode(map[string]any{"access_token": "biz-token"})
		case strings.HasSuffix(r.URL.Path, CustomerInfoPath):
			if r.Header.Get("Authorization") != "Bearer biz-token" {
				t.Errorf("业务接口应带 Bearer biz-token，实际 %q", r.Header.Get("Authorization"))
			}
			_ = json.NewEncoder(w).Encode(map[string]any{
				"code": 0,
				"data": map[string]any{
					"organizations": []any{map[string]any{
						"organizationId":   "org1",
						"organizationName": "默认机构",
						"projects": []any{map[string]any{
							"projectId": "proj1", "projectName": "默认项目",
						}},
					}},
				},
			})
		case strings.Contains(r.URL.Path, "/api_keys/copy/"):
			_ = json.NewEncoder(w).Encode(map[string]any{
				"code": 0, "data": map[string]any{"secretKey": "sk-secret"},
			})
		case strings.HasSuffix(r.URL.Path, "/api_keys"):
			// 列表返回已有密钥
			_ = json.NewEncoder(w).Encode(map[string]any{
				"code": 0,
				"data": []any{map[string]any{"name": APIKeyName, "apiKey": "ak-123"}},
			})
		default:
			t.Errorf("未预期的请求路径: %s", r.URL.Path)
		}
	}))
	defer srv.Close()

	c := clientFor(srv)
	cred, err := c.ExchangeCredential(context.Background(), &LoginTokens{
		Provider: ProviderZAI, AccessToken: "oauth-at", JWT: "jwt-x", JWTIssuedAt: 1700000000,
	})
	if err != nil {
		t.Fatal(err)
	}
	if cred.Credential != "ak-123.sk-secret" {
		t.Errorf("最终凭证应为 ak-123.sk-secret，实际 %q", cred.Credential)
	}
	if cred.Provider != ProviderZAI {
		t.Errorf("服务商应为 zai，实际 %q", cred.Provider)
	}
	if cred.JWT != "jwt-x" || cred.JWTIssuedAt != 1700000000 {
		t.Error("JWT 与其签发时间应被保留（额度查询要用）")
	}
	// 验证走了正确的路径
	joined := strings.Join(paths, " ")
	for _, want := range []string{ZaiLoginPath, CustomerInfoPath, "api_keys", "copy"} {
		if !strings.Contains(joined, want) {
			t.Errorf("兑换流程应经过 %s，实际路径: %s", want, joined)
		}
	}
}

// TestExchangeBigmodelNoZLogin bigmodel **没有 z/login 这一步**，
// 且认证头是**裸值**（不带 Bearer 前缀）。
//
// 这是两个服务商最容易搞混的差异。
func TestExchangeBigmodelNoZLogin(t *testing.T) {
	var sawZLogin bool
	var gotAuth string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if strings.HasSuffix(r.URL.Path, ZaiLoginPath) {
			sawZLogin = true
		}
		if strings.HasSuffix(r.URL.Path, CustomerInfoPath) {
			gotAuth = r.Header.Get("Authorization")
		}
		switch {
		case strings.HasSuffix(r.URL.Path, CustomerInfoPath):
			_ = json.NewEncoder(w).Encode(map[string]any{
				"code": 0,
				"data": map[string]any{
					"organizations": []any{map[string]any{
						"organizationId": "org1", "organizationName": "默认机构",
						"projects": []any{map[string]any{"projectId": "proj1", "projectName": "默认项目"}},
					}},
				},
			})
		case strings.Contains(r.URL.Path, "/api_keys/copy/"):
			// bigmodel 拿不到 secretKey 也应成功（降级为单段）
			_ = json.NewEncoder(w).Encode(map[string]any{"code": 0, "data": map[string]any{}})
		case strings.HasSuffix(r.URL.Path, "/api_keys"):
			_ = json.NewEncoder(w).Encode(map[string]any{
				"code": 0, "data": []any{map[string]any{"name": APIKeyName, "apiKey": "bm-ak"}},
			})
		}
	}))
	defer srv.Close()

	c := clientFor(srv)
	cred, err := c.ExchangeCredential(context.Background(), &LoginTokens{
		Provider: ProviderBigmodel, AccessToken: "bm-oauth-at",
	})
	if err != nil {
		t.Fatal(err)
	}

	if sawZLogin {
		t.Error("bigmodel **不应**调用 z/login（它直接用 accessToken 当 biz token）")
	}
	// 裸值：不带 "Bearer " 前缀
	if gotAuth != "bm-oauth-at" {
		t.Errorf("bigmodel 的认证头应是裸值 %q，实际 %q", "bm-oauth-at", gotAuth)
	}
	// secretKey 拿不到 → 降级为单段
	if cred.Credential != "bm-ak" {
		t.Errorf("bigmodel 拿不到 secretKey 时应降级为单段，实际 %q", cred.Credential)
	}
}

// TestExchangeZaiRequiresSecretKey zai 缺 secretKey 必须**失败**。
//
// 参考实现：`requireSecretKey=true` —— 存一个永远签不了名的凭证
// 比直接失败更糟（用户会看到"凭证无效"而不知道原因）。
func TestExchangeZaiRequiresSecretKey(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch {
		case strings.HasSuffix(r.URL.Path, ZaiLoginPath):
			_ = json.NewEncoder(w).Encode(map[string]any{"access_token": "biz"})
		case strings.HasSuffix(r.URL.Path, CustomerInfoPath):
			_ = json.NewEncoder(w).Encode(map[string]any{
				"code": 0,
				"data": map[string]any{
					"organizations": []any{map[string]any{
						"organizationId": "org1", "organizationName": "默认机构",
						"projects": []any{map[string]any{"projectId": "proj1", "projectName": "默认项目"}},
					}},
				},
			})
		case strings.Contains(r.URL.Path, "/api_keys/copy/"):
			// 故意不给 secretKey
			_ = json.NewEncoder(w).Encode(map[string]any{"code": 0, "data": map[string]any{}})
		case strings.HasSuffix(r.URL.Path, "/api_keys"):
			_ = json.NewEncoder(w).Encode(map[string]any{
				"code": 0, "data": []any{map[string]any{"name": APIKeyName, "apiKey": "ak"}},
			})
		}
	}))
	defer srv.Close()

	c := clientFor(srv)
	if _, err := c.ExchangeCredential(context.Background(), &LoginTokens{
		Provider: ProviderZAI, AccessToken: "at",
	}); err == nil {
		t.Error("zai 缺 secretKey 时应报错（否则会存下永远用不了的凭证）")
	}
}

// TestExchangeCreatesKeyWhenMissing 没有已有密钥时应创建。
func TestExchangeCreatesKeyWhenMissing(t *testing.T) {
	created := false
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch {
		case strings.HasSuffix(r.URL.Path, ZaiLoginPath):
			_ = json.NewEncoder(w).Encode(map[string]any{"access_token": "biz"})
		case strings.HasSuffix(r.URL.Path, CustomerInfoPath):
			_ = json.NewEncoder(w).Encode(map[string]any{
				"code": 0,
				"data": map[string]any{
					"organizations": []any{map[string]any{
						"organizationId": "org1", "organizationName": "默认机构",
						"projects": []any{map[string]any{"projectId": "proj1", "projectName": "默认项目"}},
					}},
				},
			})
		case strings.Contains(r.URL.Path, "/api_keys/copy/"):
			_ = json.NewEncoder(w).Encode(map[string]any{"code": 0, "data": map[string]any{"secretKey": "s"}})
		case strings.HasSuffix(r.URL.Path, "/api_keys"):
			if r.Method == http.MethodPost {
				created = true
				// 验证创建时的 body
				var b map[string]string
				_ = json.NewDecoder(r.Body).Decode(&b)
				if b["name"] != APIKeyName {
					t.Errorf("创建密钥的名字应为 %q，实际 %q", APIKeyName, b["name"])
				}
				_ = json.NewEncoder(w).Encode(map[string]any{
					"code": 0, "data": map[string]any{"apiKey": "new-ak"},
				})
				return
			}
			// 列表为空 → 触发创建
			_ = json.NewEncoder(w).Encode(map[string]any{"code": 0, "data": []any{}})
		}
	}))
	defer srv.Close()

	c := clientFor(srv)
	cred, err := c.ExchangeCredential(context.Background(), &LoginTokens{
		Provider: ProviderZAI, AccessToken: "at",
	})
	if err != nil {
		t.Fatal(err)
	}
	if !created {
		t.Error("列表里没有密钥时应创建")
	}
	if cred.Credential != "new-ak.s" {
		t.Errorf("应使用新建的密钥，实际 %q", cred.Credential)
	}
}

// TestExchangeShapeGuardOnCreate 创建密钥的响应缺 apiKey 时必须报错。
//
// 参考实现 CL-06：上游字段改名时必须失败，而不是存下 "undefined"
// 让后续每个请求都 401。
func TestExchangeShapeGuardOnCreate(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch {
		case strings.HasSuffix(r.URL.Path, ZaiLoginPath):
			_ = json.NewEncoder(w).Encode(map[string]any{"access_token": "biz"})
		case strings.HasSuffix(r.URL.Path, CustomerInfoPath):
			_ = json.NewEncoder(w).Encode(map[string]any{
				"code": 0,
				"data": map[string]any{
					"organizations": []any{map[string]any{
						"organizationId": "org1", "organizationName": "默认机构",
						"projects": []any{map[string]any{"projectId": "proj1", "projectName": "默认项目"}},
					}},
				},
			})
		case strings.HasSuffix(r.URL.Path, "/api_keys"):
			if r.Method == http.MethodPost {
				// 字段改名了（apiKey → key）
				_ = json.NewEncoder(w).Encode(map[string]any{"code": 0, "data": map[string]any{"key": "x"}})
				return
			}
			_ = json.NewEncoder(w).Encode(map[string]any{"code": 0, "data": []any{}})
		}
	}))
	defer srv.Close()

	c := clientFor(srv)
	if _, err := c.ExchangeCredential(context.Background(), &LoginTokens{
		Provider: ProviderZAI, AccessToken: "at",
	}); err == nil {
		t.Error("创建响应缺 apiKey 时应报错（形状守卫）")
	}
}

// TestExchangeNoOrganizations 账号下没有机构时必须报错。
func TestExchangeNoOrganizations(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch {
		case strings.HasSuffix(r.URL.Path, ZaiLoginPath):
			_ = json.NewEncoder(w).Encode(map[string]any{"access_token": "biz"})
		case strings.HasSuffix(r.URL.Path, CustomerInfoPath):
			_ = json.NewEncoder(w).Encode(map[string]any{"code": 0, "data": map[string]any{"organizations": []any{}}})
		}
	}))
	defer srv.Close()

	c := clientFor(srv)
	if _, err := c.ExchangeCredential(context.Background(), &LoginTokens{
		Provider: ProviderZAI, AccessToken: "at",
	}); err == nil {
		t.Error("没有机构时应报错")
	}
}

// TestZaiBizTokenShapeGuard 业务令牌响应缺 access_token 必须报错。
func TestZaiBizTokenShapeGuard(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		// 上游返回了错误但 HTTP 200（这个端点不用信封判定）
		_ = json.NewEncoder(w).Encode(map[string]any{"code": 500, "msg": "invalid", "data": nil})
	}))
	defer srv.Close()

	c := clientFor(srv)
	if _, err := c.zaiBizToken(context.Background(), "bad-token"); err == nil {
		t.Error("响应里没有 access_token 时应报错（否则存下空凭证，后续全 401）")
	}
}

// TestZaiBizTokenAcceptsThreeShapes 业务令牌的三种响应形状都要兼容。
func TestZaiBizTokenAcceptsThreeShapes(t *testing.T) {
	cases := []struct {
		name string
		resp map[string]any
		want string
	}{
		{"顶层 access_token", map[string]any{"access_token": "t1"}, "t1"},
		{"顶层 accessToken", map[string]any{"accessToken": "t2"}, "t2"},
		{"嵌套 data.access_token", map[string]any{"data": map[string]any{"access_token": "t3"}}, "t3"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				_ = json.NewEncoder(w).Encode(tc.resp)
			}))
			defer srv.Close()
			c := clientFor(srv)
			got, err := c.zaiBizToken(context.Background(), "at")
			if err != nil {
				t.Fatal(err)
			}
			if got != tc.want {
				t.Errorf("应解析出 %q，实际 %q", tc.want, got)
			}
		})
	}
}

// ---------------------------------------------------------------------------
// 会话管理
// ---------------------------------------------------------------------------

// TestSessionRoundTrip 会话落盘与读回。
func TestSessionRoundTrip(t *testing.T) {
	dir := t.TempDir()
	authDir := filepath.Join(dir, "auths")
	s := &LoginSession{
		Provider: ProviderZAI, FlowID: "flow-abc", PollToken: "tok",
		AuthorizeURL: "https://x", ExpiresAt: 1800000000, PollIntervalSec: 3,
		// ⚠ CreatedAt 必须是**当前时间**：它用于判断会话是否过期，
		// 写一个过去的固定值会让 loadSession 判它已过期（我第一版就写错了，
		// 测试报"登录会话已超过 30 分钟"）。
		CreatedAt: time.Now().Unix(),
	}
	if err := saveSession(authDir, s); err != nil {
		t.Fatal(err)
	}
	back, err := loadSession(authDir, "flow-abc")
	if err != nil {
		t.Fatal(err)
	}
	if back.FlowID != s.FlowID || back.PollToken != s.PollToken || back.PollIntervalSec != 3 {
		t.Errorf("往返不一致: %+v", back)
	}
}

// TestSessionNotInAuthDir 会话文件**不能**放在 authDir 里。
//
// authDir 会被 LoadDir 用 glob `zcode*.json` 扫描 ——
// 会话文件混进去会被当成凭证解析并报错。
func TestSessionNotInAuthDir(t *testing.T) {
	dir := t.TempDir()
	authDir := filepath.Join(dir, "auths")
	s := &LoginSession{Provider: ProviderZAI, FlowID: "f", PollToken: "t", CreatedAt: time.Now().Unix()}
	if err := saveSession(authDir, s); err != nil {
		t.Fatal(err)
	}

	// authDir 里不该有任何文件（LoadDir 会扫它）
	if entries, err := os.ReadDir(authDir); err == nil && len(entries) > 0 {
		t.Errorf("authDir 里不应有文件（会被当凭证解析），实际 %d 个", len(entries))
	}
	// 会话应在同级 sessions/ 下
	if _, err := os.Stat(filepath.Join(dir, "sessions")); err != nil {
		t.Errorf("会话目录应在 authDir 同级: %v", err)
	}
}

// TestSessionExpiry 过期会话必须被拒绝并清理。
func TestSessionExpiry(t *testing.T) {
	dir := t.TempDir()
	authDir := filepath.Join(dir, "auths")
	s := &LoginSession{
		Provider: ProviderZAI, FlowID: "old", PollToken: "t",
		// 超过 TTL
		CreatedAt: time.Now().Unix() - int64(sessionTTL.Seconds()) - 60,
	}
	if err := saveSession(authDir, s); err != nil {
		t.Fatal(err)
	}
	if _, err := loadSession(authDir, "old"); err == nil {
		t.Error("过期会话应被拒绝")
	}
	// 且应被清理
	if _, err := os.Stat(sessionFile(authDir, "old")); !os.IsNotExist(err) {
		t.Error("过期会话文件应被删除")
	}
}

// TestSessionMissing 不存在的会话应给可读错误。
func TestSessionMissing(t *testing.T) {
	dir := t.TempDir()
	_, err := loadSession(filepath.Join(dir, "auths"), "nonexistent")
	if err == nil {
		t.Fatal("不存在的会话应报错")
	}
	if !strings.Contains(err.Error(), "重新发起") {
		t.Errorf("错误文案应指导用户重新发起登录，实际 %q", err.Error())
	}
}

// TestSanitizeNameBlocksPathTraversal 会话文件名必须防路径穿越。
//
// 安全项：flowID 来自上游响应，直接拼进路径的话，
// 异常的 flowID（如 "../../etc/passwd"）会写到目录外。
func TestSanitizeNameBlocksPathTraversal(t *testing.T) {
	got := sanitizeName("../../evil")
	if strings.Contains(got, "/") || strings.Contains(got, "\\") || strings.Contains(got, "..") {
		t.Errorf("清理后仍含路径字符: %q", got)
	}
	if sanitizeName("") != "unknown" {
		t.Errorf("空值应回退成 unknown，实际 %q", sanitizeName(""))
	}
	if sanitizeName("flow-123_abc") != "flow-123_abc" {
		t.Error("合法字符应保留")
	}
}

// TestPollIntervalHasFloor 轮询间隔必须有下限。
//
// 上游可能返回 0 或很小的值；没有下限会导致高频轮询（打上游）。
func TestPollIntervalHasFloor(t *testing.T) {
	cases := map[int]bool{
		0:  true, // 应回退到 ≥1s
		-1: true,
		1:  false,
		5:  false,
	}
	for sec, shouldFloor := range cases {
		s := &LoginSession{PollIntervalSec: sec}
		d := s.PollInterval()
		if d < time.Second {
			t.Errorf("PollInterval(%d) = %v，应至少 1 秒", sec, d)
		}
		_ = shouldFloor
	}
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

func hexDecode(s string) ([]byte, error) {
	out := make([]byte, len(s)/2)
	for i := 0; i < len(out); i++ {
		hi := hexVal(s[i*2])
		lo := hexVal(s[i*2+1])
		if hi < 0 || lo < 0 {
			return nil, os.ErrInvalid
		}
		out[i] = byte(hi<<4 | lo)
	}
	return out, nil
}

func hexVal(c byte) int {
	switch {
	case c >= '0' && c <= '9':
		return int(c - '0')
	case c >= 'a' && c <= 'f':
		return int(c-'a') + 10
	case c >= 'A' && c <= 'F':
		return int(c-'A') + 10
	}
	return -1
}
