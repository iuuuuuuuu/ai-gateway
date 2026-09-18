package qoder

// 真实上游的签名有效性验证 —— **不需要任何真实凭证**。
//
// 方法（与 uitest/probe-qoder-signature-control.cjs 的对照实验同源）：
//
//	发一个「签名正确 + 令牌伪造」的请求，看上游回什么：
//	  101 Signature invalid → 签名**被拒**（算法或公钥已失效）
//	  105 Login expired     → 签名**被接受**，只是令牌假（这正是我们要的）
//
// 为什么这个方法有价值：签名算法是逆向来的，随时可能被上游改掉。
// 传统做法要拿一个真实账号去试，而真实凭证可能过期/被限流/根本不在本机 ——
// 用伪造令牌可以把「签名是否有效」这个变量**单独隔离出来**测量。
//
// 需要联网，故默认跳过；用 QODER_LIVE_SIGCHECK=1 显式开启：
//
//	QODER_LIVE_SIGCHECK=1 go test ./internal/qoder/ -run TestLiveSignatureValidity -v

import (
	"context"
	"io"
	"net/http"
	"os"
	"strings"
	"testing"
	"time"
)

// probeOnce 对指定区域发一次「签名正确 + 令牌伪造」的请求，返回响应体。
func probeOnce(t *testing.T, region Region) (int, string) {
	t.Helper()

	// 伪造凭证：令牌与 uid 都是假的，但**机器指纹齐全**（签名需要它）。
	fake := &Cred{
		UID:          "00000000-0000-0000-0000-000000000000",
		DT:           "dt-" + strings.Repeat("0", 43),
		DRT:          "drt-" + strings.Repeat("0", 43),
		MachineID:    "11111111-1111-1111-1111-111111111111",
		MachineToken: strings.Repeat("a", 32),
		MachineType:  "5",
		Region:       region,
	}

	rawURL := region.Gateway() + ModelsPath
	sess, err := NewCosySession(fake, fake.DT, fake.DRT)
	if err != nil {
		t.Fatalf("构造签名会话失败: %v", err)
	}

	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, rawURL, nil)
	if err != nil {
		t.Fatalf("构造请求失败: %v", err)
	}
	// GET 的 body 用空串签名；uid 用于 cosy-user 头
	if err := sess.ApplyHeaders(req, "", rawURL, fake.UID, false, ""); err != nil {
		t.Fatalf("设置请求头失败: %v", err)
	}

	resp, err := (&http.Client{Timeout: 30 * time.Second}).Do(req)
	if err != nil {
		t.Skipf("%s 网络不可达（与签名有效性无关）：%v", region.Label(), err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	return resp.StatusCode, string(body)
}

// TestLiveSignatureValidity 两区的签名都必须被上游接受。
//
// 判据刻意**不要求 200**：我们用的是伪造令牌，200 反而说明哪里不对。
// 只要求「不是签名错误」——那才是本次要验证的东西。
func TestLiveSignatureValidity(t *testing.T) {
	if os.Getenv("QODER_LIVE_SIGCHECK") == "" {
		t.Skip("未设置 QODER_LIVE_SIGCHECK=1 —— 跳过联网验证（避免 CI 依赖网络）")
	}

	for _, region := range []Region{RegionCN, RegionIntl} {
		t.Run(region.Label(), func(t *testing.T) {
			status, body := probeOnce(t, region)
			t.Logf("HTTP %d  %s", status, truncate(body, 200))

			// 签名被拒的判据（两种写法都覆盖：业务码 101 与文案）
			if strings.Contains(body, "Signature invalid") || strings.Contains(body, `"101"`) {
				t.Fatalf("✗ %s 上游拒绝签名（101 Signature invalid）——\n"+
					"    签名算法或内置 RSA 公钥已失效，需要重新逆向客户端。\n"+
					"    这会导致**所有** Qoder 账号无法使用。原始响应：%s",
					region.Label(), truncate(body, 300))
			}

			// 过了签名关的判据：105 Login expired（令牌假，符合预期）
			if strings.Contains(body, "Login expired") || strings.Contains(body, `"105"`) {
				t.Logf("✓ %s 签名被接受（上游回 105 Login expired，正是伪造令牌的预期结果）", region.Label())
				return
			}

			// 其它响应：可能是限流、维护等。不判定失败，但要如实记录 ——
			// 不能因为"没看到 101"就宣称签名有效（那是不充分的判据）。
			t.Logf("? %s 响应既非签名错误也非 105，无法据此判定签名有效性。"+
				"需人工确认。", region.Label())
		})
	}
}

// TestLivePublicKeyStillPaired 内置公钥的位数与格式自检。
//
// 这条**不需要联网**：它只证明我们的公钥没被写坏。
// 是否与上游当前私钥配对，只能由 TestLiveSignatureValidity 回答。
func TestLivePublicKeyStillPaired(t *testing.T) {
	if bits := ServerPubKeyBits(); bits != 1024 {
		t.Errorf("内置公钥位数异常：%d（应为 1024，与客户端一致）", bits)
	}
}
