package server

import (
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/pool"
	"workbuddy2api/internal/session"
	"workbuddy2api/internal/upstream"
)

// accountUnusableBody 是实测到的 403/11140 原文形状。
//
// 根因（见 upstream/client.go 的注释）：**该账号在本设备尚未完成登录**，
// 与请求体、模型、区域都无关 —— 同一份请求体换个账号就是 200。
// 所以它是「账号侧不可用」：必须冷却该号 + 换下一个号，
// 而不是把 403 原样甩给客户端（DSH 会把 403 映射成 AUTH → 「API 密钥无效」）。
const accountUnusableBody = `{"code":11140,"msg":"request illegal"}`

// emptyConfigBody 是给 /v3/config 的空应答。
//
// 为什么必须显式处理这条路径：成功响应回来之后 `fillBilling` 会调
// `buildCapabilityIndex()`，缓存未命中时它**真的会去打一次上游** /v3/config。
// 如果我们只按 Authorization 计数，那次配置拉取会被算成「又换了一次号」，
// 于是「首帧有效后不再轮转」这类断言永远假失败。
const emptyConfigBody = `{"code":0,"data":{"agents":[],"models":[]}}`

// chatPath 是 chat 转发的上游路径（其余路径都当配置/计费端点处理）。
const chatPath = "/v2/chat/completions"

// chatLog 只统计**chat 路径**的调用，按 Authorization 分组。
//
// 配置拉取（/v3/config）不计入 —— 它由 fillBilling 触发，与轮转无关。
type chatLog struct {
	mu   sync.Mutex
	chat map[string]int
	all  map[string]int
}

func (c *chatLog) note(authz, path string) {
	c.mu.Lock()
	c.all[path]++
	if path == chatPath {
		c.chat[authz]++
	}
	c.mu.Unlock()
}

func (c *chatLog) count(authz string) int {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.chat[authz]
}

func (c *chatLog) accounts() int {
	c.mu.Lock()
	defer c.mu.Unlock()
	return len(c.chat)
}

func (c *chatLog) total() int {
	c.mu.Lock()
	defer c.mu.Unlock()
	n := 0
	for _, v := range c.chat {
		n += v
	}
	return n
}

// accountUnusableUpstream 造一个「按路径 + 账号」分发的假上游。
//
// 与 `newFakeUpstream` 的区别：那个只看 Authorization，且**不设 BaseIntl**
//（国际版账号会真连外网）；这里把 BaseIntl 也指向 fake，并把非 chat 路径
// 统一回一份空配置，使 chat 调用次数成为唯一可信的轮转证据。
func accountUnusableUpstream(t *testing.T, chat func(authz string) (int, string, bool)) (*upstream.Client, *chatLog) {
	t.Helper()
	log := &chatLog{chat: map[string]int{}, all: map[string]int{}}
	up := &upstream.Client{
		HTTP: &http.Client{Transport: roundTripFunc(func(r *http.Request) (*http.Response, error) {
			authz := r.Header.Get("Authorization")
			log.note(authz, r.URL.Path)

			status, body, isStream := 200, emptyConfigBody, false
			if r.URL.Path == chatPath {
				status, body, isStream = chat(authz)
			}
			ct := "application/json"
			if isStream {
				ct = "text/event-stream"
			}
			return &http.Response{
				StatusCode: status,
				Header:     http.Header{"Content-Type": []string{ct}},
				Body:       io.NopCloser(strings.NewReader(body)),
			}, nil
		})},
		ChatBaseCN:    "https://fake.example",
		BillingBaseCN: "https://fake.example",
		BaseIntl:      "https://fake.example",
	}
	return up, log
}

// unusableChatBody 发一个最普通的 chat 请求体（裸模型名，不带产品/区域前缀）。
//
// 用裸名 glm-5.2 是刻意的：本文件要测的是**账号轮转**，不是产品/区域路由
// —— 带上前缀会把 FailureModelNotInProduct / FailureModelNotInRegion 那些
// 请求侧分支拉进来，测出来的失败将无法归因。
func unusableChatBody(stream bool) string {
	if stream {
		return `{"model":"glm-5.2","stream":true,"messages":[{"role":"user","content":"hi"}]}`
	}
	return `{"model":"glm-5.2","messages":[{"role":"user","content":"hi"}]}`
}

