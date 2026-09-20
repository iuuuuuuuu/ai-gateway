package server

import (
	"io"
	"net/http"
	"strings"
	"testing"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/upstream"
)

// ---------------------------------------------------------------------------
// 回归：「另一区没有真值」绝不能说成「该模型仅某区存在」
//
// 所有者反馈（2026-09-18）原话：
//
//	「智能体管理那里，v4.1flash 国际服也有好吧，为什么你标明仅国服？」
//
// 实测根因链（逐层可核，不是推测）：
//
//	① 账号库 20 个账号里 6 个国际版**全部 disabled=true**（实测 accounts.json）；
//	② 宿主导出凭证时跳过禁用账号，于是 gateway_auths/ 里 14 个文件全是国服域名
//	   （实测：11 个 copilot.tencent.com + 3 个 www.codebuddy.cn，无一个 .ai）；
//	③ 网关池靠扫描该目录建立 → 池里没有国际版账号；
//	④ pickProbeAccountInRegion(Intl) 返回 nil（刻意不跨区回退）
//	   → 拉不到国际版清单 → knownRegion[intl]=false；
//	⑤ 旧的 capabilityFieldsFor 只看 supported==["cn"] 就下发
//	   region_note=「该模型名仅在国服上游存在」。
//
// 而事实是「你没有国际版账号，所以看不到国际版真值」。
// **「没拉到」不等于「不存在」** —— 两者给用户的行动指引相反：
// 前者要去补账号，后者只能换模型。本组测试锁住这个区分。
// ---------------------------------------------------------------------------

// v3WithModels 造一份 /v3/config 响应，cli 清单即给定的模型。
func v3WithModels(ids ...string) string {
	var listed, metas []string
	for _, id := range ids {
		listed = append(listed, `"`+id+`"`)
		metas = append(metas, `{"id":"`+id+`","maxInputTokens":1000000,"maxOutputTokens":128000,"supportsImages":true}`)
	}
	return `{"code":0,"data":{
		"agents":[{"name":"cli","models":[` + strings.Join(listed, ",") + `]}],
		"models":[` + strings.Join(metas, ",") + `]
	}}`
}

// isIntlHost 判断请求主机是否属于国际版。
//
// 同时认两种标记，避免夹具域名与真实域名不一致时静默走错分支：
//   - 真实域名 www.workbuddy.ai → 含 ".ai"
//   - 夹具域名 intl.fake.example → 含 "intl."
//
// 这个函数的存在本身是一次教训：早先判据只有 `strings.Contains(host, ".ai")`，
// 而夹具基址是 https://intl.fake.example —— **不含 `.ai` 子串**。于是国际版请求
// 被当成国服，两区拿到同一份清单，`TestConfirmedAbsentRegionStillSaysOnlyOneRegion`
// 那类用例看似通过、实则从未走到国际版分支（byRegion[intl] 里出现的是国服模型）。
// 夹具域名与真实域名不一致时，判据必须两边都认，否则测试是假绿的。
func isIntlHost(host string) bool {
	return strings.Contains(host, ".ai") || strings.Contains(host, "intl.")
}

// regionAwareUpstream 造一个**按请求域名区分区域**的上游。
//
// 为什么不能用 newFakeUpstream 的 behavior：它的入参只有 Authorization 头，
// 而本组用例两区账号的 token 值相同，无从区分区域。这里直接看 URL 的 Host
// （见 isIntlHost），与 upstream.chatBase 的真实选路口径一致。
func regionAwareUpstream(t *testing.T, cnBody, intlBody string) *upstream.Client {
	t.Helper()
	return &upstream.Client{
		HTTP: &http.Client{Transport: roundTripFunc(func(r *http.Request) (*http.Response, error) {
			body := cnBody
			if isIntlHost(r.URL.Host) {
				body = intlBody
			}
			return &http.Response{
				StatusCode: http.StatusOK,
				Header:     http.Header{"Content-Type": []string{"application/json"}},
				Body:       io.NopCloser(strings.NewReader(body)),
				Request:    r,
			}, nil
		})},
		ChatBaseCN:    "https://fake.example",
		BillingBaseCN: "https://fake.example",
		BaseIntl:      "https://intl.fake.example",
	}
}

const (
	authCN   = "cn-1"
	authIntl = "intl-1"
)

func cnOnlyPool() *poolAuths {
	return &poolAuths{cn: true}
}

// poolAuths 简化的账号池构造开关。
type poolAuths struct{ cn, intl bool }

