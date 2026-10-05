package zcode

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"net/http/httptest"
	"sync"
	"sync/atomic"
	"testing"
)

// warmUpstream 一个假的上游，同时服务激活事件与领取两个端点。
//
// 记录每一次请求的路径与头，让测试能断言"发了什么、没发什么"——
// 本功能的风控口径**全靠头与 body 的形状**，只断言"成功了"是不够的。
type warmUpstream struct {
	mu sync.Mutex
	// 按顺序记录收到的路径。
	paths []string
	// 每次事件上报收到的 body（原样），按顺序。
	eventBodies [][]byte
	// 每次事件上报收到的 Authorization 头（空串 = 没发）。
	eventAuth []string
	// 每次领取收到的 param（从 X-Aliyun-Captcha-Verify-Param 头读）。
	claimParams []string
	// 每次领取收到的 body。
	claimBodies [][]byte

	eventStatus int    // 非 0 时事件端点返回它
	eventBody   string // 事件端点的响应体

	// claimCodes 按调用次序决定第 N 次领取的响应；用尽后回 success。
	claimCodes []string
	claimHits  int32
}

func (u *warmUpstream) handler() http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body := make([]byte, 0)
		if r.Body != nil {
			buf := make([]byte, 1<<16)
			for {
				n, err := r.Body.Read(buf)
				if n > 0 {
					body = append(body, buf[:n]...)
				}
				if err != nil {
					break
				}
			}
		}

		u.mu.Lock()
		u.paths = append(u.paths, r.URL.Path)
		u.mu.Unlock()

		switch {
		case hasSub(r.URL.Path, "/event/report"):
			u.mu.Lock()
			u.eventBodies = append(u.eventBodies, body)
			u.eventAuth = append(u.eventAuth, r.Header.Get("Authorization"))
			u.mu.Unlock()
			if u.eventStatus != 0 {
				w.WriteHeader(u.eventStatus)
				_, _ = w.Write([]byte(u.eventBody))
				return
			}
			w.Header().Set("Content-Type", "application/json")
			_, _ = w.Write([]byte(`{"code":0}`))

		case hasSub(r.URL.Path, "/billing/preview"):
			w.Header().Set("Content-Type", "application/json")
			_, _ = w.Write([]byte(`{"code":0,"data":{"server_time":1,"plans":[
				{"plan_id":"low","name":"low-plan","priority":1,"status":"claimable"},
				{"plan_id":"high","name":"high-plan","priority":9,"status":"claimable"}]}}`))

		case hasSub(r.URL.Path, "/billing/claim"):
			n := atomic.AddInt32(&u.claimHits, 1)
			u.mu.Lock()
			u.claimParams = append(u.claimParams, r.Header.Get("X-Aliyun-Captcha-Verify-Param"))
			u.claimBodies = append(u.claimBodies, body)
			idx := int(n) - 1
			var code string
			if idx < len(u.claimCodes) {
				code = u.claimCodes[idx]
			}
			u.mu.Unlock()

			w.Header().Set("Content-Type", "application/json")
			if code == "" || code == "0" {
				_, _ = w.Write([]byte(`{"code":0,"data":{"claimed_at":123}}`))
				return
			}
			// 非 0 业务码：上游用 HTTP 200 + 业务码表达（实测形状）。
			_, _ = w.Write([]byte(fmt.Sprintf(`{"code":%s,"msg":"biz %s"}`, code, code)))

		default:
			http.Error(w, "unexpected: "+r.URL.Path, 404)
		}
	})
}

// newWarmTestClient 造一个指向假上游、且验证码可注入的客户端。
func newWarmTestClient(t *testing.T, u *warmUpstream, mint func(context.Context) (string, string, error)) (*Client, *httptest.Server) {
	t.Helper()
	srv := httptest.NewServer(u.handler())
	t.Cleanup(srv.Close)

	c := New()
	c.HTTP = srv.Client()
	// 清空 Identity：测试不该依赖跑测试那台机器的语言/时区/平台。
	c.Identity = Identity{AppVersion: "3.14.0", Platform: "windows", Arch: "amd64"}
	c.SetEventBaseForTest(srv.URL)
	c.SetClaimBaseForTest(srv.URL)
	c.SetCaptchaMinterForTest(mint)
	return c, srv
}

