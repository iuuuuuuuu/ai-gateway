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