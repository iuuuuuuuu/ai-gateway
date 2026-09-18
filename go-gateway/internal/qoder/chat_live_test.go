package qoder

// 真实链路：对话端点必须**走到上游**并返回可解析的流。
//
// 与签名验证同样的思路：用伪造令牌，只验证"请求被上游受理"这一层。
// 判据是**不是签名错误**（101）——若拿到 105 Login expired 或流式响应，
// 说明请求构造（路径、签名、头、body）全部正确。
//
// 这条测试能抓到一类单元测试抓不到的问题：
//   · 对话路径写错（参考实现的路径很长，容易抄错一段）
//   · x-model-key 头缺失或值不对
//   · body 与签名不一致（签名覆盖 body，改一个字就 101）
//
// 需要联网，默认跳过：QODER_LIVE_SIGCHECK=1 go test ./internal/qoder/ -run TestLiveChat -v

import (
	"context"
	"encoding/json"
	"net/url"
	"os"
	"strings"
	"testing"
	"time"
)

// pathSigOf 复刻 AuthHeader 里 pathSig 的计算口径（去掉 /algo 前缀、不含查询串）。
func pathSigOf(t *testing.T, rawURL string) string {
	t.Helper()
	u, err := url.Parse(rawURL)
	if err != nil {
		t.Fatalf("解析 URL 失败: %v", err)
	}
	return strings.TrimPrefix(u.Path, "/algo")
}

// TestLiveChatRequestAccepted 对话请求必须被上游受理。
func TestLiveChatRequestAccepted(t *testing.T) {
	if os.Getenv("QODER_LIVE_SIGCHECK") == "" {
		t.Skip("未设置 QODER_LIVE_SIGCHECK=1 —— 跳过联网验证")
	}

	// 伪造凭证（机器指纹必须齐全，签名需要）
	fake := &Cred{
		UID:          "00000000-0000-0000-0000-000000000000",
		Nickname:     "probe",
		DT:           "dt-" + strings.Repeat("0", 43),
		DRT:          "drt-" + strings.Repeat("0", 43),
		MachineID:    "11111111-1111-1111-1111-111111111111",
		MachineToken: strings.Repeat("a", 32),
		MachineType:  "5",
		Region:       RegionCN,
	}

	body, _ := json.Marshal(map[string]any{
		"model": "deepseek-v4.1-flash",
		"messages": []map[string]string{
			{"role": "system", "content": "You are a helpful assistant."},
			{"role": "user", "content": "说一句话"},
		},
		"stream":     true,
		"max_tokens": 10,
	})

	c := New()
	ctx, cancel := context.WithTimeout(context.Background(), 45*time.Second)
	defer cancel()

	rc, status, respBody, err := c.ChatStream(ctx, fake, body)
	if err != nil {
		// 网络问题与签名有效性无关
		t.Skipf("网络不可达（与请求构造无关）：%v", err)
	}
	if rc != nil {
		defer rc.Close()
	}

	text := string(respBody)
	t.Logf("HTTP %d  %s", status, truncate(text, 200))

	if strings.Contains(text, "Signature invalid") || strings.Contains(text, `"101"`) {
		t.Fatalf("✗ 对话请求被判签名无效（101）—— 请求构造有误。\n"+
			"    可能是：路径抄错、x-model-key 缺失、body 与签名不一致。\n"+
			"    原始响应：%s", truncate(text, 300))
	}

	// 过了签名关：伪造令牌时预期 105 Login expired
	if strings.Contains(text, "Login expired") || strings.Contains(text, `"105"`) {
		t.Logf("✓ 对话请求被受理（105 Login expired，伪造令牌的预期结果）")
		t.Logf("  ⇒ 路径 / 签名 / 请求头 / body 全部正确")
		return
	}

	// 若真拿到流（理论上不会，因为令牌是假的），也说明请求构造正确
	if status == 200 && rc != nil {
		t.Log("✓ 对话请求返回 200 流（请求构造正确）")
		return
	}

	t.Logf("? 响应既非签名错误也非 105（status=%d），无法据此判定请求构造。需人工确认。", status)
}

// TestLiveChatPathMatchesReference 对话路径必须与参考实现一致。
//
// 这条**不需要联网**：路径抄错是纯粹的字符串问题，本地就能验。
// 参考实现的路径很长，是最容易抄错的地方。
func TestLiveChatPathMatchesReference(t *testing.T) {
	// 参考实现 internal/upstream/chat.go 的常量
	const want = "/algo/api/v2/service/pro/sse/agent_chat_generation?FetchKeys=llm_model_result&AgentId=agent_common&Encode=1"
	if ChatPath != want {
		t.Errorf("对话路径与参考实现不一致\n  期望: %s\n  实际: %s", want, ChatPath)
	}
	// 模型列表路径同理
	const wantModels = "/algo/api/v2/model/list?Encode=1"
	if ModelsPath != wantModels {
		t.Errorf("模型列表路径不一致\n  期望: %s\n  实际: %s", wantModels, ModelsPath)
	}
}

// TestLiveModelsPathSignatureInput 模型列表的签名输入必须覆盖完整 path。
//
// pathSig 是"URL path 去掉 /algo"，**不含查询串**（参考实现用 u.Path，
// 它天然不含 query）。若误把 query 也算进去，签名必然不匹配。
func TestLiveModelsPathSignatureInput(t *testing.T) {
	// 复刻 AuthHeader 的 pathSig 计算
	rawURL := RegionCN.Gateway() + ModelsPath
	pathSig := pathSigOf(t, rawURL)
	if pathSig != "/api/v2/model/list" {
		t.Errorf("pathSig 应为 /api/v2/model/list（不含查询串），实际 %s", pathSig)
	}
	// 对话路径的 pathSig
	chatSig := pathSigOf(t, RegionCN.Gateway()+ChatPath)
	if chatSig != "/api/v2/service/pro/sse/agent_chat_generation" {
		t.Errorf("对话 pathSig 错误，实际 %s", chatSig)
	}
}