// warmCred 一个 deviceMid 合法（避免触发落盘）的凭证。
func warmCred(uid, jwt string) *Cred {
	return &Cred{
		UID:       uid,
		JWT:       jwt,
		DeviceMid: "11111111-2222-4333-8444-555555555555",
		Provider:  ProviderZAI,
	}
}

// jwtWithSub 造一个只带 sub 的 JWT（不验签，故签名段可以是任意串）。
func jwtWithSub(sub string) string {
	payload, _ := json.Marshal(map[string]any{"sub": sub})
	return "eyJhbGciOiJIUzI1NiJ9." + b64url(payload) + ".sig"
}

func b64url(b []byte) string {
	const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
	var out []byte
	for i := 0; i < len(b); i += 3 {
		var chunk [3]byte
		n := copy(chunk[:], b[i:])
		out = append(out, alphabet[chunk[0]>>2])
		out = append(out, alphabet[(chunk[0]&0x03)<<4|chunk[1]>>4])
		if n > 1 {
			out = append(out, alphabet[(chunk[1]&0x0F)<<2|chunk[2]>>6])
		} else {
			out = append(out, '=')
		}
		if n > 2 {
			out = append(out, alphabet[chunk[2]&0x3F])
		} else {
			out = append(out, '=')
		}
	}
	return string(out)
}

// TestActivationEventHasExactlyTheSixteenOfficialFields 事件 body 就是官方
// sendReport 的 16 个字段，一个不多一个不少。
//
// 这条测试的价值在于**漏字段**：少一个字段本身就是"与真机不一致"的可区分
// 特征，而上游不会因为少一个字段就报错（它只会安静地少记一件事），
// 所以只有逐字段断言才守得住。
func TestActivationEventHasExactlyTheSixteenOfficialFields(t *testing.T) {
	c := New()
	c.Identity = Identity{
		AppVersion:     "3.14.0",
		Platform:       "windows",
		ClientLanguage: "zh-CN",
		ClientTimezone: "Asia/Shanghai",
	}

	raw, err := json.Marshal(c.buildActivationEvent("app_launch", "u-1", "mid-1"))
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}

	var got map[string]any
	if err := json.Unmarshal(raw, &got); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}

	want := []string{
		"event_id", "client_timezone", "client_language", "element_name",
		"event_region", "event_type", "event_text", "event_extra_detail",
		"user_id", "screen_resolution", "app_version", "device_os_category",
		"device_os_version", "device_mid", "mac_id", "marketing_params",
	}
	if len(got) != len(want) {
		t.Fatalf("字段数 %d，want %d\n实际键：%v", len(got), len(want), sortedKeys(got))
	}
	for _, k := range want {
		if _, ok := got[k]; !ok {
			t.Errorf("缺字段 %q", k)
		}
	}

	// 值也要对（这几个是官方固定值，改动会变成可区分特征）。
	fixed := map[string]any{
		"element_name":       "app_launch",
		"event_region":       "app",
		"event_type":         "view",
		"event_text":         "",
		"user_id":            "u-1",
		"screen_resolution":  "1920x1080",
		"app_version":        "3.14.0",
		"device_os_category": "windows",
		"device_mid":         "mid-1",
		"mac_id":             "",
	}
	for k, v := range fixed {
		if got[k] != v {
			t.Errorf("%s = %#v，want %#v", k, got[k], v)
		}
	}
}

