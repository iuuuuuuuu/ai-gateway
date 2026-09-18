package zcode

// Z2 真实链路验证：用**伪造凭证**打上游，确认请求构造正确、错误分类准确。
//
// ## 为什么用伪造凭证
//
// 它把"请求构造是否正确"这个变量**单独隔离出来**测量：
//
//	回 1001（认证参数未收到）→ 我们没发 Authorization（**我们的 bug**）
//	回 1000（认证失败）      → 请求构造正确，只是凭证假（**这正是我们要的**）
//
// 不需要真实账号、不消耗任何额度。
//
// 需要联网，故默认跳过：ZCODE_LIVE_PROBE=1 go test ./internal/zcode/ -run TestLive -v

import (
	"context"
	"os"
	"strings"
	"testing"
	"time"
)

// liveCred 伪造凭证（两段式，与真实形态一致）。
func liveCred(p Provider) *Cred {
	return &Cred{
		UID:        "zcode-live-probe",
		Credential: "fake-api-key-id-0001.fake-api-key-secret-0002",
		Provider:   p,
	}
}

func requireLive(t *testing.T) {
	t.Helper()
	if os.Getenv("ZCODE_LIVE_PROBE") == "" {
		t.Skip("未设置 ZCODE_LIVE_PROBE=1 —— 跳过联网验证（避免 CI 依赖网络）")
	}
}

// TestLiveChatRequestReachesBusinessLayer 对话请求必须到达业务层。
//
// 判据：**不是** 1001（认证参数未收到）。
// 得到 1000（认证失败）就说明我们发了 Authorization、请求构造正确。
func TestLiveChatRequestReachesBusinessLayer(t *testing.T) {
	requireLive(t)

	for _, p := range AllProviders() {
		t.Run(p.Label(), func(t *testing.T) {
			c := New()
			ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
			defer cancel()

			body := []byte(`{"model":"glm-4.6","messages":[{"role":"user","content":"hi"}],"max_tokens":5}`)
			rc, status, respBody, err := c.StreamChat(ctx, liveCred(p), body)
			if rc != nil {
				rc.Close()
			}
			if err != nil {
				t.Skipf("网络不可达（与请求构造无关）：%v", err)
			}

			text := string(respBody)
			t.Logf("HTTP %d  %s", status, truncate(text, 200))

			kind := Classify(status, text)
			if kind == ErrAuthMissing {
				t.Fatalf("✗ 上游回「认证参数未收到」（1001）—— 说明我们**没发 Authorization**。"+
					"这是请求构造的 bug。原始响应：%s", truncate(text, 300))
			}
			if kind == ErrSigningRequired {
				t.Fatalf("⚠ 上游开始要求客户端签名（VERIFY_*）——\n"+
					"    我们刻意没实现签名（见 signing.go），现在需要补上。\n"+
					"    原始响应：%s", truncate(text, 300))
			}
			if kind == ErrAuthFailed {
				t.Logf("✓ 请求到达业务层（上游回「认证失败」= 凭证假，正是预期）")
				return
			}
			t.Logf("? 分类为 %q，需人工确认", kind)
		})
	}
}

// TestLiveIdentityHeadersAccepted 带身份头与不带身份头的结果应**相同**。
//
// 这条验证 identity.go 的核心假设：身份头是**可选**的（不影响认证）。
// 若两者结果不同，说明身份头其实是必需的 —— 那我们的"取不到就省略"
// 策略就是错的。
func TestLiveIdentityHeadersAccepted(t *testing.T) {
	requireLive(t)

	c := New()
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	body := []byte(`{"model":"glm-4.6","messages":[{"role":"user","content":"hi"}],"max_tokens":5}`)

	// 带身份头
	withID := New()
	_, statusA, bodyA, err := withID.StreamChat(ctx, liveCred(ProviderZAI), body)
	if err != nil {
		t.Skipf("网络不可达：%v", err)
	}

	// 不带身份头（清空）
	noID := New()
	noID.Identity = Identity{} // 全空 → Headers() 只剩固定的几个
	_, statusB, bodyB, err := noID.StreamChat(ctx, liveCred(ProviderZAI), body)
	if err != nil {
		t.Skipf("网络不可达：%v", err)
	}

	kindA := Classify(statusA, string(bodyA))
	kindB := Classify(statusB, string(bodyB))
	t.Logf("带身份头  : HTTP %d  %s", statusA, truncate(string(bodyA), 120))
	t.Logf("无身份头  : HTTP %d  %s", statusB, truncate(string(bodyB), 120))

	if kindA != kindB {
		t.Errorf("带/不带身份头的分类不同（%q vs %q）—— 说明身份头**影响认证**，"+
			"那么 identity.go 的「取不到就省略」策略是错的", kindA, kindB)
	} else {
		t.Logf("✓ 两者分类相同（%q）⇒ 身份头不影响认证，与实测预期一致", kindA)
	}
	_ = c
}

