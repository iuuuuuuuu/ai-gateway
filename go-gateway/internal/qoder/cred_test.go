package qoder

// 凭证解析、双区判定、令牌刷新的单元测试。
//
// 这些是"用户能直接踩到"的路径：导入一个格式不对的文件、国际版账号被当成
// 国服、刷新后没落盘导致重启失效 —— 每一条都有真实的失败现象。

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

// TestParseNestedForm 嵌套形（OAuth 落盘的标准形态）。
func TestParseNestedForm(t *testing.T) {
	raw := []byte(`{
		"auth": {"accessToken":"dt-abc","refreshToken":"drt-xyz","expiresAt":1800000000,"domain":"qoder.com.cn"},
		"account": {"uid":"uid-1","nickname":"昵称"}
	}`)
	c, err := Parse(raw, "qoder-uid-1.json")
	if err != nil {
		t.Fatal(err)
	}
	if c.DT != "dt-abc" || c.DRT != "drt-xyz" {
		t.Errorf("令牌解析错误: DT=%q DRT=%q", c.DT, c.DRT)
	}
	if c.UID != "uid-1" || c.Nickname != "昵称" {
		t.Errorf("账号信息解析错误: uid=%q nick=%q", c.UID, c.Nickname)
	}
	if c.DTExpiresAt != 1800000000 {
		t.Errorf("过期时间解析错误: %d", c.DTExpiresAt)
	}
	if c.Region != RegionCN {
		t.Errorf("区域应为国服，实际 %q", c.Region)
	}
}

// TestParseFlatForm 扁平形（手写/旧版）。
func TestParseFlatForm(t *testing.T) {
	raw := []byte(`{"token":"dt-flat","refresh_token":"drt-flat","expires_at_unix":1800000001,"user_id":"uid-2","nickname":"扁平"}`)
	c, err := Parse(raw, "qoder-uid-2.json")
	if err != nil {
		t.Fatal(err)
	}
	if c.DT != "dt-flat" || c.DRT != "drt-flat" {
		t.Errorf("扁平形令牌解析错误: DT=%q DRT=%q", c.DT, c.DRT)
	}
	if c.UID != "uid-2" {
		t.Errorf("uid 应兼容 user_id 字段，实际 %q", c.UID)
	}
	// 扁平形没有 domain → 区域必须是"未知"，**不能猜成国服**
	if c.Region != RegionUnknown {
		t.Errorf("无 domain 时区域应为未知（不猜），实际 %q", c.Region)
	}
}

// TestParseCamelCaseRefreshToken 兼容 refreshToken 拼写。
//
// 本仓库宿主导出的形态用的是 camelCase，而参考实现用 snake_case ——
// 漏掉任何一种都会让刷新失败（表现为"用一会儿就掉线"）。
func TestParseCamelCaseRefreshToken(t *testing.T) {
	raw := []byte(`{"accessToken":"dt-a","refreshToken":"drt-b","expiresAt":1800000002,"uid":"uid-3"}`)
	c, err := Parse(raw, "qoder-uid-3.json")
	if err != nil {
		t.Fatal(err)
	}
	if c.DRT != "drt-b" {
		t.Errorf("应兼容 camelCase refreshToken，实际 %q", c.DRT)
	}
	if c.DTExpiresAt != 1800000002 {
		t.Errorf("应兼容 camelCase expiresAt，实际 %d", c.DTExpiresAt)
	}
}

// TestParseUIDFromFilename 无 uid 字段时从文件名兜底。
func TestParseUIDFromFilename(t *testing.T) {
	raw := []byte(`{"auth":{"accessToken":"dt-x"},"account":{}}`)
	c, err := Parse(raw, "/some/dir/qoder-uid-from-name.json")
	if err != nil {
		t.Fatal(err)
	}
	if c.UID != "uid-from-name" {
		t.Errorf("应从文件名取 uid，实际 %q", c.UID)
	}
}

// TestParseRejectsEmptyToken 既无 DT 也无 DRT 时必须报错。
//
// 静默接受一个空凭证，会让它在池里"看起来正常但永远失败"。
func TestParseRejectsEmptyToken(t *testing.T) {
	raw := []byte(`{"auth":{"accessToken":""},"account":{"uid":"u"}}`)
	if _, err := Parse(raw, "qoder-u.json"); err == nil {
		t.Error("空令牌应报错，实际接受了")
	}
}

