package qoder

// 未知模型名的守卫（2026-09-28）。
//
// # 要证明的核心命题
//
// 「上游**确实没有**这个模型时，网关在**发请求之前**就拒绝」。
//
// # 为什么必须拒绝而不是透传
//
// Qoder 上游对不认识的模型 key **不报错** —— 静默回退到免费通道、HTTP 200
// 正常返回内容。伪造的 `zzz-not-a-real-model-xyz` 与真实模型得到**逐项相同**
// 的响应画像。于是用户配的假模型名被放行，用量统计里出现
// 「deepseek-v4.1-flash · Qoder」的**幽灵数据**。
//
// # 为什么判据是清单而不是计费字段
//
//	· billable/credits 会**误杀合法免费模型**（qfmodel 实测 billable=false）
//	· billable 只在最后一个 usage 帧出现，流式请求那时正文早已发出
//
// 故只用**上游模型清单**判定。
//
// # 两个「不能拦」的例外（各有专门用例钉住）
//
//	① 清单拉取失败（len==0）⇒ 我们不知道上游有什么 ⇒ 原样透传
//	② `qmodel` 前缀 ⇒ 文档化逃生口，不参与校验
import (
	"context"
	"errors"
	"io"
	"net/http"
	"strings"
	"sync"
	"testing"
	"time"

	"workbuddy2api/internal/auth"
)

// guardModelListJSON 一份"上游模型清单"的假响应（形状与真响应一致）。
//
//	key=dfmodel        display=DeepSeek-Flash        → 归一化 deepseek-flash
//	key=qmodel_preview display=Qwen3.8-Max-Preview   → 归一化 qwen3.8-max-preview
const guardModelListJSON = `{"chat":[` +
	`{"key":"qmodel_preview","display_name":"Qwen3.8-Max-Preview","enable":true},` +
	`{"key":"dfmodel","display_name":"DeepSeek-Flash","enable":true},` +
	`{"key":"disabled-one","display_name":"Retired","enable":false}]}`

// guardTransport 记录请求路径的假 transport。
//
// 它同时承担两个断言职责：
//  1. 模型清单拉取 → 回假清单
//  2. **对话请求绝不该发生** → 记下路径，由用例断言"没有任何对话请求"
type guardTransport struct {
	mu    sync.Mutex
	paths []string
	// listStatus 非 0 时模型清单接口回该状态（模拟拉取失败）。
	listStatus int
}

func (t *guardTransport) RoundTrip(req *http.Request) (*http.Response, error) {
	t.mu.Lock()
	t.paths = append(t.paths, req.URL.Path)
	t.mu.Unlock()

	hdr := http.Header{"Content-Type": []string{"application/json"}}
	if strings.Contains(req.URL.Path, "/model/list") {
		if t.listStatus != 0 {
			return &http.Response{
				StatusCode: t.listStatus, Header: hdr,
				Body: io.NopCloser(strings.NewReader(`{"msg":"boom"}`)),
			}, nil
		}
		return &http.Response{
			StatusCode: http.StatusOK, Header: hdr,
			Body: io.NopCloser(strings.NewReader(guardModelListJSON)),
		}, nil
	}
	// 走到这里说明网关**发了对话请求** —— 本用例集里这一定是缺陷。
	return &http.Response{
		StatusCode: http.StatusInternalServerError, Header: hdr,
		Body: io.NopCloser(strings.NewReader(`{"msg":"对话请求不该被发出"}`)),
	}, nil
}

// chatPaths 返回被请求过的**对话**路径（排除模型清单）。
func (t *guardTransport) chatPaths() []string {
	t.mu.Lock()
	defer t.mu.Unlock()
	var out []string
	for _, p := range t.paths {
		if !strings.Contains(p, "/model/list") {
			out = append(out, p)
		}
	}
	return out
}

// guardDispatch 造一个用假 transport 的 Dispatch（凭证带齐机器指纹）。
func guardDispatch(tr *guardTransport) *Dispatch {
	return NewDispatch(&Client{
		HTTP:    &http.Client{Transport: tr},
		Timeout: 5 * time.Second,
	})
}

// guardCred 造一份可用的 Qoder 凭证（指纹齐备 ⇒ 签名层能构造成功）。
func guardCred() *Cred {
	return &Cred{
		UID: "guard-uid", DT: "dt-guard", DRT: "drt-guard",
		MachineID: "machine-id", MachineToken: "machine-token", MachineType: "5",
		Region: RegionCN,
	}
}