// TestActivationEventKeepsTheOfficialTypeSplit 两个"空对象"的**类型不同**，
// 不要顺手统一。
//
// `event_extra_detail` 是对象 `{}`，`marketing_params` 是字符串 `"{}"`。
// 这条最初是照抄参考实现时最容易被"整理"掉的一处：两个字段看起来都是空
// 对象，统一成同一种类型后 JSON 仍然合法、Go 仍然编译、上游也不报错 ——
// 但 body 形状就与真机不同了。
func TestActivationEventKeepsTheOfficialTypeSplit(t *testing.T) {
	c := New()
	c.Identity = Identity{AppVersion: "3.14.0", Platform: "windows"}

	raw, err := json.Marshal(c.buildActivationEvent("app_launch", "u", "m"))
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	var got map[string]any
	if err := json.Unmarshal(raw, &got); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}

	if _, ok := got["event_extra_detail"].(map[string]any); !ok {
		t.Errorf("event_extra_detail 应是对象，得到 %T", got["event_extra_detail"])
	}
	if s, ok := got["marketing_params"].(string); !ok || s != "{}" {
		t.Errorf(`marketing_params 应是字符串 "{}"，得到 %T(%v)`,
			got["marketing_params"], got["marketing_params"])
	}
}

// TestActivationEventUsesOurOwnIdentityLocale 语言/时区用**我们自己**声明
// 的那一对，而不是参考实现硬编码的 en-US / America/New_York。
//
// 参考实现的注释写明了它遵循的规则：「一个真实的区域对，且与 billing 头
// 声明的是同一对」——它警告的是**同一个账号不能出现两个区域对**。我们的
// 对话路径声明的是 zh-CN / Asia/Shanghai，遥测若报 en-US 就正好犯了那件事。
func TestActivationEventUsesOurOwnIdentityLocale(t *testing.T) {
	c := New()
	c.Identity = Identity{
		AppVersion:     "3.14.0",
		Platform:       "windows",
		ClientLanguage: "zh-CN",
		ClientTimezone: "Asia/Shanghai",
	}
	ev := c.buildActivationEvent("app_launch", "u", "m")
	if ev.ClientLanguage != "zh-CN" || ev.ClientTimezone != "Asia/Shanghai" {
		t.Errorf("语言/时区 = %q/%q，want zh-CN/Asia/Shanghai（要与 Identity 声明的一致）",
			ev.ClientLanguage, ev.ClientTimezone)
	}

	// 对照：Identity 没给值时走兜底，而兜底也必须与 Headers() 的兜底一致。
	c2 := New()
	c2.Identity = Identity{AppVersion: "3.14.0", Platform: "windows"}
	ev2 := c2.buildActivationEvent("app_launch", "u", "m")
	if ev2.ClientLanguage != "zh-CN" || ev2.ClientTimezone != "Asia/Shanghai" {
		t.Errorf("兜底 = %q/%q，want zh-CN/Asia/Shanghai（要与 Headers() 的 firstNonEmpty 兜底一致）",
			ev2.ClientLanguage, ev2.ClientTimezone)
	}
}

// TestPostEventSendsNoAuthorization 事件上报**不带认证头**。
//
// 这条路径的身份完全由 body 里的 device_mid 表达。多送一个认证头不只是
// 多余 —— 它是"与真机不一致"的可区分特征（与 Identity.ClaimHeaders 的
// 取舍完全同源）。
func TestPostEventSendsNoAuthorization(t *testing.T) {
	u := &warmUpstream{}
	c, _ := newWarmTestClient(t, u, nil)

	cr := warmCred("zcode-x", "jwt-abc")
	if err := c.postEvent(context.Background(), cr, "app_launch", "u-1", cr.DeviceMid); err != nil {
		t.Fatalf("postEvent: %v", err)
	}

	u.mu.Lock()
	defer u.mu.Unlock()
	if len(u.eventAuth) != 1 {
		t.Fatalf("事件请求数 %d，want 1", len(u.eventAuth))
	}
	if u.eventAuth[0] != "" {
		t.Errorf("事件带了 Authorization=%q —— 官方客户端这条路径是匿名的", u.eventAuth[0])
	}
	if len(u.eventBodies) != 1 {
		t.Fatalf("事件 body 数 %d，want 1", len(u.eventBodies))
	}
	var got map[string]any
	if err := json.Unmarshal(u.eventBodies[0], &got); err != nil {
		t.Fatalf("事件 body 不是 JSON: %v", err)
	}
	if got["device_mid"] != cr.DeviceMid {
		t.Errorf("device_mid = %v，want %q（这条路径的**唯一身份**）", got["device_mid"], cr.DeviceMid)
	}
}