// TestParseRejectsBadJSON 非法 JSON 必须报错（而不是静默跳过）。
//
// 导入功能依赖这条：用户手写的文件格式不对时，必须让他看到具体原因。
func TestParseRejectsBadJSON(t *testing.T) {
	if _, err := Parse([]byte(`{不是 json`), "qoder-u.json"); err == nil {
		t.Error("非法 JSON 应报错")
	}
	if _, err := Parse([]byte(``), "qoder-u.json"); err == nil {
		t.Error("空文件应报错")
	}
}

// TestRegionFromDomain 双区判定。
//
// 每一条都对应实测确认过的域名（见 region.go 的注释）。
func TestRegionFromDomain(t *testing.T) {
	cases := []struct {
		domain string
		want   Region
		why    string
	}{
		{"qoder.com.cn", RegionCN, "国服主域"},
		{"openapi.qoder.com.cn", RegionCN, "国服 openapi"},
		{"gateway.qoder.com.cn", RegionCN, "国服 gateway"},
		{"qoder.sh", RegionIntl, "国际版 API 根域"},
		{"api3.qoder.sh", RegionIntl, "国际版 gateway（本机客户端实测用它）"},
		{"openapi.qoder.sh", RegionIntl, "国际版 openapi"},
		{"qoder.com", RegionIntl, "国际版授权页 —— 注意它不是 .com.cn"},
		{"", RegionUnknown, "空值不猜"},
		{"example.com", RegionUnknown, "无关域名不猜"},
	}
	for _, tc := range cases {
		got := RegionFromDomain(tc.domain)
		if got != tc.want {
			t.Errorf("RegionFromDomain(%q) = %q，期望 %q（%s）", tc.domain, got, tc.want, tc.why)
		}
	}
}

// TestRegionEndpointsMatchReality 两区的端点必须与实测一致。
//
// 特别注意：国际版的**授权页**在 qoder.com，而 API 在 qoder.sh ——
// 这两个不同域，写成同一个会导致授权链接打不开（qoder.sh 的网页不存在，
// DNS 都不解析，实测确认）。
func TestRegionEndpointsMatchReality(t *testing.T) {
	cn := RegionCN
	if cn.Website() != "https://qoder.com.cn" {
		t.Errorf("国服授权页错误: %s", cn.Website())
	}
	if cn.OpenAPI() != "https://openapi.qoder.com.cn" {
		t.Errorf("国服 openapi 错误: %s", cn.OpenAPI())
	}
	if cn.Gateway() != "https://gateway.qoder.com.cn" {
		t.Errorf("国服 gateway 错误: %s", cn.Gateway())
	}

	intl := RegionIntl
	if intl.Website() != "https://qoder.com" {
		t.Errorf("国际版授权页应为 qoder.com（不是 .sh），实际 %s", intl.Website())
	}
	if intl.OpenAPI() != "https://openapi.qoder.sh" {
		t.Errorf("国际版 openapi 错误: %s", intl.OpenAPI())
	}
	if intl.Gateway() != "https://api3.qoder.sh" {
		t.Errorf("国际版 gateway 错误: %s", intl.Gateway())
	}

	// 两区的端点不能相同（否则双区支持形同虚设）
	if cn.Gateway() == intl.Gateway() {
		t.Error("两区 gateway 不应相同")
	}
}

// TestRegionLabel 区域名面向用户。
func TestRegionLabel(t *testing.T) {
	if RegionCN.Label() != "国服" {
		t.Errorf("RegionCN.Label() = %q", RegionCN.Label())
	}
	if RegionIntl.Label() != "国际版" {
		t.Errorf("RegionIntl.Label() = %q", RegionIntl.Label())
	}
	// 未知区域必须给出可读说明，而不是空串（界面会显示成空白）
	if RegionUnknown.Label() == "" {
		t.Error("未知区域应有可读标签")
	}
}

// TestLoginStartRejectsUnknownRegion 未指定区域时必须拒绝。
//
// 两区的授权页不同，猜错会让用户打开一个错误区域的页面、
// 登录成功后账号却指向另一个区 —— 这种错很难被发现。
func TestLoginStartRejectsUnknownRegion(t *testing.T) {
	if _, err := LoginStart(RegionUnknown); err == nil {
		t.Error("未指定区域时应报错")
	}
}