// handlerWithAccounts 造一个 handler：账号池按 with 决定有哪些区域，
// 上游按区域返回各自的模型清单。
func handlerWithAccounts(t *testing.T, with poolAuths, cnModels, intlModels []string) *Handler {
	t.Helper()
	resetModelsCache()
	var auths []*auth.Auth
	if with.cn {
		auths = append(auths, &auth.Auth{
			UID: authCN, AccessToken: "t", Domain: "copilot.tencent.com", SoonestExpireAt: 1 << 40,
		})
	}
	if with.intl {
		auths = append(auths, &auth.Auth{
			UID: authIntl, AccessToken: "t", Domain: "www.workbuddy.ai", SoonestExpireAt: 1 << 40,
		})
	}
	return NewHandler(Config{
		Pool:     testPoolWith(auths...),
		Upstream: regionAwareUpstream(t, v3WithModels(cnModels...), v3WithModels(intlModels...)),
		MaxRotate: 1,
	})
}

// entryOf 取某模型在 /v1/models 里的条目。
func entryOf(t *testing.T, h *Handler, id string) map[string]any {
	t.Helper()
	entry := entryByID(listModels(t, h), id)
	if entry == nil {
		t.Fatalf("列表里应有 %s", id)
	}
	return entry
}

// strList 把下发的区域列表读成 []string（JSON 解出来是 []any）。
func strList(v any) []string {
	switch t := v.(type) {
	case []any:
		out := make([]string, 0, len(t))
		for _, x := range t {
			if s, ok := x.(string); ok {
				out = append(out, s)
			}
		}
		return out
	case []string:
		return t
	}
	return nil
}

// ---------------------------------------------------------------------------
// 情形 C：一区有真值、另一区**压根没有真值** → 只能说「无法确认」
// ---------------------------------------------------------------------------

// TestNoIntlAccountDoesNotClaimIntlLacksModel 核心回归：
// 池里没有国际版账号时，**不得**说「该模型仅国服存在」。
func TestNoIntlAccountDoesNotClaimIntlLacksModel(t *testing.T) {
	const model = "deepseek-v4.1-flash"
	// 国服账号在、国际版账号不在；国际版清单即便是空的也拿不到（没账号去拉）。
	h := handlerWithAccounts(t, poolAuths{cn: true}, []string{model}, []string{model})

	entry := entryOf(t, h, model)

	// ① 必须标出「国际版未验证」。
	unverified := strList(entry["unverified_regions"])
	if len(unverified) != 1 || unverified[0] != "intl" {
		t.Fatalf("应下发 unverified_regions=[intl]，实际 %v", entry["unverified_regions"])
	}

	note, _ := entry["region_note"].(string)
	if note == "" {
		t.Fatal("应下发 region_note 说明（信息不能为了清爽而丢）")
	}
	// ② 措辞不得断言「不存在」。
	if strings.Contains(note, "仅在国服上游存在") {
		t.Errorf("没有国际版真值时**不得**断言「仅在国服上游存在」，实际: %s", note)
	}
	// ③ 必须明确说「无法确认」，并点出「不等于没有」。
	if !strings.Contains(note, "无法确认") {
		t.Errorf("应说明「无法确认」，实际: %s", note)
	}
	if !strings.Contains(note, "不等于") {
		t.Errorf("应点明「这不等于国际版没有该模型」，实际: %s", note)
	}
	// ④ 原因要落到「没有账号」这个用户能自己修的点上。
	if !strings.Contains(note, "没有国际版的可用账号") {
		t.Errorf("原因应指出「账号池里没有国际版的可用账号」，实际: %s", note)
	}
	// ⑤ 已知的事实仍要保留，不能因为「不确定」就把信息删掉。
	sup := strList(entry["supported_regions"])
	if len(sup) != 1 || sup[0] != "cn" {
		t.Errorf("应保留 supported_regions=[cn]（已确认的事实），实际 %v", entry["supported_regions"])
	}
	// ⑥ 能力照常下发，不因区域未知而降级。
	if v, ok := entry["supportsImages"].(bool); !ok || !v {
		t.Errorf("区域未知不该影响能力下发，supportsImages=%v", entry["supportsImages"])
	}
}