// guardModels 直接构造清单（纯函数用例用）。
func guardModels() []DynamicModel {
	return []DynamicModel{
		{Key: "qmodel_preview", DisplayName: "Qwen3.8-Max-Preview", Enable: true},
		{Key: "dfmodel", DisplayName: "DeepSeek-Flash", Enable: true},
	}
}

// TestResolveInListRejectsUnknownModel 核心用例：清单里没有它 ⇒ ok=false。
//
// 这正是所有者现场的那个模型（`qoder:deepseek-v4.1-flash`）。
func TestResolveInListRejectsUnknownModel(t *testing.T) {
	key, ok := resolveInList(guardModels(), "deepseek-v4.1-flash")
	if ok {
		t.Fatalf("上游清单里没有 deepseek-v4.1-flash，应判为不存在（ok=false），"+
			"实际 ok=true key=%q —— 放过去会被上游静默回退到免费通道，"+
			"产生不属于该模型的幽灵用量", key)
	}
	if key != "" {
		t.Errorf("拒绝时不应给出 key，实际 %q", key)
	}
}

// TestResolveInListAcceptsRawKey 上游 key 原样命中。
func TestResolveInListAcceptsRawKey(t *testing.T) {
	key, ok := resolveInList(guardModels(), "dfmodel")
	if !ok {
		t.Fatal("清单里有 dfmodel，应放行")
	}
	if key != "dfmodel" {
		t.Errorf("应返回同一个 key，实际 %q", key)
	}
}

// TestResolveInListAcceptsDisplayName 规范化显示名映射仍生效。
func TestResolveInListAcceptsDisplayName(t *testing.T) {
	key, ok := resolveInList(guardModels(), "DeepSeek-Flash")
	if !ok {
		t.Fatal("显示名 DeepSeek-Flash 应映射到 dfmodel 并放行")
	}
	if key != "dfmodel" {
		t.Errorf("应返回 dfmodel，实际 %q", key)
	}
}

// TestResolveInListPassthroughWhenListUnavailable 清单拉取失败 ⇒ **不能拦**。
//
// 拿不到权威信息时原样透传：拦下去会让一次网络抖动/令牌过期变成
// 「所有模型都不可用」，故障面被我们自己放大。
func TestResolveInListPassthroughWhenListUnavailable(t *testing.T) {
	key, ok := resolveInList(nil, "some-model-name")
	if !ok {
		t.Fatal("清单为空（拉取失败）时**不许拦** —— 我们不知道上游有什么，" +
			"原样透传把判断权交回上游")
	}
	if key != "some-model-name" {
		t.Errorf("清单不可用时应原样透传，实际 %q", key)
	}
}

// TestResolveInListEmptyModelUsesFirst 空模型名不报错（用清单第一个）。
func TestResolveInListEmptyModelUsesFirst(t *testing.T) {
	key, ok := resolveInList(guardModels(), "")
	if !ok {
		t.Fatal("空模型名不该被判成「模型不存在」（它是「没指定」，走清单第一个）")
	}
	if key != "qmodel_preview" {
		t.Errorf("应取清单第一个 key，实际 %q", key)
	}
	// 清单为空时也不能拦（没有第一个可用，退回空 key 让上游决定）
	if key, ok := resolveInList(nil, ""); !ok || key != "" {
		t.Errorf("清单为空 + 空模型名应返回 (\"\", true)，实际 (%q, %v)", key, ok)
	}
}

// TestResolveInListKeepsQmodelEscapeHatch `qmodel` 前缀是文档化逃生口。
//
// 上游随时可能上线我们清单里还没有的 key，而清单有 10 分钟缓存 ——
// 没有这个口子，新模型在缓存过期前完全不可用。
func TestResolveInListKeepsQmodelEscapeHatch(t *testing.T) {
	// 即便清单里**没有**它，也必须放行
	key, ok := resolveInList(guardModels(), "qmodel_brand_new")
	if !ok {
		t.Fatal("qmodel 前缀是文档化的逃生口，不该被校验拦下")
	}
	if key != "qmodel_brand_new" {
		t.Errorf("应原样透传，实际 %q", key)
	}
}