// TestLoginStartBuildsAuthURL 授权链接的形状与两区域名。
//
// ⚠ `redirect_uri` **不再**是必含项（2026-09-22 按官方源码改正）：
// 国际版带 `qoder-app://`，国服**不传**（官方 app.asar 里是 null）。
// 此前这条把它列进"必须有"，是因为旧实现两区都硬编码了
// `qoder-work-cn://` —— 一个本机从未注册的协议。
// 「两区都有」这个共同点掩盖了「两区都错」。
func TestLoginStartBuildsAuthURL(t *testing.T) {
	t.Setenv(oauthEnvRedirectURI, "")
	for _, region := range []Region{RegionCN, RegionIntl} {
		s, err := LoginStart(region)
		if err != nil {
			t.Fatalf("%s 发起登录失败: %v", region, err)
		}
		if !strings.HasPrefix(s.AuthURL, region.Website()+"/device/selectAccounts?") {
			t.Errorf("%s 授权链接前缀错误: %s", region, s.AuthURL)
		}
		// 两区**都**必须有的参数（redirect_uri 不在其中，见函数注释）
		for _, must := range []string{"challenge=", "challenge_method=S256", "nonce=", "machine_id=", "client_id="} {
			if !strings.Contains(s.AuthURL, must) {
				t.Errorf("%s 授权链接缺少 %s：%s", region, must, s.AuthURL)
			}
		}
		if s.Verifier == "" || s.Nonce == "" || s.MachineID == "" {
			t.Errorf("%s 会话字段不应为空", region)
		}
		// PKCE verifier 长度：参考实现是 64 字符（按字节取模 66 生成）
		if len(s.Verifier) != 64 {
			t.Errorf("%s verifier 应为 64 字符，实际 %d", region, len(s.Verifier))
		}
	}
}

// TestLoginStartUsesOfficialClientID 授权 URL 必须用**官方客户端的** client_id。
//
// # ⚠ 这条测试的前身是错的，值得记下来（2026-09-22）
//
// 旧版本叫 `TestLoginStartUsesSameClientIDForBothRegions`，注释写着
// 「实测结论（两区授权页都 302 进登录页）」—— 那个"实测"**方法是错的**：
// `/device/selectAccounts` 对**任何** client_id 都回 302 进登录页，
// **不做校验**。真正的校验在用户点「确认授权」那一刻。
//
// 于是那条测试**只能证明"两区用了同一个字符串"**，
// 完全证明不了"这个字符串是对的"—— 它把两个错值都放行了。
//
// 现在改为对着**官方 app.asar 里读出的真实值**断言：
//
//	%LOCALAPPDATA%\Programs\Qoder\resources\app.asar      （国际版）
//	%LOCALAPPDATA%\Programs\Qoder CN\resources\app.asar   （国服）
//	    authClientIds: { prod: "732aef47-…", test: "732aef47-…" }
//
// ⚠ 断言的是**常量**而不是 URL 里有没有某个字符串：
// 常量错了这条就红，而不是等用户在浏览器点确认才发现。
func TestLoginStartUsesOfficialClientID(t *testing.T) {
	const official = "732aef47-9cf2-46a2-95fe-4cebb5d0d1fa"
	if oauthClientIDDefault != official {
		t.Errorf("client_id 必须是官方客户端的值 %q（从 app.asar 读出），实际 %q",
			official, oauthClientIDDefault)
	}
	// 环境变量未设时，生成的 URL 里必须是这个值。
	t.Setenv(oauthEnvClientID, "")
	cn, _ := LoginStart(RegionCN)
	intl, _ := LoginStart(RegionIntl)
	for _, c := range []*LoginSession{cn, intl} {
		if !strings.Contains(c.AuthURL, official) {
			t.Errorf("授权 URL 应含官方 client_id %q，实际：%s", official, c.AuthURL)
		}
		if strings.Contains(c.AuthURL, "1c5e33e1") {
			t.Errorf("授权 URL 仍含**旧的错值** 1c5e33e1…（那正是国际版登录"+
				"报「参数无效」的原因）：%s", c.AuthURL)
		}
	}
}

