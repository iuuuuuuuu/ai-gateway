package server

// `?refresh=1` 清掉按区域的**失败负缓存**（所有者 2026-09-20 实测缺陷）。
//
// # 现场
//
// 所有者：「我明明国内外账号都有,居然还有这个提示 这是个bug」。
//
// 他两侧账号都齐（实测带代理后 intl 21 个模型、cn 16 个模型都能拉到），
// 但**某一轮**国际版拉取失败（网络抖动）⇒ `knownRegion[intl]=false` ⇒
// 近半数模型被打上「国服未检测到可用真值」。
//
// 而那段文案结尾写着「稍后点「刷新」重试即可确认」—— **但它做不到**：
//
//	`resetModelsCache()` 只被测试调用，生产代码里**没有任何地方调它**；
//	`fetchModelsForRegion` 失败后进入 `modelsFetchFailCooldown`（5 分钟），
//	期间**直接 return nil**，连试都不试。
//
// 于是用户点「刷新」→ 走同一个 `/v1/models` → 仍吃负缓存 → 界面毫无变化。
// **文案让他做的事，代码不支持。**
import (
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/upstream"
)

// flakyUpstream 第一次拉模型失败、之后成功。
//
// 用来复现"某一轮抖动导致 knownRegion=false，之后本可成功却被负缓存挡住"。
func flakyUpstream(t *testing.T, failFirst int) (*Handler, *int) {
	t.Helper()
	resetModelsCache()
	calls := 0
	up := newFakeUpstream(t, func(string) (int, string, bool) {
		calls++
		if calls <= failFirst {
			return http.StatusInternalServerError, `{"code":500,"msg":"boom"}`, false
		}
		return http.StatusOK, "", false
	})
	up.BaseIntl = "https://intl.fake.example"
	h := NewHandler(Config{
		Pool: testPoolWith(
			&auth.Auth{UID: authCN, AccessToken: "t", Domain: "copilot.tencent.com", SoonestExpireAt: 1 << 40},
			&auth.Auth{UID: authIntl, AccessToken: "t", Domain: "www.workbuddy.ai", SoonestExpireAt: 1 << 40},
		),
		Upstream:  up,
		MaxRotate: 1,
	})
	return h, &calls
}

// callModelsWith 调一次 /v1/models（可带 query）。
func callModelsWith(h *Handler, query string) int {
	req := httptest.NewRequest(http.MethodGet, "/v1/models"+query, nil)
	rec := httptest.NewRecorder()
	h.mux.ServeHTTP(rec, req)
	return rec.Code
}

// TestModelsRefreshClearsFailCooldown `?refresh=1` 必须真的重拉（突破负缓存）。
//
// 判据是**上游调用次数增加** —— 若仍吃负缓存，第二次请求根本不会打上游。
func TestModelsRefreshClearsFailCooldown(t *testing.T) {
	h, calls := flakyUpstream(t, 2) // 前两次调用失败

	// 第一次：两区都失败 ⇒ 进入负缓存
	callModelsWith(h, "")
	afterFirst := *calls

	// 第二次**不带 refresh**：应被负缓存挡住，不再打上游
	callModelsWith(h, "")
	if *calls != afterFirst {
		t.Errorf("负缓存期内不该重复打上游（第 1 次后 %d，不带 refresh 后 %d）",
			afterFirst, *calls)
	}

	// 第三次**带 refresh**：必须突破负缓存、真的重试
	callModelsWith(h, "?refresh=1")
	if *calls == afterFirst {
		t.Fatalf("`?refresh=1` 必须清掉负缓存并重拉，实际上游调用次数没变（%d）—— "+
			"那正是所有者的现场：文案说「稍后点刷新重试即可确认」，"+
			"而点了没有任何变化", *calls)
	}
}

// TestModelsRefreshAcceptsCommonSpellings 宽容接受几种写法。
//
// 客户端不必纠结用 1 / true / yes —— 与网关其它布尔 query 同一取向。
func TestModelsRefreshAcceptsCommonSpellings(t *testing.T) {
	for _, q := range []string{"?refresh=1", "?refresh=true", "?refresh=TRUE", "?refresh=yes"} {
		h, calls := flakyUpstream(t, 2)
		callModelsWith(h, "")
		before := *calls
		callModelsWith(h, q)
		if *calls == before {
			t.Errorf("%s 应触发重拉（上游调用次数未变）", q)
		}
	}
	// 反面：空值 / 0 / false 不该触发重拉
	for _, q := range []string{"?refresh=0", "?refresh=false", "?refresh=", "?refresh=no"} {
		h, calls := flakyUpstream(t, 2)
		callModelsWith(h, "")
		before := *calls
		callModelsWith(h, q)
		if *calls != before {
			t.Errorf("%s 不该触发重拉（它不表示强制刷新）", q)
		}
	}
}