// newUnusableTestHandler 装配一个转发环境，并做好**进程级全局状态**的隔离。
//
// resetWafIP：wafIP 是进程级窗口（60s / 2 个不同账号），上个用例留下的命中
// 会渗进来，让「403 未污染 IP 窗口」的断言假失败。
// resetModelsCache：模型清单缓存是包级全局的，残留会让成本倍率注入不确定。
func newUnusableTestHandler(t *testing.T, up *upstream.Client, maxRotate int, auths ...*auth.Auth) (*Handler, *pool.Pool) {
	t.Helper()
	resetWafIP(t)
	resetModelsCache()

	p := testPoolWith(auths...)
	return NewHandler(Config{
		Pool:         p,
		Upstream:     up,
		MaxRotate:    maxRotate,
		SoftCooldown: time.Minute,
	}), p
}

// workBuddyAuth 造一个国服 WorkBuddy 账号（区域字段显式给全，避免测试依赖默认值）。
func workBuddyAuth(uid, token string) *auth.Auth {
	return &auth.Auth{
		UID:         uid,
		AccessToken: token,
		Domain:      "www.workbuddy.cn",
		ExpiresAt:   9999999999,
	}
}

// 403/11140 的账号必须被**软冷却**，且同一个请求内要换到下一个账号，
// 客户端拿到 200 而不是失败。
//
// 这是所有者报的那个缺陷的正面回归：「密钥明明是正确的，不知道怎么对话
// 过程中就提示『本轮运行失败 API 密钥无效』」—— 旧行为把 403 原样透传，
// DSH 把 403 映射成 AUTH，界面就显示「API 密钥无效」。
func TestAccountUnusableRotatesToNextAccount(t *testing.T) {
	if got := upstream.Classify(403, accountUnusableBody); got != upstream.ErrAccountUnusable {
		t.Fatalf("前置契约失效：Classify(403, 11140)=%v，期望 ErrAccountUnusable", got)
	}

	up, log := accountUnusableUpstream(t, func(authz string) (int, string, bool) {
		if authz == "Bearer at-bad" {
			return 403, accountUnusableBody, false
		}
		return 200, sseOK, true
	})

	bad := workBuddyAuth("bad", "at-bad")
	good := workBuddyAuth("good", "at-good")
	h, p := newUnusableTestHandler(t, up, 0, bad, good)

	// bad 积分更高 → 权重更高 → 注入 randInt64N=0 的池必然先选它，
	// 「谁先被打」完全确定，不再受加权随机影响。
	p.SetCredits("bad", 2000)

	code, body := postChat(t, h, unusableChatBody(false))
	if code != 200 {
		t.Fatalf("403/11140 后没有换到下一个账号：code=%d body=%s", code, body)
	}
	if log.count("Bearer at-bad") != 1 || log.count("Bearer at-good") != 1 {
		t.Fatalf("上游 chat 调用次数不符（期望 bad/good 各 1 次）：bad=%d good=%d",
			log.count("Bearer at-bad"), log.count("Bearer at-good"))
	}
	if !strings.Contains(body, "你好") {
		t.Fatalf("客户端没有拿到第二个账号的内容：%s", body)
	}
	// 失败必须被吞在网关内部 —— 漏出 11140 就等于又把这个缺陷暴露了一次。
	if strings.Contains(body, "11140") {
		t.Fatalf("上游 403 报文漏进了成功响应：%s", body)
	}

	st, ok := p.Status("bad")
	if !ok {
		t.Fatal("bad 账号从池里消失了")
	}
	if !st.Cooling {
		t.Fatalf("403/11140 的账号没有被冷却 —— 坏号会永远占着最便宜那一档，"+
			"表现为「同一个模型反复失败却永远不降级」：%+v", st)
	}
	// CoolSoft 在 /status 里的呈现是 soft_rate（**不是** "account_unusable"）。
	if st.CoolKind != "soft_rate" {
		t.Errorf("CoolKind=%q，期望 soft_rate", st.CoolKind)
	}
	if !strings.Contains(st.Reason, "11140") {
		t.Errorf("冷却原因没有带上 11140 归因，事后无法分辨它是怎么被罚的：%q", st.Reason)
	}
	// 根因是「未完成登录」，重登可恢复 ⇒ 只软冷却，绝不永久禁用。
	if st.Disabled {
		t.Errorf("403/11140 不应永久禁用账号：%+v", st)
	}

	if gs, _ := p.Status("good"); gs.Cooling || gs.Disabled {
		t.Errorf("成功账号被误罚：%+v", gs)
	}
}