// TestRedirectURIMatchesOfficialPerRegion redirect_uri 必须与官方**按区域**一致。
//
// 官方两个客户端在这里**不同**（从各自 app.asar 读出）：
//
//	国际版  authRedirectUris: { stable: "qoder-app://" }
//	国服    authRedirectUris: { stable: null          }   ← 不传
//
// ⚠ 旧代码两区都用 `qoder-work-cn://` —— 一个**本机从未注册**的协议
//（实测 HKCR 只有 `qoder` 与 `qoder-cn`）。
func TestRedirectURIMatchesOfficialPerRegion(t *testing.T) {
	t.Setenv(oauthEnvRedirectURI, "")

	intl, _ := LoginStart(RegionIntl)
	if !strings.Contains(intl.AuthURL, "redirect_uri=qoder-app") {
		t.Errorf("国际版应带 redirect_uri=qoder-app://，实际：%s", intl.AuthURL)
	}

	cn, _ := LoginStart(RegionCN)
	if strings.Contains(cn.AuthURL, "redirect_uri=") {
		t.Errorf("国服**不应**带 redirect_uri 参数（官方为 null）：%s", cn.AuthURL)
	}
	// 两区都不该再出现那个未注册的旧协议
	for _, c := range []*LoginSession{cn, intl} {
		if strings.Contains(c.AuthURL, "qoder-work-cn") {
			t.Errorf("授权 URL 仍含未注册的旧协议 qoder-work-cn：%s", c.AuthURL)
		}
	}
}

// TestClientIDEnvOverride 环境变量可覆盖 client_id（对齐官方客户端行为）。
//
// 官方实现里 `QODER_AUTH_CLIENT_ID` 优先 —— 保留这个能力，
// 将来上游改 client_id 时不必重新打包就能热修。
func TestClientIDEnvOverride(t *testing.T) {
	t.Setenv(oauthEnvClientID, "11111111-2222-4333-8444-555555555555")
	got := clientID()
	if got != "11111111-2222-4333-8444-555555555555" {
		t.Errorf("环境变量应覆盖 client_id，实际 %q", got)
	}
	s, _ := LoginStart(RegionIntl)
	if !strings.Contains(s.AuthURL, "11111111-2222-4333-8444-555555555555") {
		t.Errorf("覆盖后的值应出现在授权 URL 里：%s", s.AuthURL)
	}
}

// TestLoginPollPending 未授权时必须返回 LoginPending 而不是普通错误。
//
// 区分二者是刻意的：界面据此显示"请继续在浏览器完成授权"而不是"登录失败"。
func TestLoginPollPending(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusNotFound) // 上游用 404 表示"还没授权"
	}))
	defer srv.Close()

	s := &LoginSession{Verifier: "v", Nonce: "n", MachineID: "m", Region: RegionCN}
	hc := srv.Client()
	hc.Transport = rewriteTransport{base: srv.Client().Transport, target: srv.URL}
	_, err := LoginPoll(context.Background(), hc, s, t.TempDir())
	if err == nil {
		t.Fatal("未授权时应返回错误")
	}
	if _, ok := err.(LoginPending); !ok {
		t.Errorf("未授权时应返回 LoginPending，实际 %T: %v", err, err)
	}
}