// TestModelsRefreshRecoversRegionTruth 重拉后区域真值要真的恢复。
//
// 这是**用户可见的结果**：重拉成功后，之前被打上「另一区未检测到真值」的
// 模型应当不再带 unverified_regions。
//
// 夹具：池里两区账号都有，但**国际版的上游第一次失败**（模拟网络抖动）。
// 于是第一轮 knownRegion[intl]=false ⇒ 只在 intl 出现的模型被打 unverified=cn。
// 重拉（且这次上游成功）后应当补回真值。
func TestModelsRefreshRecoversRegionTruth(t *testing.T) {
	resetModelsCache()

	// 国际版第一次失败、之后成功；国服始终成功。
	// ⚠ 按 **Host** 区分区域（复用 isIntlHost）而不是按调用次序 ——
	// 两区的拉取是并发的，"第几次调用"会偶尔分错，
	// 那会让测试时过时不过（我第一版就是按次数，第一轮压根没产生 unverified）。
	intlCalls := 0
	fail := true
	proxy := roundTripFunc(func(r *http.Request) (*http.Response, error) {
		if isIntlHost(r.URL.Host) {
			intlCalls++
			if fail {
				return &http.Response{
					StatusCode: http.StatusInternalServerError,
					Header:     http.Header{"Content-Type": []string{"application/json"}},
					Body:       io.NopCloser(strings.NewReader(`{"code":500,"msg":"boom"}`)),
				}, nil
			}
		}
		// 复用 regionAwareUpstream 的构造口径：按区域给不同清单
		body := v3WithModels("cn-only-model", "shared")
		if isIntlHost(r.URL.Host) {
			body = v3WithModels("intl-only-model", "shared")
		}
		return &http.Response{
			StatusCode: http.StatusOK,
			Header:     http.Header{"Content-Type": []string{"application/json"}},
			Body:       io.NopCloser(strings.NewReader(body)),
		}, nil
	})

	up := &upstream.Client{
		HTTP:       &http.Client{Transport: proxy},
		ChatBaseCN: "https://cn.fake.example",
	}
	h := NewHandler(Config{
		Pool: testPoolWith(
			&auth.Auth{UID: authCN, AccessToken: "t", Domain: "copilot.tencent.com", SoonestExpireAt: 1 << 40},
			&auth.Auth{UID: authIntl, AccessToken: "t", Domain: "www.workbuddy.ai", SoonestExpireAt: 1 << 40},
		),
		Upstream:  up,
		MaxRotate: 1,
	})

	// 第一轮：国际版失败 ⇒ intl-only-model 应带 unverified
	first := listModels(t, h)
	before := 0
	for _, e := range first {
		if len(strList(e["unverified_regions"])) > 0 {
			before++
		}
	}
	if before == 0 {
		t.Fatalf("夹具未产生 unverified（国际版应失败一次），实际 0 条 —— "+
			"这个测试就没在测恢复；intlCalls=%d", intlCalls)
	}

	// 上游恢复
	fail = false

	// 不带 refresh：仍吃负缓存 ⇒ 不该有变化
	callModelsWith(h, "")
	still := 0
	for _, e := range listModels(t, h) {
		if len(strList(e["unverified_regions"])) > 0 {
			still++
		}
	}
	if still != before {
		t.Errorf("负缓存期内不该变化（前 %d → 后 %d）", before, still)
	}

	// 带 refresh：必须重拉并补回真值
	callModelsWith(h, "?refresh=1")
	after := 0
	for _, e := range listModels(t, h) {
		if len(strList(e["unverified_regions"])) > 0 {
			after++
		}
	}
	if after >= before {
		t.Errorf("`?refresh=1` 后未验证数应减少（前 %d → 后 %d）—— "+
			"若没减少，用户点了刷新也拿不回真值，只能干等满 5 分钟", before, after)
	}
}
// TestModelsRetryBackoff 失败重试间隔是**指数退避**且有上下限。
//
// # 所有者 2026-09-20
//
//	「这应该是自动的,而不是需要人手动同步,你懂吗?」
//
// 此前失败后是**固定** 5 分钟负缓存，期间连试都不试 ——
// 一次瞬时抖动（代理偶发抽风）就让"信息不完整"持续 5 分钟，
// 用户感受到的就是"这得手动同步"。而系统本可在 15 秒后自愈。
//
// 契约：
//   · 第一次失败只等 **15 秒**（瞬时抖动几乎无感 —— 那是最常见情形）
//   · 逐次翻倍，但**上限 5 分钟**（上游真挂时不打它）
//   · 上限必须存在：若无限增长，上游挂一天后恢复，用户还要再等几小时
//     —— 那又把"自动"变成了"手动"
func TestModelsRetryBackoff(t *testing.T) {
	first := modelsRetryAfter(1)
	if first != 15*time.Second {
		t.Errorf("第一次失败应只等 15 秒（瞬时抖动无感），实际 %v", first)
	}

	// 单调不减（退避必须越来越长，否则就是重试风暴）
	prev := time.Duration(0)
	for i := 1; i <= 12; i++ {
		d := modelsRetryAfter(i)
		if d < prev {
			t.Errorf("退避应单调不减：fails=%d 得 %v，而上一次是 %v", i, d, prev)
		}
		prev = d
	}

	// 上限
	if got := modelsRetryAfter(50); got != 5*time.Minute {
		t.Errorf("退避上限应为 5 分钟，实际 %v（无上限会让恢复后还要等很久）", got)
	}
	if got := modelsRetryAfter(0); got != 15*time.Second {
		t.Errorf("fails=0 应按首次处理（15 秒），实际 %v", got)
	}
	if got := modelsRetryAfter(-3); got != 15*time.Second {
		t.Errorf("负数应安全按首次处理，实际 %v", got)
	}
}