// TestLiveErrorClassificationMatchesRealResponses 真实响应必须被正确分类。
//
// 这是 errors.go 的核心验证：分类错了会把用户引向错误的排查方向。
func TestLiveErrorClassificationMatchesRealResponses(t *testing.T) {
	requireLive(t)

	c := New()
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	body := []byte(`{"model":"glm-4.6","messages":[{"role":"user","content":"hi"}],"max_tokens":5}`)

	_, status, respBody, err := c.StreamChat(ctx, liveCred(ProviderZAI), body)
	if err != nil {
		t.Skipf("网络不可达：%v", err)
	}
	text := string(respBody)
	kind := Classify(status, text)

	// 伪造凭证下预期是 ErrAuthFailed。其它结果都要解释清楚。
	switch kind {
	case ErrAuthFailed:
		t.Logf("✓ 分类为 auth_failed（伪造凭证的正确分类）")
	case ErrAuthMissing:
		t.Fatalf("✗ 分类为 auth_missing —— 但我们**发了** Authorization，说明分类逻辑有误")
	case ErrSigningRequired:
		t.Fatalf("⚠ 分类为 signing_required —— 上游开始要求签名了，需要补实现")
	case ErrNone:
		t.Logf("? 未能分类（原始响应：%s）—— 可能是上游改了错误格式", truncate(text, 200))
	default:
		t.Logf("? 分类为 %q（原始响应：%s）", kind, truncate(text, 200))
	}
}

// TestLiveModelsFromConfig 模型清单必须能从**免认证**的 config 端点拉到。
//
// 这是 design.md D6 的验证：模型清单从上游拉、不硬编码。
//
// 实测（2026-09-19）该接口返回 **2 个模型**（GLM-5.3 / GLM-5.3-Flash），
// 而参考实现硬编码了 11 个 —— 那 9 个已过时。硬编码会让用户看到用不了的模型。
func TestLiveModelsFromConfig(t *testing.T) {
	requireLive(t)

	c := New()
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	// 传 nil 凭证：这个端点**不需要认证**，正好验证这一点
	models, err := c.FetchModels(ctx, nil)
	if err != nil {
		t.Skipf("拉取模型清单失败（网络问题）：%v", err)
	}
	if len(models) == 0 {
		t.Fatal("模型清单不应为空")
	}

	t.Logf("✓ 拉到 %d 个模型（来源 %s）", len(models), models[0].Source)
	for _, m := range models {
		t.Logf("    %-18s 上下文 %-9d 输出 %-8d 视觉 %-5v 推理 %v",
			m.ID, m.ContextWindow, m.MaxOutput, m.Vision, m.Reasoning)
	}

	// 每个模型都必须有来源标记（界面据此区分真值与兜底）
	for _, m := range models {
		if m.Source == "" {
			t.Errorf("模型 %s 缺少 Source 标记", m.ID)
		}
		if m.Source == "builtin" {
			t.Errorf("模型 %s 标为 builtin，但这次是从上游拉的 —— 来源标记有误", m.ID)
		}
	}

	// 抽查：上游当前返回的模型应有上下文窗口（不是零值）
	hasCtx := false
	for _, m := range models {
		if m.ContextWindow > 0 {
			hasCtx = true
			break
		}
	}
	if !hasCtx {
		t.Error("所有模型的上下文窗口都是 0 —— 字段名可能变了（形状守卫）")
	}
}