// TestNoCNAccountDoesNotClaimCNLacksModel 对称情形：没有国服账号时同理。
func TestNoCNAccountDoesNotClaimCNLacksModel(t *testing.T) {
	const model = "gpt-6-astra"
	h := handlerWithAccounts(t, poolAuths{intl: true}, []string{model}, []string{model})

	entry := entryOf(t, h, model)

	unverified := strList(entry["unverified_regions"])
	if len(unverified) != 1 || unverified[0] != "cn" {
		t.Fatalf("应下发 unverified_regions=[cn]，实际 %v", entry["unverified_regions"])
	}
	note, _ := entry["region_note"].(string)
	if strings.Contains(note, "仅在国际版上游存在") {
		t.Errorf("没有国服真值时不得断言「仅在国际版上游存在」，实际: %s", note)
	}
	if !strings.Contains(note, "没有国服的可用账号") {
		t.Errorf("原因应指出缺国服账号，实际: %s", note)
	}
	// 国际版这一侧的事实仍要保留。
	if sup := strList(entry["supported_regions"]); len(sup) != 1 || sup[0] != "intl" {
		t.Errorf("应保留 supported_regions=[intl]，实际 %v", entry["supported_regions"])
	}
}

// TestUnverifiedReasonDistinguishesNoAccountFromFetchFailure
// 「没有账号」与「有账号但没拉到」必须给出不同原因 —— 用户动作不同。
func TestUnverifiedReasonDistinguishesNoAccountFromFetchFailure(t *testing.T) {
	// 情形一：池里没有国际版账号。
	h1 := handlerWithAccounts(t, poolAuths{cn: true}, []string{"m"}, []string{"m"})
	note1, _ := entryOf(t, h1, "m")["region_note"].(string)
	if !strings.Contains(note1, "没有国际版的可用账号") {
		t.Errorf("无账号时原因应为「没有账号」，实际: %s", note1)
	}

	// 情形二：有国际版账号，但**只有国际版**拉取失败（上游 500）。
	//
	// 这里不能用 newFakeUpstream：它的 behavior 只拿到 Authorization 头，
	// 而本用例两区账号的 token 相同，无从区分区域 —— 结果会是两区都失败，
	// 于是「另一区确知缺失」根本构造不出来（实测踩过：region_note 为空）。
	// 改为按 Host 分流：国服 200 且列出 glm-5.3，国际版 500。
	resetModelsCache()
	up := &upstream.Client{
		HTTP: &http.Client{Transport: roundTripFunc(func(r *http.Request) (*http.Response, error) {
			if isIntlHost(r.URL.Host) {
				return &http.Response{
					StatusCode: http.StatusInternalServerError,
					Header:     http.Header{"Content-Type": []string{"application/json"}},
					Body:       io.NopCloser(strings.NewReader(`{"code":500,"msg":"boom"}`)),
					Request:    r,
				}, nil
			}
			return &http.Response{
				StatusCode: http.StatusOK,
				Header:     http.Header{"Content-Type": []string{"application/json"}},
				Body:       io.NopCloser(strings.NewReader(v3WithModels("glm-5.3"))),
				Request:    r,
			}, nil
		})},
		ChatBaseCN:    "https://fake.example",
		BillingBaseCN: "https://fake.example",
		BaseIntl:      "https://intl.fake.example",
	}
	h2 := NewHandler(Config{
		Pool: testPoolWith(
			&auth.Auth{UID: authCN, AccessToken: "t", Domain: "copilot.tencent.com", SoonestExpireAt: 1 << 40},
			&auth.Auth{UID: authIntl, AccessToken: "t", Domain: "www.workbuddy.ai", SoonestExpireAt: 1 << 40},
		),
		Upstream:  up,
		MaxRotate: 1,
	})
	entry2 := entryOf(t, h2, "glm-5.3")
	note2, _ := entry2["region_note"].(string)
	if note2 == note1 {
		t.Errorf("拉取失败与无账号的原因不该是同一句，实际都是: %s", note2)
	}
	if strings.Contains(note2, "没有国际版的可用账号") {
		t.Errorf("有账号但拉取失败时不该说「没有账号」（会把用户引去反复补号），实际: %s", note2)
	}
	if !strings.Contains(note2, "未拉到") {
		t.Errorf("拉取失败时应说明「未拉到」，实际: %s", note2)
	}
	// 关键：有账号但拉取失败 ≠ 确知没有 → 必须标未验证，且不得断言「仅国服存在」。
	if uv := strList(entry2["unverified_regions"]); len(uv) != 1 || uv[0] != "intl" {
		t.Errorf("应下发 unverified_regions=[intl]，实际 %v", entry2["unverified_regions"])
	}
	if strings.Contains(note2, "仅在国际版上游存在") || strings.Contains(note2, "仅在国服上游存在") {
		t.Errorf("拉取失败时不得断言「仅某区存在」，实际: %s", note2)
	}
}