// TestWarmAccountWithoutUserIDSkipsEverything JWT 里没有 user_id/sub 时，
// 只如实说明原因，**不报错、也不去领取**。
//
// 事件 body 里的 user_id 是必需的身份字段；发不出去就领套餐，等于在一个
// 上游看来"从未活动过"的账号上做写操作 —— 那正是最容易被判异常的时序。
func TestWarmAccountWithoutUserIDSkipsEverything(t *testing.T) {
	u := &warmUpstream{}
	c, _ := newWarmTestClient(t, u, nil)

	// 不带 user_id/sub 的 JWT：payload 是 {}。
	cr := warmCred("zcode-x", "eyJhbGciOiJIUzI1NiJ9.e30.sig")
	rep, err := c.WarmAccount(context.Background(), cr)
	if err != nil {
		t.Fatalf("WarmAccount 不该报错（上游业务状态不是函数错误）: %v", err)
	}
	if rep.Events == "" {
		t.Error("Events 应说明跳过原因（空串 = 成功，会让界面以为激活成功了）")
	}
	if rep.Skipped == "" {
		t.Error("Skipped 应说明未执行领取")
	}
	if atomic.LoadInt32(&u.claimHits) != 0 {
		t.Error("不该尝试领取")
	}
	u.mu.Lock()
	defer u.mu.Unlock()
	if len(u.eventBodies) != 0 {
		t.Errorf("不该上报事件，实际发了 %d 个", len(u.eventBodies))
	}
}

// TestWarmAccountStopsBeforeClaimWhenEventFails 事件失败 ⇒ **不领取**。
//
// 与参考实现一致：激活事件是"让账号看起来活过"的前提，领取是它的收益。
// 事件没发出去就领，是最容易被判异常的时序。
func TestWarmAccountStopsBeforeClaimWhenEventFails(t *testing.T) {
	u := &warmUpstream{eventStatus: 500, eventBody: `{"code":500}`}
	c, _ := newWarmTestClient(t, u, nil)

	cr := warmCred("zcode-x", jwtWithSub("u-1"))
	rep, err := c.WarmAccount(context.Background(), cr)
	if err != nil {
		t.Fatalf("WarmAccount: %v", err)
	}
	if rep.Events == "" {
		t.Error("Events 应记下失败原因")
	}
	if rep.Skipped == "" {
		t.Error("Skipped 应说明未执行领取")
	}
	if atomic.LoadInt32(&u.claimHits) != 0 {
		t.Errorf("事件失败后不该领取，实际领了 %d 次", atomic.LoadInt32(&u.claimHits))
	}

	u.mu.Lock()
	defer u.mu.Unlock()
	// 第一个 element 就失败 ⇒ break，第二个不该发。
	if len(u.eventBodies) != 1 {
		t.Errorf("事件请求数 %d，want 1（任一失败即停，不是继续发下一个）", len(u.eventBodies))
	}
}

