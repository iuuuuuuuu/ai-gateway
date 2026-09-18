package server

// 思考等级（reasoning effort）的契约（2026-09-18 修订）：
//
//  1. **下发**：/v1/models 要把档位信息告诉客户端，并如实区分三种情形 ——
//     上游声明了范围 / 未声明范围但有默认档 / 什么都没声明。
//  2. **透传**：客户端指定的档位**原样发给上游**，网关既不拦也不改写。
//
// ⚠ 第 2 条曾相反（「不支持就 400 拒绝」），2026-09-18 被实测推翻：
//
//	hy3     声明 [low, high]     → medium / max / minimal / **off** 全部接受
//	glm-5.2 声明 [high, xhigh]  → low / max / medium / **off** 全部接受
//
// 上游对范围外的档照常接受 ⇒ supportedEfforts 只是「界面建议列出哪些」，
// 不是可用范围。据它拦截会误拒**合法**请求，而用户拿到 400 后毫无办法。
//
// 静默改写（原 normalizeReasoningEffort）同样移除：用户明确调 max 却被降成
// high，他只会看到「调了没效果」，日志里那行 downgrade 他看不到。
//
// 现在唯一的权威判据是**上游自己**：它接受的档正常生效，不接受的它报 400，
// 错误原样返回客户端。

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync/atomic"
	"testing"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/upstream"
)

// v3WithEfforts 贴近真实的 /v3/config：三个模型的档位声明各不相同。
//
//	effort-full    声明 4 档 + 默认 high   → 下发全部档位与默认档
//	effort-fixed   只声明 1 档（固定档模型）→ 下发 1 档，无默认档
//	effort-none    整条没有 reasoning      → **不下发任何档位键**
const v3WithEfforts = `{"code":0,"data":{
	"agents":[{"name":"cli","models":["effort-full","effort-fixed","effort-none"]}],
	"models":[
		{"id":"effort-full","maxInputTokens":1000000,"maxOutputTokens":128000,"supportsImages":true,
		 "reasoning":{"effort":"high","supportedEfforts":["low","medium","high","max"]}},
		{"id":"effort-fixed","maxInputTokens":200000,"maxOutputTokens":24000,
		 "reasoning":{"supportedEfforts":["medium"]}},
		{"id":"effort-none","maxInputTokens":200000,"maxOutputTokens":24000}
	]}}`

// effortsHandler 构造一个「动态拉取成功、模型带档位声明」的 handler。
func effortsHandler(t *testing.T, body string) *Handler {
	t.Helper()
	resetModelsCache()
	p := testPoolWith(&auth.Auth{
		UID:             "cn-1",
		AccessToken:     "t",
		Domain:          "copilot.tencent.com",
		SoonestExpireAt: 1 << 40,
	})
	return NewHandler(Config{
		Pool:     p,
		Upstream: newFakeUpstream(t, func(string) (int, string, bool) { return http.StatusOK, body, false }),
	})
}

// ---------------------------------------------------------------------------
// 一、下发
// ---------------------------------------------------------------------------

// TestModelsExposeReasoningEfforts 声明的档位与默认档都要下发。
func TestModelsExposeReasoningEfforts(t *testing.T) {
	h := effortsHandler(t, v3WithEfforts)
	// 先拉一次 /v1/models 让能力缓存填充（校验路径只读缓存，见下）。
	entry := entryByID(listModels(t, h), "effort-full")
	if entry == nil {
		t.Fatal("模型列表里应有 effort-full")
	}

	// 主拼写：OpenAI 风格。
	got, ok := entry["supported_efforts"].([]any)
	if !ok {
		t.Fatalf("应下发 supported_efforts，实际 %#v", entry["supported_efforts"])
	}
	if len(got) != 4 {
		t.Errorf("档位应有 4 个，实际 %d 个：%v", len(got), got)
	}
	// **顺序与值都要保真**：客户端常把第一个当推荐档展示，重排会改变语义。
	want := []string{"low", "medium", "high", "max"}
	for i, w := range want {
		if got[i] != w {
			t.Errorf("第 %d 档应为 %q，实际 %v（顺序必须与上游一致）", i, w, got[i])
		}
	}

	// 默认档：上游的 reasoning.effort，不能拿 efforts[0] 冒充。
	if d, _ := entry["default_effort"].(string); d != "high" {
		t.Errorf("default_effort 应为上游声明的 high，实际 %q", d)
	}

	// 容错拼写：各客户端读的字段名不统一。
	for _, k := range []string{"supportedEfforts", "reasoning_efforts", "reasoningEfforts"} {
		if _, ok := entry[k]; !ok {
			t.Errorf("应同时下发容错拼写 %q", k)
		}
	}
	// OpenRouter 风格嵌套。
	nested, ok := entry["reasoning"].(map[string]any)
	if !ok {
		t.Fatalf("应下发 reasoning 嵌套对象，实际 %#v", entry["reasoning"])
	}
	if _, ok := nested["supported_efforts"]; !ok {
		t.Error("reasoning.supported_efforts 应存在")
	}
	if d, _ := nested["default_effort"].(string); d != "high" {
		t.Errorf("reasoning.default_effort 应为 high，实际 %q", d)
	}
}