// 冷却必须让坏号**退出下一轮请求的候选**：这正是「自动降级到下一个成本档」
// 的机制。若只换号不冷却（修复前的 default 分支行为），坏号会永远被选中，
// 成本分层就卡在最便宜却打不通的那一档 —— 实测裸名 0/6 全败。
func TestAccountUnusableCooldownKeepsBadAccountOutOfNextRequest(t *testing.T) {
	up, log := accountUnusableUpstream(t, func(authz string) (int, string, bool) {
		if authz == "Bearer at-bad" {
			return 403, accountUnusableBody, false
		}
		return 200, sseOK, true
	})

	bad := workBuddyAuth("bad", "at-bad")
	good := workBuddyAuth("good", "at-good")
	h, p := newUnusableTestHandler(t, up, 0, bad, good)
	p.SetCredits("bad", 2000)

	if code, body := postChat(t, h, unusableChatBody(false)); code != 200 {
		t.Fatalf("第一次请求失败：code=%d body=%s", code, body)
	}
	// 第二次请求：坏号在冷却窗口内，必须**一次都不再被打**。
	if code, body := postChat(t, h, unusableChatBody(false)); code != 200 {
		t.Fatalf("第二次请求失败：code=%d body=%s", code, body)
	}

	if got := log.count("Bearer at-bad"); got != 1 {
		t.Fatalf("冷却中的坏号被再次选中（第 %d 次）—— 成本分层会卡在这一档", got)
	}
	if got := log.count("Bearer at-good"); got != 2 {
		t.Fatalf("好号调用次数=%d，期望 2（两次请求各一次）", got)
	}
}

// 全部账号都 403/11140、且循环因**选不出号**而结束时，对外是 503
// + no_healthy_account —— 绝不能是 401/403。
//
// 为什么是 503 而不是 502：选不出号的分支（forward.go 的 acct == nil）
// 会先把 lastStatus 置成 503 再 break，因此走不到 clientFacingStatus 的
// 403→502 映射。两条路径都安全，但必须钉住「绝不漏 403」这一点。
func TestAccountUnusableAllExhaustedNeverLooksLikeAuth(t *testing.T) {
	up, log := accountUnusableUpstream(t, func(string) (int, string, bool) {
		return 403, accountUnusableBody, false
	})
	h, _ := newUnusableTestHandler(t, up, 0, workBuddyAuth("only", "at-only"))

	code, body := postChat(t, h, unusableChatBody(false))

	if code == http.StatusUnauthorized || code == http.StatusForbidden {
		t.Fatalf("上游 403 直接漏给客户端（DSH 会显示「API 密钥无效」）：code=%d body=%s", code, body)
	}
	if code != http.StatusServiceUnavailable {
		t.Fatalf("code=%d，期望 503：%s", code, body)
	}
	// 唯一账号失败后 tried 已含它，下一轮必然选不出号 ⇒ 只打一次上游。
	if log.total() != 1 {
		t.Fatalf("唯一账号被打了 %d 次 chat，期望 1 次（tried 必须拦住重复选号）", log.total())
	}

	var env struct {
		Error struct {
			Message string `json:"message"`
			Code    string `json:"code"`
		} `json:"error"`
	}
	if err := json.Unmarshal([]byte(body), &env); err != nil {
		t.Fatalf("错误体不是 JSON：%v body=%s", err, body)
	}
	if env.Error.Code != "no_healthy_account" {
		t.Errorf("code=%q，期望 no_healthy_account", env.Error.Code)
	}
	// 文案来自 FriendlyMessage 的 ErrAccountUnusable 分支（见
	// upstream/client.go）。在此之前它没有分支，于是这里回落到
	// `msg += ": " + lastErr.Error()` —— 客户端拿到的是整段上游 JSON
	// `{"code":11140,"msg":"request illegal"}`，既无归因也无下一步动作。
	//
	// 这里钉住「归因必须留在文案里」：换成别的措辞可以，但不能退化成
	// 一句无法排查的「服务不可用」。
	if !strings.Contains(env.Error.Message, "11140") {
		t.Errorf("耗尽文案里没有 11140 归因，用户无法判断该去重登哪个账号：%q", env.Error.Message)
	}
	// 同时不得把上游原始报文整段透出（实测就是上面那串 JSON）。
	if strings.Contains(env.Error.Message, "request illegal") {
		t.Errorf("耗尽文案把上游原始报文透给了客户端：%q", env.Error.Message)
	}
}