// TestResolveInListNormalizeVariants 大小写/空格/下划线变体都能命中。
func TestResolveInListNormalizeVariants(t *testing.T) {
	for _, in := range []string{
		"deepseek-flash", "DeepSeek-Flash", "DEEPSEEK_FLASH",
		"deepseek flash", "DeepSeek_Flash", "  DeepSeek-Flash  ",
	} {
		key, ok := resolveInList(guardModels(), in)
		if !ok || key != "dfmodel" {
			t.Errorf("变体 %q 应命中 dfmodel，实际 (%q, %v)", in, key, ok)
		}
	}
}

// TestResolveModelKeyRejectsOnEveryCall 缓存命中的**后续**请求也必须拦。
//
// # 为什么专门锁这条（Lead 指出的实现陷阱）
//
// 旧快速路径直接查映射表 `cache.m` 命中就 return，**根本不拿清单**。
// 若只把校验放在"缓存未命中 → 刷新"那条路径上，同一个假模型会
// **第一次请求被拦、之后 10 分钟内全部放行**（缓存命中跳过校验），
// 对用户就是"有时拦有时不拦" —— 而且极难在单测里发现。
//
// 故这里连续调用两次，第二次**必定走缓存**，仍须 ok=false。
func TestResolveModelKeyRejectsOnEveryCall(t *testing.T) {
	tr := &guardTransport{}
	d := guardDispatch(tr)
	cr := guardCred()
	ctx := context.Background()

	// 第一次：缓存为空 ⇒ 走 refreshModels 拉清单
	_, models, ok := d.resolveModelKey(ctx, cr, "deepseek-v4.1-flash")
	if ok {
		t.Fatal("第一次请求就该被拦（清单已成功拉取且没有这个模型）")
	}
	if len(models) == 0 {
		t.Fatal("拒绝时必须带着可用清单（错误文案要列出上游实际有什么）")
	}

	// 第二次：缓存已填充 ⇒ 走快速路径。**仍然必须拦**。
	_, models2, ok2 := d.resolveModelKey(ctx, cr, "deepseek-v4.1-flash")
	if ok2 {
		t.Fatal("第二次请求走了缓存快速路径却放行了 —— " +
			"这正是「第一次拦、之后 10 分钟全放行」的缺陷形状")
	}
	if len(models2) == 0 {
		t.Fatal("缓存命中时也必须能拿到完整清单（否则错误文案列不出可用模型）")
	}

	// 反面：清单里真实存在的模型，两次都要放行
	if _, _, ok := d.resolveModelKey(ctx, cr, "dfmodel"); !ok {
		t.Fatal("清单里的模型不该被拦")
	}
	if _, _, ok := d.resolveModelKey(ctx, cr, "dfmodel"); !ok {
		t.Fatal("清单里的模型（缓存路径）不该被拦")
	}
}

// TestChatStreamRejectsBeforeSendingRequest 拒绝必须发生在**发请求之前**。
//
// 判据是假 transport 记录的路径：只应有模型清单那一发，
// **绝不能出现任何对话请求**。
func TestChatStreamRejectsBeforeSendingRequest(t *testing.T) {
	tr := &guardTransport{}
	d := guardDispatch(tr)

	a := &auth.Auth{
		UID: "guard-uid", Product: auth.ProductQoder,
		AccessToken: "dt-guard", RefreshToken: "drt-guard",
		// 到期时间放在远处，避免触发令牌刷新（那会多出一发请求）
		ExpiresAt: time.Now().Add(24 * time.Hour).Unix(),
		Domain:    "qoder.com.cn",
	}
	body := []byte(`{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"hi"}]}`)

	rc, status, respBody, err := d.ChatStream(context.Background(), a, body)
	if err == nil {
		t.Fatal("上游没有这个模型，ChatStream 必须返回错误（而不是发请求拿一个假成功）")
	}
	var mnf *ModelNotFoundError
	if !errors.As(err, &mnf) {
		t.Fatalf("错误类型应为 *ModelNotFoundError，实际 %T: %v", err, err)
	}
	if mnf.Model != "deepseek-v4.1-flash" {
		t.Errorf("错误里应带客户端请求的模型名，实际 %q", mnf.Model)
	}
	if mnf.Region != RegionCN {
		t.Errorf("错误里应带判定所用区域，实际 %q", mnf.Region)
	}
	if len(mnf.Available) == 0 {
		t.Error("错误里应带上游可用模型清单（供文案展示出路）")
	}
	// 清单已排序 ⇒ 文案顺序稳定
	if !sortedStrings(mnf.Available) {
		t.Errorf("可用清单应已排序（否则同一条错误每次显示顺序都不同），实际 %v", mnf.Available)
	}
	// 不存在的模型不该出现在可用清单里
	for _, k := range mnf.Available {
		if k == "deepseek-v4.1-flash" {
			t.Error("可用清单里不该出现被判不存在的模型")
		}
	}
	if rc != nil || respBody != nil {
		t.Error("拒绝时不该返回任何流或响应体")
	}
	if status != 0 {
		t.Errorf("拒绝时不该给出上游状态码，实际 %d", status)
	}
	if got := tr.chatPaths(); len(got) != 0 {
		t.Errorf("**绝不能发对话请求**（上游会静默回退到免费通道并返回 200），"+
			"实际发出 %v", got)
	}
}