// TestModelsOmitEffortsWhenUpstreamSilent 上游没声明档位时**一个键都不写**。
//
// 为什么不能写空数组：客户端会把「有该字段但没档位」当成「该模型没有可用档位」，
// 与「未声明（按自己的默认处理）」是两种语义。谎报会让人以为模型坏了。
func TestModelsOmitEffortsWhenUpstreamSilent(t *testing.T) {
	h := effortsHandler(t, v3WithEfforts)
	entry := entryByID(listModels(t, h), "effort-none")
	if entry == nil {
		t.Fatal("模型列表里应有 effort-none")
	}
	for _, k := range []string{"supported_efforts", "supportedEfforts", "reasoning_efforts", "reasoningEfforts", "reasoning", "default_effort"} {
		if v, ok := entry[k]; ok {
			t.Errorf("上游未声明档位时不该下发 %q，实际 %#v", k, v)
		}
	}
}

// TestModelsOmitDefaultEffortWhenNotDeclared 固定档模型只下发档位、不下发默认档。
//
// 上游没给 reasoning.effort 时网关也不知道默认是哪档，**不要**用 efforts[0] 猜。
func TestModelsOmitDefaultEffortWhenNotDeclared(t *testing.T) {
	h := effortsHandler(t, v3WithEfforts)
	entry := entryByID(listModels(t, h), "effort-fixed")
	if entry == nil {
		t.Fatal("模型列表里应有 effort-fixed")
	}
	if _, ok := entry["supported_efforts"]; !ok {
		t.Error("固定档模型仍应下发它支持的那一档")
	}
	for _, k := range []string{"default_effort", "defaultEffort", "default_reasoning_effort"} {
		if v, ok := entry[k]; ok {
			t.Errorf("上游未声明默认档时不该下发 %q，实际 %#v（不得用 efforts[0] 冒充）", k, v)
		}
	}
}

// TestModelReasoningFieldsCopiesSlice 下发的切片必须是副本。
//
// efforts 来自按区域的模型缓存，是共享切片。直接塞进响应 map 会让调用方
// 对返回值的 in-place 修改污染缓存 —— 下一个请求就会带着被改过的档位列表。
func TestModelReasoningFieldsCopiesSlice(t *testing.T) {
	src := []string{"low", "high"}
	out := modelReasoningFields(src, "")
	if out == nil {
		t.Fatal("有档位时应返回字段")
	}
	list, _ := out["supported_efforts"].([]string)
	if len(list) != 2 {
		t.Fatalf("应有 2 档，实际 %v", list)
	}
	list[0] = "MUTATED"
	if src[0] != "low" {
		t.Errorf("修改下发切片污染了源数据：src[0]=%q", src[0])
	}
}

// TestModelReasoningFieldsEmptyReturnsNil 已被 TestModelReasoningFieldsThreeStates 取代。
//
// 原用例断言 `modelReasoningFields(nil, "high") == nil`（「无档位就不下发任何键」）——
// 那条断言**本身就是缺陷**：它把「上游没给档位数组」等同于「不支持思考」，
// 而实测固定档模型的 effort 是有效的（上面用例的 B 情形）。保留它会把 bug 锁死，
// 故删除；三态语义改由 TestModelReasoningFieldsThreeStates 覆盖。

// hasEffort 判断档位列表里是否含某个值（大小写不敏感）。
func hasEffort(list []string, want string) bool {
	for _, e := range list {
		if strings.EqualFold(strings.TrimSpace(e), want) {
			return true
		}
	}
	return false
}