// TestLoginPollSuccess 授权成功后应产出凭证并落盘。
//
// ⚠ 必须用 rewriteTransport 把请求重定向到测试服务器：LoginPoll 打的是
// Region.OpenAPI()（真实域名），直接传 srv.Client() 不会命中测试服务器 ——
// 请求会真的发到 qoder.com.cn，然后因为非 404/202 的响应或网络错误而失败。
// 我第一次就是这么写的，测试报"授权尚未完成"，看起来像产品缺陷，其实是测试没接上。
func TestLoginPollSuccess(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		// 顺带断言轮询路径正确（这是协议的一部分，错了上游会 404）
		if !strings.Contains(r.URL.Path, "/api/v1/deviceToken/poll") {
			t.Errorf("轮询应打 deviceToken/poll，实际 %s", r.URL.Path)
		}
		for _, q := range []string{"nonce=", "verifier=", "challenge_method=S256"} {
			if !strings.Contains(r.URL.RawQuery, q) {
				t.Errorf("轮询查询串缺少 %s：%s", q, r.URL.RawQuery)
			}
		}
		_ = json.NewEncoder(w).Encode(map[string]any{
			"token":         "dt-new",
			"refresh_token": "drt-new",
			"user_id":       "uid-login",
			"expires_in":    int64(3600 * 1000),
		})
	}))
	defer srv.Close()

	dir := t.TempDir()
	s := &LoginSession{Verifier: "v", Nonce: "n", MachineID: "machine-1", Region: RegionCN}
	hc := srv.Client()
	hc.Transport = rewriteTransport{base: srv.Client().Transport, target: srv.URL}

	c, err := LoginPoll(context.Background(), hc, s, dir)
	if err != nil {
		t.Fatalf("授权成功却报错: %v", err)
	}
	if c.DT != "dt-new" || c.DRT != "drt-new" || c.UID != "uid-login" {
		t.Errorf("凭证字段错误: DT=%q DRT=%q UID=%q", c.DT, c.DRT, c.UID)
	}
	if c.MachineID != "machine-1" {
		t.Errorf("机器指纹应来自会话，实际 %q", c.MachineID)
	}
	if c.MachineToken == "" || c.MachineType == "" {
		t.Error("登录后应补齐机器指纹的其余两项")
	}
	// 必须落盘，且文件名能被 LoadDir 的 glob 命中
	if c.FilePath == "" {
		t.Fatal("应回填 FilePath")
	}
	if !strings.HasPrefix(filepath.Base(c.FilePath), "qoder-") {
		t.Errorf("文件名应以 qoder- 开头（与 LoadDir 的 glob 一致），实际 %s", filepath.Base(c.FilePath))
	}
	if _, err := os.Stat(c.FilePath); err != nil {
		t.Errorf("凭证应已落盘: %v", err)
	}
	// 落盘内容应能被自己读回（往返一致性）
	back, err := LoadFile(c.FilePath)
	if err != nil {
		t.Fatalf("读回落盘的凭证失败: %v", err)
	}
	if back.DT != c.DT || back.UID != c.UID {
		t.Error("往返读写不一致")
	}
}

// TestLoginPollMissingUserIDFails 授权成功但缺 user_id 必须报错。
//
// 没有 uid 就无法确定账号标识，静默接受会产生一个"无名账号"。
func TestLoginPollMissingUserIDFails(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_ = json.NewEncoder(w).Encode(map[string]any{"token": "dt-x"})
	}))
	defer srv.Close()

	s := &LoginSession{Verifier: "v", Nonce: "n", MachineID: "m", Region: RegionCN}
	hc := srv.Client()
	hc.Transport = rewriteTransport{base: srv.Client().Transport, target: srv.URL}
	if _, err := LoginPoll(context.Background(), hc, s, t.TempDir()); err == nil {
		t.Error("缺 user_id 时应报错")
	}
}

// TestRefreshTokenRotatesDRTAndPersists DRT 必须轮换并落盘。
//
// 这是最容易被忽略、后果最严重的一条：只更新内存不落盘的话，
// 重启后读到的是**已失效的旧 DRT**，表现为"重启就掉登录"。
func TestRefreshTokenRotatesDRTAndPersists(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if !strings.HasSuffix(r.URL.Path, "/api/v1/deviceToken/refresh") {
			t.Errorf("刷新应打 deviceToken/refresh，实际 %s", r.URL.Path)
		}
		_ = json.NewEncoder(w).Encode(map[string]any{
			"token":         "dt-refreshed",
			"refresh_token": "drt-rotated",
			"expires_in":    int64(7200 * 1000),
		})
	}))
	defer srv.Close()

	dir := t.TempDir()
	path := filepath.Join(dir, "qoder-uid-refresh.json")
	c := &Cred{
		UID: "uid-refresh", DT: "dt-old", DRT: "drt-old",
		MachineID: "m", MachineToken: "t", MachineType: "5",
		Region: RegionCN, FilePath: path,
	}
	// 先落一次盘，模拟"已存在的凭证文件"
	if err := c.SaveAtomic(); err != nil {
		t.Fatal(err)
	}

	// 让 Region 指向测试服务器：直接改 OpenAPI 不可行（它由 Region 决定），
	// 故这里验证的是**逻辑**部分（轮换 + 落盘），网络部分由上面的 handler 断言路径。
	// 用一个自定义 transport 把请求重定向到测试服务器。
	c.Region = RegionCN
	hc := srv.Client()
	hc.Transport = rewriteTransport{base: srv.Client().Transport, target: srv.URL}

	before := time.Now()
	if err := c.RefreshToken(context.Background(), hc); err != nil {
		t.Fatalf("刷新失败: %v", err)
	}
	if c.DT != "dt-refreshed" {
		t.Errorf("DT 应更新，实际 %q", c.DT)
	}
	if c.DRT != "drt-rotated" {
		t.Errorf("DRT 应轮换，实际 %q", c.DRT)
	}
	if c.DTExpiresAt <= before.Unix() {
		t.Errorf("到期时间应被更新为将来，实际 %d", c.DTExpiresAt)
	}

	// 关键：重新从磁盘读，DRT 必须是新的
	back, err := LoadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if back.DRT != "drt-rotated" {
		t.Errorf("磁盘上的 DRT 应为轮换后的值，实际 %q —— "+
			"没落盘会导致重启后登录失效", back.DRT)
	}
}