// TestChatStreamPassesThroughWhenListFetchFails 清单拉取失败 ⇒ 照常发请求。
//
// 这是"不能拦"的第一条例外：拿不到权威信息时不许拦，
// 否则一次拉取失败会让**所有**模型不可用。
func TestChatStreamPassesThroughWhenListFetchFails(t *testing.T) {
	tr := &guardTransport{listStatus: http.StatusInternalServerError}
	d := guardDispatch(tr)

	a := &auth.Auth{
		UID: "guard-uid", Product: auth.ProductQoder,
		AccessToken: "dt-guard", RefreshToken: "drt-guard",
		ExpiresAt: time.Now().Add(24 * time.Hour).Unix(),
		Domain:    "qoder.com.cn",
	}
	body := []byte(`{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"hi"}]}`)

	// 清单拉取失败 ⇒ 原样透传 ⇒ 会真的去发对话请求。
	// 假 transport 对对话请求回 500 —— 这正是我们要的"确实发出去了"的证据。
	_, status, _, err := d.ChatStream(context.Background(), a, body)
	if err != nil {
		var mnf *ModelNotFoundError
		if errors.As(err, &mnf) {
			t.Fatal("清单拉取失败时**绝不能**判成「模型不存在」—— " +
				"拿不到权威信息时不许拦（否则一次网络抖动 = 所有模型不可用）")
		}
	}
	if status != http.StatusInternalServerError {
		t.Errorf("应把对话请求真的发出去并原样转达上游结果，实际 status=%d err=%v", status, err)
	}
	if got := tr.chatPaths(); len(got) == 0 {
		t.Error("清单拉取失败时应原样透传（真的发请求），实际一发都没发")
	}
}

// TestModelNotFoundErrorMessage 文案含区域与模型名（便于用户定位）。
func TestModelNotFoundErrorMessage(t *testing.T) {
	e := &ModelNotFoundError{Model: "deepseek-v4.1-flash", Region: RegionCN, Available: []string{"dfmodel"}}
	msg := e.Error()
	if !strings.Contains(msg, "deepseek-v4.1-flash") {
		t.Errorf("文案应含模型名，实际 %q", msg)
	}
	if !strings.Contains(msg, "国服") {
		t.Errorf("文案应含区域名（两区清单不同），实际 %q", msg)
	}
	// nil 接收者不该 panic（错误值可能被零值传递）
	var nilErr *ModelNotFoundError
	if nilErr.Error() == "" {
		t.Error("nil 接收者的 Error() 不该返回空串")
	}
}

// TestAvailableKeysDedupAndSort 可用清单去重 + 排序（文案顺序稳定）。
func TestAvailableKeysDedupAndSort(t *testing.T) {
	got := availableKeys([]DynamicModel{
		{Key: "zzz"}, {Key: "aaa"}, {Key: "zzz"}, {Key: ""}, {Key: "mmm"},
	})
	want := []string{"aaa", "mmm", "zzz"}
	if len(got) != len(want) {
		t.Fatalf("应去重并跳过空 key，期望 %v 实际 %v", want, got)
	}
	for i := range want {
		if got[i] != want[i] {
			t.Fatalf("应去重并排序，期望 %v 实际 %v", want, got)
		}
	}
}

// sortedStrings 报告 s 是否已按字典序排列。
func sortedStrings(s []string) bool {
	for i := 1; i < len(s); i++ {
		if s[i-1] > s[i] {
			return false
		}
	}
	return true
}