// ---------------------------------------------------------------------------
// 情形 B（现状，必须保留）：两区都有真值，另一区清单里**确实没有**该模型
// ---------------------------------------------------------------------------

// TestConfirmedAbsentRegionStillSaysOnlyOneRegion 反面保护：
// 确知另一区没有该模型时，仍要说「仅某区存在」并保留 11102 成因。
//
// 与被测的新行为成对存在：若只加「未验证」分支而把确认分支也改成含糊措辞，
// 用户就再也不知道 11102 是因为「模型确实不在那一侧」了。
func TestConfirmedAbsentRegionStillSaysOnlyOneRegion(t *testing.T) {
	// 两区各有账号 → 两区真值都拿得到；国服清单没有 intl-only-model，
	// 国际版清单没有 cn-only-model，于是两侧互相**确认**对方没有。
	h := handlerWithAccounts(t, poolAuths{cn: true, intl: true},
		[]string{"shared-model", "cn-only-model"},
		[]string{"shared-model", "intl-only-model"})

	entry := entryOf(t, h, "cn-only-model")

	if len(strList(entry["unverified_regions"])) != 0 {
		t.Fatalf("两区都有真值时不该标未验证，实际 %v", entry["unverified_regions"])
	}
	note, _ := entry["region_note"].(string)
	if !strings.Contains(note, "仅在国服上游存在") {
		t.Errorf("确知国际版没有该模型时应保留「仅国服存在」的断言，实际: %s", note)
	}
	if !strings.Contains(note, "11102") {
		t.Errorf("确知缺失时应保留 11102 的成因说明，实际: %s", note)
	}
	if sup := strList(entry["supported_regions"]); len(sup) != 1 || sup[0] != "cn" {
		t.Errorf("supported_regions 应为 [cn]，实际 %v", entry["supported_regions"])
	}
}

// TestIntlOnlyModelConfirmedAbsentInCN 对称：国际版独有模型说「仅国际版」。
func TestIntlOnlyModelConfirmedAbsentInCN(t *testing.T) {
	h := handlerWithAccounts(t, poolAuths{cn: true, intl: true},
		[]string{"shared-model", "cn-only-model"},
		[]string{"shared-model", "intl-only-model"})

	entry := entryOf(t, h, "intl-only-model")
	if len(strList(entry["unverified_regions"])) != 0 {
		t.Errorf("国际版独有且两区有真值时不该标未验证，实际 %v", entry["unverified_regions"])
	}
	note, _ := entry["region_note"].(string)
	if !strings.Contains(note, "仅在国际版上游存在") {
		t.Errorf("应为「仅国际版存在」，实际: %s", note)
	}
	if !strings.Contains(note, "国服") || !strings.Contains(note, "11102") {
		t.Errorf("应说明国服账号调用会 11102，实际: %s", note)
	}
}

// ---------------------------------------------------------------------------
// 情形 A：两区都确认有 → 不下发任何区域字段
// ---------------------------------------------------------------------------

func TestBothRegionsHaveModelNoRegionFields(t *testing.T) {
	h := handlerWithAccounts(t, poolAuths{cn: true, intl: true},
		[]string{"shared-model"},
		[]string{"shared-model"})

	entry := entryOf(t, h, "shared-model")
	for _, k := range []string{"supported_regions", "region_note", "unverified_regions"} {
		if v, exists := entry[k]; exists {
			t.Errorf("两区都确认有该模型时不该下发 %s，实际 %v", k, v)
		}
	}
}