// TestRefreshTokenInvalidReturnsAuthInvalid 上游拒绝刷新时给出可识别的错误。
//
// 调用方据此把账号标成"需重新登录"，而不是反复重试。
func TestRefreshTokenInvalidReturnsAuthInvalid(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusUnauthorized)
		_, _ = w.Write([]byte(`{"code":"105","message":"Login expired"}`))
	}))
	defer srv.Close()

	c := &Cred{UID: "u", DT: "dt", DRT: "drt", MachineID: "m", MachineToken: "t", MachineType: "5", Region: RegionCN}
	hc := srv.Client()
	hc.Transport = rewriteTransport{base: srv.Client().Transport, target: srv.URL}

	err := c.RefreshToken(context.Background(), hc)
	if err == nil {
		t.Fatal("401 时应报错")
	}
	if _, ok := err.(*AuthInvalidError); !ok {
		t.Errorf("401 应返回 AuthInvalidError（供上层标记需重登），实际 %T: %v", err, err)
	}
}

// TestNeedsRefresh 刷新时机判定。
//
// ⚠ 表里用 **指针** 而不是值：Cred 内含 sync.Mutex（串行化刷新与写回），
// 按值拷贝会让 vet 报 "range var copies lock" —— 而且真拷了锁之后，
// 并发场景下的行为会变得难以预料。用指针既消除告警也符合真实用法。
func TestNeedsRefresh(t *testing.T) {
	now := time.Now().Unix()
	cases := []struct {
		name string
		c    *Cred
		want bool
		why  string
	}{
		{"无 DT", &Cred{DTExpiresAt: now + 86400}, true, "没有令牌必须刷新"},
		{"过期时间未知", &Cred{DT: "x", DTExpiresAt: 0}, true, "未知时宁可按需刷新一次"},
		{"已过期", &Cred{DT: "x", DTExpiresAt: now - 10}, true, "已过期"},
		{"即将过期", &Cred{DT: "x", DTExpiresAt: now + 600}, true, "落在提前刷新窗口内"},
		{"还很久", &Cred{DT: "x", DTExpiresAt: now + 86400}, false, "无需刷新"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if got := tc.c.NeedsRefresh(refreshSkew); got != tc.want {
				t.Errorf("NeedsRefresh = %v，期望 %v（%s）", got, tc.want, tc.why)
			}
		})
	}
}

// TestEnsureFingerprintStable 已有指纹时不得改动（跨重启必须稳定）。
func TestEnsureFingerprintStable(t *testing.T) {
	c := &Cred{MachineID: "fixed-id", MachineToken: "fixed-token", MachineType: "5"}
	if changed := c.EnsureFingerprint(); changed {
		t.Error("指纹齐全时不应报告改动 —— 每次重启换指纹会让上游看到'多设备登录'")
	}
	if c.MachineID != "fixed-id" || c.MachineToken != "fixed-token" {
		t.Error("已有指纹不得被覆盖")
	}

	// 缺失时应补齐并报告改动
	empty := &Cred{}
	if changed := empty.EnsureFingerprint(); !changed {
		t.Error("指纹缺失时应报告改动（调用方需要落盘）")
	}
	if empty.MachineID == "" || empty.MachineToken == "" || empty.MachineType == "" {
		t.Error("应补齐三项指纹")
	}
}