// 当失败来自**账号耗尽 + MaxRotate 用尽**这条路径时，对外是 502：
// 这是 clientFacingStatus 把上游 403 映射掉的那条分支，必须端到端验证一次
// （纯函数表在 client_status_test.go，这里验的是转发层真的走到了它）。
func TestAccountUnusableExhaustedViaMaxRotateIs502(t *testing.T) {
	up, log := accountUnusableUpstream(t, func(string) (int, string, bool) {
		return 403, accountUnusableBody, false
	})
	// MaxRotate=1：打完唯一账号后 for 条件不成立直接退出循环，
	// lastStatus 保持 403 → clientFacingStatus(403) = 502。
	h, _ := newUnusableTestHandler(t, up, 1, workBuddyAuth("only", "at-only"))

	code, body := postChat(t, h, unusableChatBody(false))

	if code != http.StatusBadGateway {
		t.Fatalf("code=%d，期望 502（403 必须被 clientFacingStatus 映射掉）：%s", code, body)
	}
	if log.total() != 1 {
		t.Fatalf("chat 调用次数=%d，期望 1", log.total())
	}
	if !strings.Contains(body, "11140") {
		t.Errorf("502 文案里没有 11140 归因：%s", body)
	}
}

// 流式请求同理：403/11140 发生在**响应头写出之前**，必须换号重试；
// 换到成功账号后客户端拿到的是一条干净的 200 SSE 流。
func TestAccountUnusableStreamRotatesBeforeHeaders(t *testing.T) {
	up, log := accountUnusableUpstream(t, func(authz string) (int, string, bool) {
		if authz == "Bearer at-bad" {
			return 403, accountUnusableBody, false
		}
		return 200, sseOK, true
	})

	bad := workBuddyAuth("bad", "at-bad")
	good := workBuddyAuth("good", "at-good")
	h, p := newUnusableTestHandler(t, up, 0, bad, good)
	p.SetCredits("bad", 2000)

	req := httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(unusableChatBody(true)))
	req.Header.Set("Content-Type", "application/json")
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, req)

	if rec.Code != 200 {
		t.Fatalf("流式请求没有换到下一个账号：code=%d body=%s", rec.Code, rec.Body.String())
	}
	if ct := rec.Header().Get("Content-Type"); !strings.Contains(ct, "text/event-stream") {
		t.Errorf("Content-Type=%q，期望 text/event-stream", ct)
	}
	out := rec.Body.String()
	if !strings.Contains(out, "你好") || !strings.Contains(out, "data: [DONE]") {
		t.Fatalf("流内容不完整：%s", out)
	}
	if strings.Contains(out, "11140") {
		t.Fatalf("上游 403 报文漏进了 SSE 流：%s", out)
	}
	if log.count("Bearer at-bad") != 1 || log.count("Bearer at-good") != 1 {
		t.Fatalf("上游 chat 调用次数不符：bad=%d good=%d",
			log.count("Bearer at-bad"), log.count("Bearer at-good"))
	}
}

