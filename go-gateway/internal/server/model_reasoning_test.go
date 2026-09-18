package server

// 思考等级（reasoning effort）的两条契约：
//
//  1. **下发**：/v1/models 要把上游声明的档位告诉客户端（此前 reasoning.effort
//     被解析进来却全树无消费方，客户端看不到任何档位信息）。
//  2. **校验**：客户端显式指定的档位不被该模型支持时**直接拒绝**（400 +
//     unsupported_reasoning_effort），而不是静默降级。
//
// 第 2 条是本项目对上游行为的**有意收紧**。原作者的做法是静默降级
//（`normalizeReasoningEffort`：把 max 改写成 ≤max 的最高支持档，或在
//「支持档全部高于请求档」时取最低档），只在日志里留一行 downgraded。
// 那在客户端看来是「我明明调了 max，回答却很短」——用户既不知道发生了什么，
// 也不知道该改成哪个档，只能反复试。既然档位是**显式**指定的，不支持时就该
// 如实报错并列出支持哪些档。
//
// 静默降级本身没有删除：它仍服务于「档位名无法识别」之外的场景（例如
// 客户端从别处抄来一个该模型没有的档位），但**显式且已知的档位**不再被悄悄改写。

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync/atomic"
	"testing"

	"workbuddy2api/internal/auth"
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

// TestModelReasoningFieldsThreeStates 思考能力的三种情形必须分开表达。
//
// 这是所有者报的缺陷（「国服还是国际服都是有思考档位的，你这里数据不对吧」）：
// 早先本函数只判 `len(efforts)==0 → return nil`，把「固定档」与「不支持思考」
// 混成一件事，于是 18 个（国服）/ 8 个（国际版）**有思考能力、只是不可选档**
// 的模型在界面上显示成「—」。
//
// 三种情形（均为 2026-09-18 实测上游 /v3/config 的真实形态）：
//
//	有 supportedEfforts       → 列出档位（12 国服 / 12 国际版）
//	只有 effort/defaultEffort → 支持思考但只有固定一档（18 国服 / 8 国际版）
//	两者都无                  → 才是真的不支持思考，不下发
func TestModelReasoningFieldsThreeStates(t *testing.T) {
	// ---- A. 有可选档位 ----
	a := modelReasoningFields([]string{"low", "high"}, "high")
	if a == nil {
		t.Fatal("有档位时必须下发字段")
	}
	if list, ok := a["supported_efforts"].([]string); !ok || len(list) != 2 {
		t.Errorf("应下发 2 个档位，实际 %#v", a["supported_efforts"])
	}
	if a["reasoning_fixed"] != false {
		t.Errorf("有可选档位时 reasoning_fixed 应为 false，实际 %#v", a["reasoning_fixed"])
	}
	if a["default_effort"] != "high" {
		t.Errorf("默认档应为 high，实际 %#v", a["default_effort"])
	}

	// ---- B. 只有固定档：**这是本缺陷的核心** ----
	// 真实形态：reasoning={"effort":"high","summary":"auto"}，无 supportedEfforts。
	b := modelReasoningFields(nil, "high")
	if b == nil {
		t.Fatal("只有固定档时必须下发字段 —— 返回 nil 会让界面显示「—」，" +
			"用户以为该模型不能思考（这正是所有者报的缺陷）")
	}
	if b["reasoning_fixed"] != true {
		t.Errorf("固定档应标 reasoning_fixed=true，实际 %#v", b["reasoning_fixed"])
	}
	if b["supports_reasoning"] != true {
		t.Errorf("固定档也是「支持思考」，supports_reasoning 应为 true，实际 %#v", b["supports_reasoning"])
	}
	if b["default_effort"] != "high" {
		t.Errorf("固定档必须下发它是哪一档，实际 %#v", b["default_effort"])
	}
	// 可选项为空 —— 不能伪造一个 ["high"] 的单元素列表：
	// 那会让客户端渲染出「只有一个选项的下拉框」，暗示可以选择。
	if list, ok := b["supported_efforts"].([]string); !ok || len(list) != 0 {
		t.Errorf("固定档的可选档位应为**空列表**（区别于字段缺失），实际 %#v", b["supported_efforts"])
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
// 二、校验：不支持就拒绝
// ---------------------------------------------------------------------------

// TestUnsupportedEffortRejectedBeforeUpstream 请求不支持的档位时直接 400，
// **且一次上游都不打**（请求侧错误，换号重试无意义）。
func TestUnsupportedEffortRejectedBeforeUpstream(t *testing.T) {
	var calls int32
	resetModelsCache()
	p := testPoolWith(
		&auth.Auth{UID: "u1", AccessToken: "at1", Domain: "copilot.tencent.com", SoonestExpireAt: 1 << 40},
		&auth.Auth{UID: "u2", AccessToken: "at2", Domain: "copilot.tencent.com", SoonestExpireAt: 1 << 40},
		&auth.Auth{UID: "u3", AccessToken: "at3", Domain: "copilot.tencent.com", SoonestExpireAt: 1 << 40},
	)
	h := NewHandler(Config{
		Pool:      p,
		MaxRotate: 3,
		Upstream: newFakeUpstream(t, func(string) (int, string, bool) {
			atomic.AddInt32(&calls, 1)
			return http.StatusOK, v3WithEfforts, false
		}),
	})

	// 先拉一次模型清单填充缓存（正常使用时由 /v1/models 或上一次请求填好）。
	listModels(t, h)
	before := atomic.LoadInt32(&calls)

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(`{"model":"effort-fixed","reasoning_effort":"max","messages":[{"role":"user","content":"hi"}]}`)))

	if rec.Code != http.StatusBadRequest {
		t.Errorf("不支持的档位应返回 400，实际 %d（body=%s）", rec.Code, rec.Body.String())
	}
	if n := atomic.LoadInt32(&calls) - before; n != 0 {
		t.Errorf("请求侧错误不该打上游（换号无用），实际打了 %d 次", n)
	}

	var payload map[string]any
	if err := json.Unmarshal(rec.Body.Bytes(), &payload); err != nil {
		t.Fatalf("响应不是 JSON: %v", err)
	}
	errObj, _ := payload["error"].(map[string]any)
	if code, _ := errObj["code"].(string); code != "unsupported_reasoning_effort" {
		t.Errorf("错误码应为 unsupported_reasoning_effort，实际 %q", code)
	}
	// 文案必须列出**支持哪些档**，否则用户没法改对，只能挨个试。
	msg, _ := errObj["message"].(string)
	if !strings.Contains(msg, "medium") {
		t.Errorf("文案应列出支持档 medium，实际 %q", msg)
	}
	if !strings.Contains(msg, "max") {
		t.Errorf("文案应指出收到的档位 max，实际 %q", msg)
	}
}