// TestWarmAccountSendsBothEventsInOrderThenClaims 两个事件**按顺序**发完，
// 然后领上游给出的每一个套餐。
func TestWarmAccountSendsBothEventsInOrderThenClaims(t *testing.T) {
	u := &warmUpstream{}
	c, _ := newWarmTestClient(t, u, func(context.Context) (string, string, error) {
		return "param-x", "cn", nil
	})

	cr := warmCred("zcode-x", jwtWithSub("u-1"))
	rep, err := c.WarmAccount(context.Background(), cr)
	if err != nil {
		t.Fatalf("WarmAccount: %v", err)
	}
	if rep.Events != "" {
		t.Errorf("Events = %q，want 空串（空 = 成功）", rep.Events)
	}
	if rep.Claim != "" {
		t.Errorf("Claim = %q，want 空串", rep.Claim)
	}

	// 顺序：app_launch → app_daily_active → preview → claim×2。
	u.mu.Lock()
	defer u.mu.Unlock()
	var elements []string
	for _, b := range u.eventBodies {
		var m map[string]any
		_ = json.Unmarshal(b, &m)
		elements = append(elements, fmt.Sprint(m["element_name"]))
	}
	if len(elements) != 2 || elements[0] != "app_launch" || elements[1] != "app_daily_active" {
		t.Errorf("事件顺序 = %v，want [app_launch app_daily_active]", elements)
	}

	// 领取顺序：priority 9 的 high 必须在 priority 1 的 low **之前**。
	if len(u.claimBodies) != 2 {
		t.Fatalf("领取次数 %d，want 2（上游给出两个套餐就领两个）", len(u.claimBodies))
	}
	first := string(u.claimBodies[0])
	if !containsStr(first, "high") {
		t.Errorf("第一次领取的是 %s，want priority 最高的 high", first)
	}
	if len(rep.Claimed) != 2 {
		t.Errorf("Claimed = %v，want 两个", rep.Claimed)
	}
}

// TestWarmAccountRetriesCaptchaWithAFreshParam 吃到 3007 ⇒ 换一个**新** param
// 再试一次（同一个 param 用第二次上游必回 3007，重发它毫无意义）。
func TestWarmAccountRetriesCaptchaWithAFreshParam(t *testing.T) {
	// 前两次领取都回 3007（第一个套餐的两次尝试），之后成功。
	u := &warmUpstream{claimCodes: []string{"3007", "3007"}}
	var n int32
	c, _ := newWarmTestClient(t, u, func(context.Context) (string, string, error) {
		return fmt.Sprintf("param-%d", atomic.AddInt32(&n, 1)), "cn", nil
	})

	cr := warmCred("zcode-x", jwtWithSub("u-1"))
	rep, err := c.WarmAccount(context.Background(), cr)
	if err != nil {
		t.Fatalf("WarmAccount: %v", err)
	}

	u.mu.Lock()
	params := append([]string(nil), u.claimParams...)
	u.mu.Unlock()

	if len(params) < 2 {
		t.Fatalf("领取次数 %d，want ≥2（3007 要重试一次）", len(params))
	}
	if params[0] == "" || params[1] == "" {
		t.Errorf("两次领取都应带 param，得到 %q / %q", params[0], params[1])
	}
	if params[0] == params[1] {
		t.Errorf("重试用了**同一个** param %q —— 它是一次性的，第二次必被拒", params[0])
	}
	// 两次都 3007 ⇒ 该套餐报"连续两次被拒"，但整个激活不该返回 error。
	if rep.Claim == "" {
		t.Error("Claim 应记下连续两次被拒")
	}
}

// TestClaimWithCaptchaDoesNotMutateCallerCred 验证码 param 是**一次性**的，
// 不该留在调用方的 Cred 上（那会让后续别的调用误以为它还能用）。
func TestClaimWithCaptchaDoesNotMutateCallerCred(t *testing.T) {
	u := &warmUpstream{}
	c, _ := newWarmTestClient(t, u, func(context.Context) (string, string, error) {
		return "param-once", "cn", nil
	})

	cr := warmCred("zcode-x", jwtWithSub("u-1"))
	cr.CaptchaParam = "preexisting"
	cr.CaptchaRegion = "sgp"

	if _, err := c.claimWithCaptcha(context.Background(), cr, "p1"); err != nil {
		t.Fatalf("claimWithCaptcha: %v", err)
	}
	if cr.CaptchaParam != "preexisting" || cr.CaptchaRegion != "sgp" {
		t.Errorf("调用方的 Cred 被改动了：param=%q region=%q",
			cr.CaptchaParam, cr.CaptchaRegion)
	}

	// 而请求里带的应该是**新铸的那个**，不是调用方原有的。
	u.mu.Lock()
	defer u.mu.Unlock()
	if len(u.claimParams) != 1 || u.claimParams[0] != "param-once" {
		t.Errorf("请求带的 param = %v，want [param-once]", u.claimParams)
	}
}