// 反向保障：首个有效数据帧一旦拿到，**不得**再换号 ——
// 换了就会把已经写出的内容丢掉，客户端看到半截回复。
//
// 让「先被选中的账号」直接返回合法 SSE、另一个账号配置成 403：
// 只要第二个账号一次都没被打，就证明首帧有效后网关是**提交**而不是继续轮转。
func TestValidFirstFrameCommitsWithoutFurtherRotation(t *testing.T) {
	up, log := accountUnusableUpstream(t, func(authz string) (int, string, bool) {
		if authz == "Bearer at-streamer" {
			return 200, sseOK, true
		}
		return 403, accountUnusableBody, false
	})

	streamer := workBuddyAuth("streamer", "at-streamer")
	other := workBuddyAuth("other", "at-other")
	h, p := newUnusableTestHandler(t, up, 0, streamer, other)
	p.SetCredits("streamer", 2000)

	req := httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(unusableChatBody(true)))
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, req)

	if rec.Code != 200 {
		t.Fatalf("code=%d body=%s", rec.Code, rec.Body.String())
	}
	if got := log.count("Bearer at-streamer"); got != 1 {
		t.Fatalf("成功账号 chat 调用次数=%d，期望 1", got)
	}
	if got := log.count("Bearer at-other"); got != 0 {
		t.Fatalf("首帧已有效却仍换号（会把已写出的内容丢掉）：other 被打了 %d 次", got)
	}
}

// 业务信封的 403/11140 **不是** WAF 拦截：它不得进入出口 IP 窗口，
// 也不得触发「换号无用」的提前终止 —— 换号恰恰是唯一正确的处置。
func TestAccountUnusableDoesNotTripEgressIPWindow(t *testing.T) {
	up, log := accountUnusableUpstream(t, func(string) (int, string, bool) {
		return 403, accountUnusableBody, false
	})

	h, _ := newUnusableTestHandler(t, up, 3,
		workBuddyAuth("a1", "at-a1"),
		workBuddyAuth("a2", "at-a2"),
		workBuddyAuth("a3", "at-a3"))

	code, body := postChat(t, h, unusableChatBody(false))

	if got := wafIP.distinct(); got != 0 {
		t.Fatalf("业务信封 403 被误判成 WAF，IP 窗口被污染：distinct=%d", got)
	}
	if strings.Contains(body, "egress_ip_blocked") {
		t.Fatalf("403/11140 被误报成出口 IP 被封（会误导用户去换网络）：%s", body)
	}
	// 三个账号 + MaxRotate=3 ⇒ 三个号都要试过，说明 403 没有让轮转提前停下。
	if got := log.accounts(); got != 3 {
		t.Fatalf("只试了 %d 个账号（期望 3）", got)
	}
	if code == http.StatusForbidden || code == http.StatusUnauthorized {
		t.Fatalf("code=%d 是鉴权类状态码，会被 DSH 显示成「API 密钥无效」：%s", code, body)
	}
}

// 会话粘性必须跟着**最终成功的账号**走：坏号被解绑、新号被绑定，
// 否则下一轮请求还会被粘回那个打不通的号，表现为「同一个会话反复失败」。
func TestAccountUnusableStickyConversationMovesToNextAccount(t *testing.T) {
	st := newBindStore()
	sess := session.New(session.Config{
		TTL:       time.Minute,
		Store:     st,
		Available: func() []string { return []string{"bad", "good"} },
	})
	sess.Bind("conv-11140", "bad")

	up, log := accountUnusableUpstream(t, func(authz string) (int, string, bool) {
		if authz == "Bearer at-bad" {
			return 403, accountUnusableBody, false
		}
		return 200, sseOK, true
	})

	resetWafIP(t)
	resetModelsCache()
	p := testPoolWith(workBuddyAuth("bad", "at-bad"), workBuddyAuth("good", "at-good"))
	h := NewHandler(Config{
		Pool:         p,
		Upstream:     up,
		Session:      sess,
		SoftCooldown: time.Minute,
	})

	body := `{"model":"glm-5.2","messages":[{"role":"user","content":"hi"}],` +
		`"metadata":{"conversation_id":"conv-11140"}}`
	code, resp := postChat(t, h, body)

	if code != 200 {
		t.Fatalf("粘性会话没有从坏号换到好号：code=%d body=%s", code, resp)
	}
	if uid, ok := st.lastUID("conv-11140"); !ok || uid != "good" {
		t.Fatalf("会话绑定没有迁移到成功账号：uid=%q ok=%v", uid, ok)
	}
	if st.delCnt == 0 {
		t.Error("坏号的粘性绑定没有被解绑 —— 下一轮还会被粘回它")
	}
	if log.count("Bearer at-bad") != 1 || log.count("Bearer at-good") != 1 {
		t.Fatalf("上游 chat 调用次数不符：bad=%d good=%d",
			log.count("Bearer at-bad"), log.count("Bearer at-good"))
	}
}