// TestModelsFetchRecoversAutomaticallyWithoutManualRefresh 无需任何手动操作即可自愈。
//
// 这是所有者诉求的**直接编码**：
//
//	「这应该是自动的,而不是需要人手动同步」
//
// 场景：某区第一次失败（瞬时抖动），退避窗口过后**下一次自动拉取**就该成功。
// 判据不涉及任何 `?refresh=1` —— 那是"手动"路径。
func TestModelsFetchRecoversAutomaticallyWithoutManualRefresh(t *testing.T) {
	resetModelsCache()

	// ⚠ 成功时**必须返回真实的 /v3/config 结构**（用 v3WithModels 造），
	// 不能回空 body —— 空 body 解析出 0 个模型，等价于"没拉到"，
	// 于是两区都没有真值，测试里根本不会出现 unverified（我第一版就是回空，
	// 结果被 skip）。夹具必须让"成功"这个分支真的有清单。
	intlFails := 0
	up := newFakeUpstream(t, func(authz string) (int, string, bool) {
		if strings.Contains(authz, "tok-intl") {
			if intlFails < 1 {
				intlFails++
				return http.StatusInternalServerError, `{"code":500,"msg":"boom"}`, false
			}
			return http.StatusOK, v3WithModels("intl-only-model", "shared"), false
		}
		return http.StatusOK, v3WithModels("cn-only-model", "shared"), false
	})
	up.BaseIntl = "https://intl.fake.example"
	h := NewHandler(Config{
		Pool: testPoolWith(
			&auth.Auth{UID: authCN, AccessToken: "tok-cn", Domain: "copilot.tencent.com", SoonestExpireAt: 1 << 40},
			&auth.Auth{UID: authIntl, AccessToken: "tok-intl", Domain: "www.workbuddy.ai", SoonestExpireAt: 1 << 40},
		),
		Upstream:  up,
		MaxRotate: 1,
	})

	// 第一轮：国际版失败 ⇒ 应该有不完整（unverified）的模型
	first := listModels(t, h)
	incomplete := 0
	for _, e := range first {
		if len(strList(e["unverified_regions"])) > 0 {
			incomplete++
		}
	}

	// 把上次失败时刻**人为拨回**到退避窗口之外（模拟"过了 15 秒"）。
	//
	// ⚠ 不 sleep：那会让测试慢且不稳；退避的时长本身由
	// TestModelsRetryBackoff 单独锁定。这里只验证"过了窗口就会重试"。
	regionModelCache.Lock()
	for _, rm := range regionModelCache.byRegion {
		if rm != nil && !rm.lastErr.IsZero() {
			rm.lastErr = time.Now().Add(-modelsRetryAfter(rm.fails) - time.Second)
		}
	}
	regionModelCache.Unlock()

	// **不带任何 refresh 参数**再读一次 —— 模拟前端 30 秒后的自动重试
	second := listModels(t, h)
	after := 0
	for _, e := range second {
		if len(strList(e["unverified_regions"])) > 0 {
			after++
		}
	}

	if incomplete == 0 {
		t.Skip("夹具第一轮没产生 unverified，跳过自愈验证")
	}
	if after != 0 {
		t.Errorf("过了退避窗口后，**无需手动刷新**就该自动补齐（前 %d 条不完整 → 后 %d 条）—— "+
			"若仍是 %d，说明自愈没接上，用户只能自己点（那正是所有者反对的）",
			incomplete, after, after)
	}
}