// TestClaimWithCaptchaSendsRequestWhenMinterFails 铸不出 param 时**照发**，
// 让上游如实回 3007。
//
// 求解器是可选组件（目录可能为空、node 可能不存在、也可能正被上游限流）。
// "因为没配求解器所以整个激活失败"会掩盖真实原因；照发之后上游回的 3007
// 才是诚实的答复，用户也能分清是求解器的问题还是上游的判定。
func TestClaimWithCaptchaSendsRequestWhenMinterFails(t *testing.T) {
	u := &warmUpstream{}
	c, _ := newWarmTestClient(t, u, func(context.Context) (string, string, error) {
		return "", "", errors.New("求解器未配置")
	})

	cr := warmCred("zcode-x", jwtWithSub("u-1"))
	if _, err := c.claimWithCaptcha(context.Background(), cr, "p1"); err != nil {
		t.Fatalf("铸不出 param 不该让领取直接失败: %v", err)
	}

	u.mu.Lock()
	defer u.mu.Unlock()
	if len(u.claimParams) != 1 {
		t.Fatalf("应照发一次请求，实际 %d 次", len(u.claimParams))
	}
	if u.claimParams[0] != "" {
		t.Errorf("没铸出 param 就不该带这个头，得到 %q", u.claimParams[0])
	}
}

// TestClaimWithCaptchaReportsMinterFailure 两次都 3007 且求解器铸不出来时，
// 错误里要**同时**说明"验证码被拒"与"求解器没能铸出 param"。
//
// 只说前者会让用户去怀疑上游风控，而真实原因是我们这边根本没有可用的求解器。
func TestClaimWithCaptchaReportsMinterFailure(t *testing.T) {
	u := &warmUpstream{claimCodes: []string{"3007", "3007"}}
	c, _ := newWarmTestClient(t, u, func(context.Context) (string, string, error) {
		return "", "", errors.New("求解器未配置")
	})

	cr := warmCred("zcode-x", jwtWithSub("u-1"))
	_, err := c.claimWithCaptcha(context.Background(), cr, "p1")
	if err == nil {
		t.Fatal("两次 3007 应报错")
	}
	msg := err.Error()
	if !containsStr(msg, "验证码") {
		t.Errorf("错误应提到验证码：%s", msg)
	}
	if !containsStr(msg, "求解器") {
		t.Errorf("错误应提到求解器没能铸出 param：%s", msg)
	}
}

// TestSortOffersByPriorityIsDescendingAndStable priority 降序，且同 priority
// 时保持上游给出的相对顺序。
func TestSortOffersByPriorityIsDescendingAndStable(t *testing.T) {
	mk := func(id string, p int) PlanOffer {
		return PlanOffer{PlanID: id, Raw: map[string]any{"priority": float64(p)}}
	}
	got := sortOffersByPriority([]PlanOffer{
		mk("a", 1), mk("b", 9), mk("c", 5), mk("d", 9), mk("e", 0),
	})
	want := []string{"b", "d", "c", "a", "e"}
	for i := range want {
		if got[i].PlanID != want[i] {
			t.Fatalf("顺序 = %v，want %v", idsOf(got), want)
		}
	}
}

// TestPriorityOfToleratesMissingAndCamelCase priority 缺失 = 0（不是崩、
// 也不是把整个 offer 丢掉），并同时认 snake_case 与首字母大写两种拼写。
func TestPriorityOfToleratesMissingAndCamelCase(t *testing.T) {
	cases := []struct {
		name string
		raw  map[string]any
		want int
	}{
		{"nil Raw", nil, 0},
		{"缺字段", map[string]any{"plan_id": "p"}, 0},
		{"snake", map[string]any{"priority": float64(7)}, 7},
		{"大写", map[string]any{"Priority": float64(8)}, 8},
		{"字符串数字", map[string]any{"priority": "9"}, 9},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if got := priorityOf(PlanOffer{Raw: tc.raw}); got != tc.want {
				t.Errorf("priorityOf = %d，want %d", got, tc.want)
			}
		})
	}
}