// 区域不是优先级维度（所有者原话「跟区域没关系，就是成本有关系」）：
// 一个区域的账号被 11140 拒绝后，必须能换到**另一个区域**的账号，
// 而不是因为「偏好区域」把自己锁死在一个打不通的区域里。
func TestAccountUnusableRotatesAcrossRegions(t *testing.T) {
	up, log := accountUnusableUpstream(t, func(authz string) (int, string, bool) {
		if authz == "Bearer at-intl" {
			return 403, accountUnusableBody, false
		}
		return 200, sseOK, true
	})

	// 用**到期档位**而不是积分来定「谁先被打」：同档内积分不参与权重
	// （tierWeightOf 刻意不含 credits —— 同一档紧迫度相同），
	// 此时并列权重按 UID 升序破平，会选中 cn-1 而不是 intl-1。
	// 让国际版号更早到期 → 它是唯一的最紧迫档 → 必然先被选中。
	intl := &auth.Auth{UID: "intl-1", AccessToken: "at-intl", Domain: "www.workbuddy.ai",
		ExpiresAt: 9999999999, SoonestExpireAt: farFuture}
	cn := &auth.Auth{UID: "cn-1", AccessToken: "at-cn", Domain: "www.workbuddy.cn",
		ExpiresAt: 9999999999, SoonestExpireAt: farFuture + 86400*30}

	h, p := newUnusableTestHandler(t, up, 0, intl, cn)

	code, body := postChat(t, h, unusableChatBody(false))

	if code != 200 {
		t.Fatalf("国际版号被拒后没有降级到国服号：code=%d body=%s", code, body)
	}
	if log.count("Bearer at-intl") != 1 || log.count("Bearer at-cn") != 1 {
		t.Fatalf("跨区域轮转的 chat 调用次数不符：intl=%d cn=%d",
			log.count("Bearer at-intl"), log.count("Bearer at-cn"))
	}
	if st, _ := p.Status("intl-1"); !st.Cooling {
		t.Errorf("被拒的国际版号没有被冷却：%+v", st)
	}
	if st, _ := p.Status("cn-1"); st.Cooling || st.Disabled {
		t.Errorf("国服号被误罚：%+v", st)
	}
}

// 网关自身的鉴权失败必须仍然是 401，且**不得**打到上游 ——
// 这条与「上游 403 映射成 502/503」是两件不同的事，
// 混在一起就又回到「API 密钥无效」那个误报上。
func TestGatewayAuthFailureStill401AndSkipsUpstream(t *testing.T) {
	up, log := accountUnusableUpstream(t, func(string) (int, string, bool) {
		return 403, accountUnusableBody, false
	})

	resetWafIP(t)
	resetModelsCache()
	p := testPoolWith(workBuddyAuth("only", "at-only"))
	h := NewHandler(Config{
		Pool:         p,
		Upstream:     up,
		APIKey:       "secret",
		SoftCooldown: time.Minute,
	})

	req := httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(unusableChatBody(false)))
	req.Header.Set("Authorization", "Bearer wrong")
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, req)

	if rec.Code != http.StatusUnauthorized {
		t.Fatalf("网关鉴权失败 code=%d，期望 401：%s", rec.Code, rec.Body.String())
	}
	if log.total() != 0 {
		t.Fatalf("鉴权失败仍打了 %d 次上游 chat", log.total())
	}
}