// TestModelReasoningFieldsThreeStates 思考能力的三种情形必须分开表达。
//
// 这是所有者报的缺陷（「国服还是国际服都是有思考档位的，你这里数据不对吧」）：
// 早先本函数只判 `len(efforts)==0 → return nil`，把「未声明范围」与「不支持思考」
// 混成一件事，于是 18 个（国服）/ 8 个（国际版）模型在界面上显示成「—」。
//
// ⚠ 2026-09-18 二次修正（所有者报「我现在就用的这个模型，用的 max 档位，
// 为什么没有拦截报错？」）：上一版把这种情况标成 `reasoning_fixed=true`
//（「固定单档」）—— 那是**错的**。实测 deepseek-v4.1-flash：
//
//	off / bogus_value → HTTP 400 "the reasoning effort value is not supported"
//	minimal/low/medium/high/max/xhigh → **全部接受**
//	低到高各档推理长度：low 397 → medium 420 → high 646 → max 776（单调递增）
//
// 单调序列不可能由随机性产生 ⇒ **档位真的生效**，用户调 max 确实有效。
// 所以「固定」这个词是谎报（方向与「谎报不支持图片」同样有害：让用户放弃调档）。
//
// 三种情形（2026-09-18 实测上游 /v3/config 的真实形态）：
//
//	A. 有 supportedEfforts       → 如实例出该模型自己的范围（12 国服 / 12 国际版）
//	B. 只有 effort/defaultEffort → **范围未声明**：默认档 X，档位可指定，
//	                               标准阶梯可用（18 国服 / 8 国际版）
//	C. 两者都无                  → 上游没声明任何思考信息，不下发
func TestModelReasoningFieldsThreeStates(t *testing.T) {
	// ---- A. 上游声明了可选档位 ----
	a := modelReasoningFields([]string{"low", "high"}, "high")
	if a == nil {
		t.Fatal("有档位时必须下发字段")
	}
	if list, ok := a["supported_efforts"].([]string); !ok || len(list) != 2 {
		t.Errorf("应下发上游声明的 2 个档位，实际 %#v", a["supported_efforts"])
	}
	if a["reasoning_fixed"] != false {
		t.Errorf("有可选档位时 reasoning_fixed 应为 false，实际 %#v", a["reasoning_fixed"])
	}
	if a["default_effort"] != "high" {
		t.Errorf("默认档应为 high，实际 %#v", a["default_effort"])
	}
	// 声明了范围时**不得**标「范围未声明」——那会让客户端把上游的确切声明
	// 当成「不知道」，从而放开本该拒绝的档位。
	if _, bad := a["reasoning_range_undeclared"]; bad {
		t.Error("上游已声明 supportedEfforts 时不该标 reasoning_range_undeclared")
	}

	// ---- B. 未声明可选范围，但有默认档：**本缺陷的核心** ----
	// 真实形态：reasoning={"effort":"high","summary":"auto"}，无 supportedEfforts。
	b := modelReasoningFields(nil, "high")
	if b == nil {
		t.Fatal("有默认档时必须下发字段 —— 返回 nil 会让界面显示「—」，" +
			"用户以为该模型不能思考")
	}
	if b["reasoning_fixed"] == true {
		t.Error("**不得**标成 fixed：实测该类模型接受整个标准阶梯，" +
			"「固定」会让用户以为调档没用而放弃（所有者正是这么被误导的）")
	}
	if b["reasoning_range_undeclared"] != true {
		t.Errorf("应标 reasoning_range_undeclared=true（范围未声明 ≠ 不可选），实际 %#v",
			b["reasoning_range_undeclared"])
	}
	if b["supports_reasoning"] != true {
		t.Errorf("该类模型确实支持思考，supports_reasoning 应为 true，实际 %#v", b["supports_reasoning"])
	}
	if b["default_effort"] != "high" {
		t.Errorf("必须下发默认档，实际 %#v", b["default_effort"])
	}
	// 必须给出可选档位（候选阶梯）——空列表会让客户端渲染出没有选项的控件，
	// 用户只能看到「固定」而无法尝试任何档位，那正是本缺陷的表现。
	list, ok := b["supported_efforts"].([]string)
	if !ok || len(list) < 4 {
		t.Fatalf("应下发候选档位阶梯，实际 %#v", b["supported_efforts"])
	}
	// 顺序必须稳定且由低到高：map 遍历顺序随机，不排序会让客户端下拉框顺序跳变。
	wantOrder := []string{"off", "minimal", "low", "medium", "high", "xhigh", "max"}
	if len(list) != len(wantOrder) {
		t.Errorf("候选阶梯应有 %d 档，实际 %d：%v", len(wantOrder), len(list), list)
	} else {
		for i := range wantOrder {
			if list[i] != wantOrder[i] {
				t.Errorf("阶梯顺序应由低到高 %v，实际 %v", wantOrder, list)
				break
			}
		}
	}
	// ⚠ off 必须**在**列表里。此前我把它删了，理由是「deepseek-v4.1-flash
	// 拒绝 off（HTTP 400）」—— 那是从**单模型单次观测**推到「一类模型」。
	// 逐模型实测（16 个国服模型 × 7 档）显示 off 在 14/16 个模型上被接受，
	// 只有两个 deepseek 模型拒绝。删掉它会让那 14 个模型的用户少一个可用档位。
	if !hasEffort(list, "off") {
		t.Error("候选阶梯应包含 off —— 实测 14/16 个模型接受它，" +
			"删掉会让多数模型的用户少一个可用档位（我此前正是这么错的）")
	}
	// 必须标注**哪些档有上游清单背书**：off/minimal 从未出现在任何模型的
	// supportedEfforts 里，只有我们的实测结果 —— 客户端据此措辞更保守。
	backed, ok := b["upstream_declared_efforts"].(map[string]bool)
	if !ok {
		t.Fatalf("应下发 upstream_declared_efforts 标注，实际 %#v", b["upstream_declared_efforts"])
	}
	if !backed["high"] || !backed["low"] {
		t.Errorf("low/high 有上游清单背书，应为 true，实际 %#v", backed)
	}
	if backed["off"] || backed["minimal"] {
		t.Errorf("off/minimal 从未被上游声明过，应为 false，实际 %#v", backed)
	}

	// ---- C. 完全无思考信息：不下发 ----
	if got := modelReasoningFields(nil, ""); got != nil {
		t.Errorf("无档位且无默认档时应返回 nil，实际 %#v", got)
	}
	if got := modelReasoningFields([]string{}, ""); got != nil {
		t.Errorf("空切片且无默认档应返回 nil，实际 %#v", got)
	}

	// ---- D. 默认档是 defaultEffort 拼写时同样成立 ----
	// 上游 12 个多档模型用的是 `defaultEffort` 键（见 ModelInfo.DefaultEffort），
	// upstream 层合并后这里拿到的是同一个值，行为不该因拼写而变。
	d := modelReasoningFields([]string{"low", "high", "max"}, "max")
	if d["default_effort"] != "max" {
		t.Errorf("defaultEffort 拼写应同样生效，实际 %#v", d["default_effort"])
	}
}