// TestIsCaptchaRequiredMatchesKindAndCode 3007 的两个判据都认。
//
// 上游的码有时是数字、有时是字符串（见 codeOf 的注释），分类结果与原始码
// 只要有一处命中就不该漏判。
func TestIsCaptchaRequiredMatchesKindAndCode(t *testing.T) {
	if isCaptchaRequired(nil) {
		t.Error("nil 不是验证码错误")
	}
	byKind := &Error{Kind: ErrCaptchaRequired, Status: 400}
	if !isCaptchaRequired(byKind) {
		t.Error("Kind=ErrCaptchaRequired 应被判为验证码错误")
	}
	byCode := &Error{Kind: ErrNone, Status: 400, Code: CodeCaptchaRequired}
	if !isCaptchaRequired(byCode) {
		t.Error("Code=3007 应被判为验证码错误")
	}
	// 包装后仍要认得（errors.As）。
	wrapped := fmt.Errorf("claim p1: %w", byKind)
	if !isCaptchaRequired(wrapped) {
		t.Error("包装后的错误应仍被认出（用 errors.As，不是类型断言）")
	}
	other := &Error{Kind: ErrAuthFailed, Status: 401, Code: "1000"}
	if isCaptchaRequired(other) {
		t.Error("1000 认证失败不是验证码错误")
	}
	if isCaptchaRequired(errors.New("plain")) {
		t.Error("普通错误不是验证码错误")
	}
}

// TestWarmAccountGeneratesDeviceMidWhenMissing device_mid 缺失时现场生成
// 一个**UUID 形态**的值（事件不带认证头，device_mid 就是唯一身份）。
func TestWarmAccountGeneratesDeviceMidWhenMissing(t *testing.T) {
	u := &warmUpstream{}
	c, _ := newWarmTestClient(t, u, func(context.Context) (string, string, error) {
		return "param", "cn", nil
	})

	// FilePath 为空 ⇒ 不会触发落盘（落盘由 persistDeviceMidOnce 负责，
	// 而它需要 FilePath）。
	cr := warmCred("zcode-x", jwtWithSub("u-1"))
	cr.DeviceMid = ""

	if _, err := c.WarmAccount(context.Background(), cr); err != nil {
		t.Fatalf("WarmAccount: %v", err)
	}
	if !IsUUID(cr.DeviceMid) {
		t.Errorf("device_mid = %q，want UUID 形态（上游对控制面校验格式）", cr.DeviceMid)
	}

	u.mu.Lock()
	defer u.mu.Unlock()
	var m map[string]any
	_ = json.Unmarshal(u.eventBodies[0], &m)
	if m["device_mid"] != cr.DeviceMid {
		t.Errorf("事件里的 device_mid = %v，与 Cred 的 %q 不一致", m["device_mid"], cr.DeviceMid)
	}
}

// TestWarmAccountSkipsClaimWhenNoOffers 上游没给出可领套餐 = 正常状态
// （活动未上线 / 已领完），不是错误。
func TestWarmAccountSkipsClaimWhenNoOffers(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		switch {
		case hasSub(r.URL.Path, "/event/report"):
			_, _ = w.Write([]byte(`{"code":0}`))
		case hasSub(r.URL.Path, "/billing/preview"):
			_, _ = w.Write([]byte(`{"code":0,"data":{"server_time":1,"plans":[]}}`))
		default:
			http.Error(w, "unexpected", 404)
		}
	}))
	defer srv.Close()

	c := New()
	c.HTTP = srv.Client()
	c.Identity = Identity{AppVersion: "3.14.0", Platform: "windows"}
	c.SetEventBaseForTest(srv.URL)
	c.SetClaimBaseForTest(srv.URL)

	rep, err := c.WarmAccount(context.Background(), warmCred("zcode-x", jwtWithSub("u-1")))
	if err != nil {
		t.Fatalf("WarmAccount: %v", err)
	}
	if rep.Events != "" {
		t.Errorf("Events = %q，want 空串（事件成功了）", rep.Events)
	}
	if rep.Claim != "" {
		t.Errorf("Claim = %q，want 空串（没有可领套餐不是错误）", rep.Claim)
	}
	if rep.Skipped == "" {
		t.Error("Skipped 应说明领取阶段被跳过")
	}
	if len(rep.Plans) != 0 {
		t.Errorf("Plans = %v，want 空", rep.Plans)
	}
}