// TestNormalizeModelName 模型名规范化。
func TestNormalizeModelName(t *testing.T) {
	cases := map[string]string{
		"Qwen3.8-Max-Preview": "qwen3.8-max-preview",
		"DeepSeek-V4-Pro":     "deepseek-v4-pro",
		"GLM-5.2":             "glm-5.2",
		"  Spaces  Around  ":  "spaces-around",
		"Under_Score":         "under-score",
		"Double--Dash":        "double-dash",
		"-Leading":            "leading",
		"Trailing-":           "trailing",
	}
	for in, want := range cases {
		if got := NormalizeModelName(in); got != want {
			t.Errorf("NormalizeModelName(%q) = %q，期望 %q", in, got, want)
		}
	}
}

// TestLoadDirReportsFailures LoadDir 必须把解析失败的文件报出来。
//
// 与 auth.LoadDir（静默跳过）不同：Qoder 的凭证可能是用户手写导入的，
// 格式错误必须可见，否则用户会以为"导入成功了但账号没出现"。
func TestLoadDirReportsFailures(t *testing.T) {
	dir := t.TempDir()
	// 一个好文件
	good := `{"auth":{"accessToken":"dt-ok","refreshToken":"drt-ok","domain":"qoder.com.cn"},"account":{"uid":"uid-ok"}}`
	if err := os.WriteFile(filepath.Join(dir, "qoder-uid-ok.json"), []byte(good), 0o600); err != nil {
		t.Fatal(err)
	}
	// 一个坏文件
	if err := os.WriteFile(filepath.Join(dir, "qoder-bad.json"), []byte(`{坏`), 0o600); err != nil {
		t.Fatal(err)
	}
	// 一个无关文件（不应被 glob 命中）
	if err := os.WriteFile(filepath.Join(dir, "workbuddy-x.json"), []byte(`{}`), 0o600); err != nil {
		t.Fatal(err)
	}

	creds, failed, err := LoadDir(dir)
	if err != nil {
		t.Fatal(err)
	}
	if len(creds) != 1 || creds[0].UID != "uid-ok" {
		t.Errorf("应解析出 1 个凭证（uid-ok），实际 %d 个", len(creds))
	}
	if len(failed) != 1 {
		t.Errorf("应报告 1 个失败文件，实际 %d 个: %v", len(failed), failed)
	}
	if len(failed) == 1 && !strings.Contains(failed[0], "qoder-bad.json") {
		t.Errorf("失败信息应指明是哪个文件，实际 %q", failed[0])
	}
}

// TestSanitizeUIDPreventsPathTraversal uid 里的路径字符必须被清理。
//
// 安全项：uid 来自上游响应，若直接拼进文件名，恶意/异常的 uid
// （如 "../../etc/passwd"）会写到目录外。
func TestSanitizeUIDPreventsPathTraversal(t *testing.T) {
	cases := map[string]string{
		// "../../evil" = 4 个字符（.. / .. / / / e...）：前三段各变一个下划线
		// → ".." → "__"、".." → "__"、"/" → "_"，再加 "evil" = "____evil"
		"../../evil": "____evil",
		"a/b\\c":     "a_b_c",
		"normal-uid": "normal-uid",
		"":           "unknown",
		"with:colon": "with_colon",
	}
	for in, want := range cases {
		if got := sanitizeUID(in); got != want {
			t.Errorf("sanitizeUID(%q) = %q，期望 %q", in, got, want)
		}
	}
	// 关键：结果里不能有路径分隔符或 ..
	got := sanitizeUID("../../etc/passwd")
	if strings.Contains(got, "/") || strings.Contains(got, "\\") || strings.Contains(got, "..") {
		t.Errorf("清理后的 uid 仍含路径字符: %q", got)
	}
}

// rewriteTransport 把请求重定向到测试服务器（保留原路径）。
type rewriteTransport struct {
	base   http.RoundTripper
	target string
}

func (t rewriteTransport) RoundTrip(req *http.Request) (*http.Response, error) {
	u, err := req.URL.Parse(t.target)
	if err != nil {
		return nil, err
	}
	// 只换 scheme/host，保留 path 与 query（这样能验证请求路径）
	req.URL.Scheme = u.Scheme
	req.URL.Host = u.Host
	base := t.base
	if base == nil {
		base = http.DefaultTransport
	}
	return base.RoundTrip(req)
}