// TestSupportedEffortPassesThrough 支持的档位正常放行到上游。
func TestSupportedEffortPassesThrough(t *testing.T) {
	var calls int32
	resetModelsCache()
	// ExpiresAt 必须给未来值：缺省会被判成「需刷新」，账号在选号阶段就被刷掉，
	// 表现为 503 no_healthy_account（实测踩到，与档位校验无关）。
	p := testPoolWith(&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999, Domain: "copilot.tencent.com", SoonestExpireAt: 1 << 40})
	h := NewHandler(Config{
		Pool: p,
		Upstream: newFakeUpstream(t, func(string) (int, string, bool) {
			atomic.AddInt32(&calls, 1)
			// 走到上游即证明放行；返回一个正常 SSE 让请求成功。
			return http.StatusOK, "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n", true
		}),
	})
	listModels(t, h)
	before := atomic.LoadInt32(&calls)

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(`{"model":"effort-fixed","reasoning_effort":"medium","messages":[{"role":"user","content":"hi"}]}`)))

	if rec.Code == http.StatusBadRequest {
		t.Errorf("支持的档位不该被拒，实际 400：%s", rec.Body.String())
	}
	if n := atomic.LoadInt32(&calls) - before; n == 0 {
		t.Errorf("支持的档位应放行到上游，实际没打上游；响应=%d %s", rec.Code, rec.Body.String())
	}
}

// TestEffortCheckAllowsWhenUnknown 未知情形一律放行（**不能拦**）。
//
// 四种放行情形逐一覆盖：没带字段、空串、模型不在能力表、上游未声明档位。
// 拦错任何一个都会让本可用的请求 400 —— 比漏拦严重得多。
func TestEffortCheckAllowsWhenUnknown(t *testing.T) {
	supported := []string{"low", "high"}

	cases := []struct {
		name      string
		requested any
	}{
		{"没带该字段（用默认档）", nil},
		{"空串", ""},
		{"纯空白", "   "},
		{"值不是字符串（交给上游报类型错）", 3},
	}
	for _, c := range cases {
		if err := checkRequestedEffort("m", c.requested, supported); err != nil {
			t.Errorf("%s：应放行，实际被拒 %v", c.name, err)
		}
	}

	// 上游未声明档位 → 无从校验 → 放行。
	if err := checkRequestedEffort("m", "max", nil); err != nil {
		t.Errorf("上游未声明档位时应放行，实际被拒 %v", err)
	}
	if err := checkRequestedEffort("m", "max", []string{}); err != nil {
		t.Errorf("空档位列表应放行，实际被拒 %v", err)
	}
}

// TestEffortCheckIsCaseAndSpaceInsensitive 大小写与空白不敏感。
//
// 客户端写法并不统一（`High` / ` high ` 都出现过）。若按字面比较，
// 这些请求会被误判成「不支持」而 400。
func TestEffortCheckIsCaseAndSpaceInsensitive(t *testing.T) {
	supported := []string{"Low", "High"}
	for _, req := range []string{"high", "HIGH", "High", " high ", "  HIGH  "} {
		if err := checkRequestedEffort("m", req, supported); err != nil {
			t.Errorf("档位 %q 应被认作支持，实际被拒 %v", req, err)
		}
	}
}

// TestEffortCheckRejectsUnrecognisedName 档位名本身不认识时也拒绝。
//
// 那种拼写错误此前被原样透传给上游，上游多半静默忽略 —— 用户同样看不到原因，
// 表现和「降级」一样（调了没生效）。文案要与「认识但不被该模型支持」区分开。
func TestEffortCheckRejectsUnrecognisedName(t *testing.T) {
	err := checkRequestedEffort("m", "highest", []string{"low", "high"})
	if err == nil {
		t.Fatal("无法识别的档位名应被拒")
	}
	var ue *unsupportedEffortError
	if !asUnsupported(err, &ue) {
		t.Fatalf("应是 unsupportedEffortError，实际 %T", err)
	}
	if !ue.unknown {
		t.Error("应标记为「档位名无法识别」，以便文案给出不同提示")
	}
	if !strings.Contains(ue.Error(), "high") {
		t.Errorf("文案应列出支持档，实际 %q", ue.Error())
	}
}

// asUnsupported 小工具：避免在测试里直接 import errors 只为一次 As。
func asUnsupported(err error, target **unsupportedEffortError) bool {
	ue, ok := err.(*unsupportedEffortError)
	if ok {
		*target = ue
	}
	return ok
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