// TestStaticFallbackDoesNotClaimAbsence 静态兜底表不得用来断言「另一区没有」。
//
// 关键回归：静态表会过期（曾漏了 glm-5.3），用它否定存在会把用户引偏。
// 这里两区账号都没有 → 两区都走静态兜底 → knownRegion 全 false，
// 因此**任何**模型都不该出现「仅某区上游存在」这种断言。
func TestStaticFallbackDoesNotClaimAbsence(t *testing.T) {
	resetModelsCache()
	// 池里有账号但上游恒失败 → 两区都退回静态表。
	up := newFakeUpstream(t, func(string) (int, string, bool) {
		return http.StatusInternalServerError, `{"code":500,"msg":"boom"}`, false
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

	for _, entry := range listModels(t, h) {
		id, _ := entry["id"].(string)
		note, _ := entry["region_note"].(string)
		if strings.Contains(note, "上游存在") {
			t.Errorf("静态兜底（无真值）不得断言 %s「仅在某一侧上游存在」，实际: %s", id, note)
		}
	}
}

// TestNoRouteAccountStillSuppliesRegionTruth 被用户禁用（no_route）的账号**仍要**
// 提供它所在区域的上游真值。
//
// 这是实测踩出来的回归（所有者报「ds flash 国内外明明都有，怎么你又搞成只有国内有了」）：
// 把 NoRoute 加进 pool.healthy() 之后，`pickProbeAccountInRegion` 原先用的
// AvailableUIDs() 就不再返回那些账号 → 拉不到国际版清单 →
// knownRegion[intl]=false → 该模型被标成「仅国服」，而两区其实都有。
//
// 根因：把「不接流量」与「不作为」混为一谈。拉 /v3/config 是**只读探测**，
// 不产生流量也不消耗积分 —— 用户禁用账号是为了不接请求，不是为了让我们
// 连它的区域事实都看不到。
//
// 夹具里国服账号是**正常可用**的：否则「池里一个可接流量的账号都没有」
// 会掩盖真实差异，用例就可能因为别的原因通过。
func TestNoRouteAccountStillSuppliesRegionTruth(t *testing.T) {
	const model = "shared-model"
	resetModelsCache()

	h := NewHandler(Config{
		Pool: testPoolWith(
			&auth.Auth{UID: authCN, AccessToken: "t", Domain: "copilot.tencent.com", SoonestExpireAt: 1 << 40},
			&auth.Auth{
				UID: authIntl, AccessToken: "t", Domain: "www.workbuddy.ai",
				SoonestExpireAt: 1 << 40, NoRoute: true,
			},
		),
		Upstream:  regionAwareUpstream(t, v3WithModels(model), v3WithModels(model)),
		MaxRotate: 1,
	})

	entry := entryOf(t, h, model)

	// 两区都有真值 → 不该下发任何区域字段（尤其**不该**说「仅国服」）。
	if sup := strList(entry["supported_regions"]); len(sup) != 0 {
		t.Errorf("两区都有真值（其中国际版账号被用户禁用）时不该标单区，实际 %v", sup)
	}
	if uv := strList(entry["unverified_regions"]); len(uv) != 0 {
		t.Errorf("被禁用的账号仍能提供真值，不该标未验证，实际 %v", uv)
	}
	if note, _ := entry["region_note"].(string); note != "" {
		t.Errorf("不该有区域说明，实际 %q", note)
	}
}

// TestRegionNoteHelpersSeparateAssertionFromUnverified 两个文案函数语义不得混用。
func TestRegionNoteHelpersSeparateAssertionFromUnverified(t *testing.T) {
	confirmed := regionNote(regionCodeCN)
	if !strings.Contains(confirmed, "仅在国服上游存在") || !strings.Contains(confirmed, "国际版") {
		t.Errorf("regionNote 是**断言**：应说「仅在国服上游存在」并点名国际版，实际: %s", confirmed)
	}
	if strings.Contains(confirmed, "无法确认") {
		t.Errorf("regionNote 不该出现「无法确认」（它是确认后的措辞），实际: %s", confirmed)
	}

	unverified := unverifiedNote(regionCodeCN, regionCodeIntl, regionGapInfo{Cause: KindMissingAccounts, Reason: "账号池里没有国际版的可用账号"})
	if strings.Contains(unverified, "仅在国服上游存在") {
		t.Errorf("unverifiedNote 不得复用断言语，实际: %s", unverified)
	}
	if !strings.Contains(unverified, "无法确认") || !strings.Contains(unverified, "不等于") {
		t.Errorf("unverifiedNote 应说清「无法确认」且「不等于没有」，实际: %s", unverified)
	}
	if !strings.Contains(unverified, "账号池里没有国际版的可用账号") {
		t.Errorf("unverifiedNote 应把原因带上（否则用户不知道该做什么），实际: %s", unverified)
	}
	// 方向必须跟着区域走（别把国服/国际版写反）。
	rev := unverifiedNote(regionCodeIntl, regionCodeCN, regionGapInfo{Cause: KindMissingAccounts, Reason: "账号池里没有国服的可用账号"})
	if !strings.Contains(rev, "已确认国际版") || !strings.Contains(rev, "国服未检测到") {
		t.Errorf("反方向措辞错误，实际: %s", rev)
	}
}

// TestUnverifiedNoteDoesNotTellExistingAccountsToAddAccounts 有账号却被告知「补账号」。
//
// # 所有者 2026-09-20 的现场（这条是回归）
//
//	「还有这个,我明明国内外账号都有,居然还有这个提示 这是个bug」
//
// 他判断对了。旧 `unverifiedNote` **无条件**结尾写「补齐X账号后刷新即可确认」，
// 而他的现场是 `KindFetchFailed`（有 12 个国服账号，只是那一轮上游没拉到）——
// 于是提示让他去"补账号"，而他账号早就够了。**建议指向了不存在的问题**。
//
// 契约：`KindFetchFailed` 时**绝不能**出现"补账号"类措辞，
// 且必须说清"账号够了、是上游暂时不可达、稍后重试"。
func TestUnverifiedNoteDoesNotTellExistingAccountsToAddAccounts(t *testing.T) {
	got := unverifiedNote(regionCodeIntl, regionCodeCN, regionGapInfo{
		Cause:  KindFetchFailed,
		Reason: "国服账号清单本次未拉到（上游暂时不可达，稍后刷新即可）",
	})

	// ① 绝不能建议补账号 —— 那正是本次缺陷
	for _, bad := range []string{"补齐", "补一个", "添加账号", "没有账号", "启用账号"} {
		if strings.Contains(got, bad) {
			t.Errorf("有账号（拉取失败）时不该出现 %q —— "+
				"会让用户去做无用功；实际：%s", bad, got)
		}
	}
	// ② 必须明确说"账号是够的"，让用户不必去查账号
	if !strings.Contains(got, "账号是够的") {
		t.Errorf("应明确告诉用户账号够用（否则他会去查账号池），实际：%s", got)
	}
	// ③ 必须给出**可执行**的下一步
	if !strings.Contains(got, "刷新") {
		t.Errorf("应给出可执行建议（稍后刷新重试），实际：%s", got)
	}
	// ④ 原有信息不能丢：仍要说清"无法确认"，且"不等于没有"
	if !strings.Contains(got, "无法确认") || !strings.Contains(got, "不等于") {
		t.Errorf("仍应说清「无法确认」且「不等于没有」，实际：%s", got)
	}
}

// TestUnverifiedNoteKeepsAddAdviceWhenAccountsReallyMissing 真的没账号时，仍要建议补账号。
//
// 反面：别为了修上面那条，把"真缺账号"这个正例也一起改坏 ——
// 那是唯一一种**用户自己能修好**的成因，建议必须保留。
func TestUnverifiedNoteKeepsAddAdviceWhenAccountsReallyMissing(t *testing.T) {
	got := unverifiedNote(regionCodeIntl, regionCodeCN, regionGapInfo{
		Cause:  KindMissingAccounts,
		Reason: "账号池里没有国服的可用账号",
	})
	if !strings.Contains(got, "补齐") {
		t.Errorf("真的没有该区账号时，应建议补账号，实际：%s", got)
	}
	if strings.Contains(got, "账号是够的") {
		t.Errorf("没有账号时不该说「账号是够的」（自相矛盾），实际：%s", got)
	}
}

// TestRegionGapReasonDistinguishesCauses 成因判定的两种情形要区分开。
//
// 判据是**枚举**而不是文案（文案改一个字，靠 strings.Contains 的判断就失效）。
func TestRegionGapReasonDistinguishesCauses(t *testing.T) {
	// 只有国服账号 → 问国际版的成因，应是「没有该区账号」
	h := handlerWithAccounts(t, poolAuths{cn: true}, []string{"m"}, []string{"m"})
	gapIntl := h.regionGapReason(auth.RegionIntl)
	if gapIntl.Cause != KindMissingAccounts {
		t.Errorf("池里没有国际版账号时，国际版成因应为 KindMissingAccounts，实际 %v（%s）",
			gapIntl.Cause, gapIntl.Reason)
	}
	// 国服有账号 → 若国服也没拉到（上游一直失败），成因是 KindFetchFailed。
	//
	// 用 `handlerWithAccounts` 的默认上游（它会正常返回）时国服是能拉到的，
	// 故这里只断言"有账号时不会是 KindMissingAccounts"——
	// 那正是本次缺陷的判据（有账号却被说成没账号）。
	gapCN := h.regionGapReason(auth.RegionCN)
	if gapCN.Cause == KindMissingAccounts {
		t.Errorf("池里有国服账号，成因不该是 KindMissingAccounts，实际：%s", gapCN.Reason)
	}
}