// ---------------------------------------------------------------------------
// 二、透传：档位**一律原样发给上游**，网关既不拦也不改写
//
// ⚠ 这一节在 2026-09-18 被**整体反转**，原内容是「不支持就拒绝」。
//
// 原前提：supportedEfforts 是该模型的**硬范围**，范围外的档该拒。
// 实测推翻（真实流式调用，国服账号）：
//
//	hy3     声明 [low, high]     → medium / max / minimal / **off** 全部接受
//	glm-5.2 声明 [high, xhigh]  → low / max / medium / **off** 全部接受
//
// 上游对范围外的档照常接受 ⇒ supportedEfforts 只是「界面建议列出哪些」，
// 不是可用范围。据它拦截会把**合法请求**拒掉，而且用户拿到 400 后毫无办法
//（他用的档其实能用）—— 比漏拦严重得多。
//
// 现在网关的职责收敛为**如实透传**：
//   · 上游接受的档 → 正常生效（实测 deepseek-v4.1-flash low→max 推理单调递增）
//   · 上游不接受的档 → 上游自己报 400，错误原样返回（唯一权威判据）
//
// 这几条用例锁住「网关不再自作主张」这个性质。
// ---------------------------------------------------------------------------

// TestUndeclaredEffortRangePassesThrough 未声明范围的档位照常放行到上游。
//
// 这正是所有者报的那个现象：他用 max 档、没被拦截 —— **那是对的**。
// 本条把它固化成契约，防止有人再按 supportedEfforts 加回拦截。
func TestUndeclaredEffortRangePassesThrough(t *testing.T) {
	var calls int32
	resetModelsCache()
	p := testPoolWith(
		&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999, Domain: "copilot.tencent.com", SoonestExpireAt: 1 << 40},
	)
	h := NewHandler(Config{
		Pool: p,
		Upstream: newFakeUpstream(t, func(string) (int, string, bool) {
			atomic.AddInt32(&calls, 1)
			return http.StatusOK, "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n", true
		}),
	})
	listModels(t, h)
	before := atomic.LoadInt32(&calls)

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(`{"model":"effort-fixed","reasoning_effort":"max","messages":[{"role":"user","content":"hi"}]}`)))

	if rec.Code == http.StatusBadRequest {
		t.Errorf("档位不该被网关拒绝（supportedEfforts 不是硬范围），实际 400：%s", rec.Body.String())
	}
	if n := atomic.LoadInt32(&calls) - before; n == 0 {
		t.Errorf("应放行到上游，实际没打上游；响应=%d %s", rec.Code, rec.Body.String())
	}
}