// TestWarmAccountTreatsAlreadyClaimedAsSuccess 1003 already_claimed 算成功
// （目标"拿到该套餐"已经达成），且要标出来让界面能区分。
func TestWarmAccountTreatsAlreadyClaimedAsSuccess(t *testing.T) {
	u := &warmUpstream{claimCodes: []string{"1003", "1003"}}
	c, _ := newWarmTestClient(t, u, func(context.Context) (string, string, error) {
		return "param", "cn", nil
	})

	rep, err := c.WarmAccount(context.Background(), warmCred("zcode-x", jwtWithSub("u-1")))
	if err != nil {
		t.Fatalf("WarmAccount: %v", err)
	}
	if rep.Claim != "" {
		t.Errorf("Claim = %q，want 空串（1003 不是错误）", rep.Claim)
	}
	if len(rep.Claimed) != 2 {
		t.Fatalf("Claimed = %v，want 两个（幂等命中也要记）", rep.Claimed)
	}
	for _, p := range rep.Plans {
		if !p.OK || !p.AlreadyClaimed {
			t.Errorf("套餐 %s 应标记 OK + AlreadyClaimed，得到 %+v", p.PlanID, p)
		}
	}
}

// TestWarmAccountReportsBusinessErrorWithoutFailingTheCall 上游业务错误
// 写进 report，**不**作为函数 error 返回。
//
// 与 logincli 的既有口径一致（runPreviewPlan 的注释）：否则宿主会把"上游
// 返回业务错误"当成"网关命令执行失败"，用户看到笼统的"命令失败"而非
// 具体原因。
func TestWarmAccountReportsBusinessErrorWithoutFailingTheCall(t *testing.T) {
	u := &warmUpstream{claimCodes: []string{"1004", "1004"}}
	c, _ := newWarmTestClient(t, u, func(context.Context) (string, string, error) {
		return "param", "cn", nil
	})

	rep, err := c.WarmAccount(context.Background(), warmCred("zcode-x", jwtWithSub("u-1")))
	if err != nil {
		t.Fatalf("上游业务错误不该让 WarmAccount 返回 error: %v", err)
	}
	if rep.Claim == "" {
		t.Error("Claim 应记下 1004 ineligible")
	}
	for _, p := range rep.Plans {
		if p.OK {
			t.Errorf("套餐 %s 不该标成功", p.PlanID)
		}
		if p.Message == "" {
			t.Errorf("套餐 %s 应带上失败原因", p.PlanID)
		}
	}
}

// TestWarmAccountRejectsNilCred 没有账号就是函数级错误（区别于上游业务错误）。
func TestWarmAccountRejectsNilCred(t *testing.T) {
	c := New()
	if _, err := c.WarmAccount(context.Background(), nil); err == nil {
		t.Fatal("nil 凭证应报错")
	}
}

// --- 小工具 ---

func sortedKeys(m map[string]any) []string {
	out := make([]string, 0, len(m))
	for k := range m {
		out = append(out, k)
	}
	for i := 1; i < len(out); i++ {
		for j := i; j > 0 && out[j] < out[j-1]; j-- {
			out[j], out[j-1] = out[j-1], out[j]
		}
	}
	return out
}

func idsOf(offers []PlanOffer) []string {
	out := make([]string, 0, len(offers))
	for _, o := range offers {
		out = append(out, o.PlanID)
	}
	return out
}

// containsStr / hasSub 用包里已有的（noroute_test.go:162、claim_scheduler_test.go:81），
// 不重复声明 —— Go 的测试文件同属一个包，重名会 build failed。