// TestLiveVisionComesFromCapabilities 视觉标记必须来自上游 capabilities，
// **不能**用"id 里含 v"的启发式。
//
// 实测：上游的 `GLM-5.3-Flash` 带 `capabilities.vision=true`，而它的 id 里
// **没有 v** —— 启发式会漏判，用户会以为模型不支持图片。
//
// 参考实现用的就是启发式（`routes-openai.ts` 注释自认是 heuristic）。
func TestLiveVisionComesFromCapabilities(t *testing.T) {
	requireLive(t)

	c := New()
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	models, err := c.FetchModels(ctx, nil)
	if err != nil {
		t.Skipf("拉取模型清单失败：%v", err)
	}

	var visionModels []string
	for _, m := range models {
		if m.Vision {
			visionModels = append(visionModels, m.ID)
		}
	}
	t.Logf("上游标为视觉的模型（%d 个）：%v", len(visionModels), visionModels)

	// 启发式会判成什么
	var byHeuristic []string
	for _, m := range models {
		if strings.ContainsAny(strings.ToLower(m.ID), "v") {
			byHeuristic = append(byHeuristic, m.ID)
		}
	}
	t.Logf("启发式（id 含 v）会判为视觉的：%v", byHeuristic)

	// 找出启发式会漏判的
	var missed []string
	for _, id := range visionModels {
		found := false
		for _, h := range byHeuristic {
			if h == id {
				found = true
				break
			}
		}
		if !found {
			missed = append(missed, id)
		}
	}
	if len(missed) > 0 {
		t.Logf("⚠ 启发式会漏判 %d 个：%v —— 这就是必须用 capabilities.vision 的原因",
			len(missed), missed)
	} else {
		t.Logf("（本轮启发式恰好没漏判，但不能因此改用启发式 —— 上游模型会变）")
	}
}

// TestLiveQuotaRequiresJWT 额度查询必须用 JWT（没有则明确报错）。
//
// 验证 ErrNoJWT 的语义：只导入了凭证的账号查不到额度，
// 界面据此显示"未知"而不是 0（0 会被误读成"额度耗尽"）。
func TestLiveQuotaRequiresJWT(t *testing.T) {
	requireLive(t)

	c := New()
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()

	// 有凭证但无 JWT（用户粘贴导入的典型形态）
	cr := &Cred{Credential: "fake.fake", Provider: ProviderZAI}
	_, err := c.FetchQuota(ctx, cr)
	if err != ErrNoJWT {
		t.Errorf("无 JWT 时应返回 ErrNoJWT（供界面区分），实际 %v", err)
	} else {
		t.Log("✓ 无 JWT 时返回 ErrNoJWT（界面会显示「额度未知」而不是 0）")
	}
}
//
// 若某个端点 DNS 都不解析，该服务商的账号全部不可用，
// 而错误会显示成"网络问题"。
// TestLiveProviderEndpointsReachable 两个服务商的端点都必须可达。
//
// 若某个端点 DNS 都不解析，该服务商的账号全部不可用，
// 而错误会显示成"网络问题"。
func TestLiveProviderEndpointsReachable(t *testing.T) {
	requireLive(t)

	c := New()
	ctx, cancel := context.WithTimeout(context.Background(), 40*time.Second)
	defer cancel()
	body := []byte(`{"model":"glm-4.6","messages":[{"role":"user","content":"hi"}],"max_tokens":5}`)

	for _, p := range AllProviders() {
		t.Run(p.Label(), func(t *testing.T) {
			_, status, respBody, err := c.StreamChat(ctx, liveCred(p), body)
			if err != nil {
				if strings.Contains(err.Error(), "no such host") ||
					strings.Contains(err.Error(), "connection refused") {
					t.Errorf("%s 端点不可达: %v", p.Label(), err)
					return
				}
				t.Skipf("%s 网络问题（非端点不可达）：%v", p.Label(), err)
			}
			t.Logf("✓ %s 端点可达（%s）HTTP %d", p.Label(), p.OpenAIBase(), status)
			_ = respBody
		})
	}
}