// TestEffortNotRewrittenInPayload 网关**不得改写** reasoning_effort。
//
// 除了「不拦」，还要「不改」：曾经的 normalizeReasoningEffort 会把它悄悄
// 降级成 supportedEfforts 里最接近的档，用户看到「调了 max 却答得很短」，
// 日志里只有一行 downgrade —— 参数被改了却无从知晓。
func TestEffortNotRewrittenInPayload(t *testing.T) {
	const body = `{"model":"effort-fixed","reasoning_effort":"max","messages":[{"role":"user","content":"hi"}]}`

	// efforts 表里 effort-fixed 只声明了 medium —— 旧实现会把它改成 medium。
	got := upstream.PrepareBodyOptWithEfforts([]byte(body), false,
		map[string][]string{"effort-fixed": {"medium"}})

	var out map[string]any
	if err := json.Unmarshal(got, &out); err != nil {
		t.Fatalf("改写后的 body 不是 JSON: %v", err)
	}
	if out["reasoning_effort"] != "max" {
		t.Errorf("reasoning_effort 必须原样透传，实际被改成 %#v", out["reasoning_effort"])
	}
}

// TestEffortPassthroughKeepsSnakeAndCamel 两种拼写都原样保留。
//
// 客户端写法不统一（`reasoning_effort` / `reasoningEffort` 都出现过），
// 透传时不能只保一个 —— 那会让另一种拼写的用户发现参数被丢了。
func TestEffortPassthroughKeepsSnakeAndCamel(t *testing.T) {
	for _, key := range []string{"reasoning_effort", "reasoningEffort"} {
		body := `{"model":"m","` + key + `":"xhigh","messages":[{"role":"user","content":"hi"}]}`
		got := upstream.PrepareBodyOptWithEfforts([]byte(body), false,
			map[string][]string{"m": {"low"}})
		var out map[string]any
		if err := json.Unmarshal(got, &out); err != nil {
			t.Fatalf("[%s] 改写后的 body 不是 JSON: %v", key, err)
		}
		if out[key] != "xhigh" {
			t.Errorf("[%s] 应原样透传 xhigh，实际 %#v", key, out[key])
		}
	}
}

// TestEffortsForModelReadsCacheOnly 校验路径**只读缓存、不触发上游拉取**。
//
// 这是实测踩到的坑：最初的实现调 buildCapabilityIndex()，缓存未命中时它会真的
// 去打上游（fetchModelsForRegion → FetchModels），于是
// TestContextTooLongDoesNotRotateAccounts 的「只打上游 1 次」变成 2 次。
// 除了让计数断言无故失败，生产上还意味着**每个冷启动后的首个请求都要先等一次
// 模型拉取**。
func TestEffortsForModelReadsCacheOnly(t *testing.T) {
	var calls int32
	resetModelsCache()
	p := testPoolWith(&auth.Auth{UID: "u1", AccessToken: "at1", Domain: "copilot.tencent.com", SoonestExpireAt: 1 << 40})
	h := NewHandler(Config{
		Pool: p,
		Upstream: newFakeUpstream(t, func(string) (int, string, bool) {
			atomic.AddInt32(&calls, 1)
			return http.StatusOK, v3WithEfforts, false
		}),
	})

	// 缓存空时：不拉取，返回空（= 无从校验，放行）。
	if got := h.effortsForModel("effort-full"); len(got) != 0 {
		t.Errorf("缓存空时应返回空档位，实际 %v", got)
	}
	if n := atomic.LoadInt32(&calls); n != 0 {
		t.Errorf("只读缓存不该触发上游拉取，实际打了 %d 次", n)
	}

	// 填充缓存后：能读到档位，且仍不额外拉取。
	listModels(t, h)
	after := atomic.LoadInt32(&calls)
	got := h.effortsForModel("effort-full")
	if len(got) != 4 {
		t.Errorf("缓存填充后应读到 4 档，实际 %v", got)
	}
	if n := atomic.LoadInt32(&calls) - after; n != 0 {
		t.Errorf("读缓存不该再拉取，实际又打了 %d 次", n)
	}
}
