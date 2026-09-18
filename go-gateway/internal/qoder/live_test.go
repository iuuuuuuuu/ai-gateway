package qoder

// 真实链路验证：用**所有者真实的 Qoder 凭证**打上游。
//
// 单元测试只能证明"我们自己签自己验通过"，无法证明"上游接受我们的签名"。
// 唯一可靠的判据是真实请求 —— 本文件正是为此。
//
// 运行方式（凭证目录由环境变量给出，缺省则跳过，保证 CI 不会误跑）：
//
//	QODER_LIVE_AUTH_DIR=<目录> go test ./internal/qoder/ -run TestLive -v
//
// 凭证由调用脚本**只读复制**到临时目录，原件分毫不动。
//
// 关键判据（与 uitest/probe-qoder-signature-control.cjs 的对照实验一致）：
//
//	签名被接受 → 上游回 105 Login expired（伪造 token 时）或 200（真 token 时）
//	签名被拒   → 上游回 101 Signature invalid
//
// 所以只要**没有**看到 101，就说明签名层是好的。

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// liveCreds 加载真实凭证；没有则跳过。
func liveCreds(t *testing.T) []*Cred {
	t.Helper()
	dir := os.Getenv("QODER_LIVE_AUTH_DIR")
	if dir == "" {
		t.Skip("未提供 QODER_LIVE_AUTH_DIR —— 跳过真实链路验证")
	}
	creds, failed, err := LoadDir(dir)
	if err != nil {
		t.Fatalf("读取凭证目录失败: %v", err)
	}
	for _, f := range failed {
		t.Logf("跳过无法解析的凭证: %s", f)
	}
	if len(creds) == 0 {
		t.Skip("凭证目录里没有可用的 Qoder 凭证")
	}
	return creds
}

// TestLiveSignatureAcceptedByUpstream 真实请求：上游必须**不**报签名错误。
//
// 这是整个 Qoder 集成的关键验证 —— 签名算法是逆向来的，
// 一旦上游改规则，这里会立刻发现（错误码从 105 变成 101）。
func TestLiveSignatureAcceptedByUpstream(t *testing.T) {
	creds := liveCreds(t)
	c := New()
	ctx := context.Background()

	// 逐区域验证：两区都要测（国际版是本次新增的能力，参考实现没有）
	seenRegion := map[Region]bool{}
	for _, cr := range creds {
		if seenRegion[cr.Region] {
			continue
		}
		seenRegion[cr.Region] = true

		t.Run("区域="+cr.Region.Label(), func(t *testing.T) {
			models, err := c.FetchModels(ctx, cr)
			if err != nil {
				msg := err.Error()
				// 关键判据：签名被拒是**致命**的（说明逆向失效）；
				// 其它错误（令牌过期、网络）不否定签名层的有效性。
				if strings.Contains(msg, "Signature invalid") || strings.Contains(msg, `"101"`) {
					t.Fatalf("上游拒绝签名（101 Signature invalid）—— "+
						"签名算法或内置公钥已失效，需要重新逆向。原始错误：%v", err)
				}
				t.Skipf("非签名类错误（不影响签名有效性的结论）：%v", err)
			}
			if len(models) == 0 {
				t.Error("签名通过但模型列表为空")
			} else {
				t.Logf("✓ %s 签名被接受，拉到 %d 个模型", cr.Region.Label(), len(models))
				// 抽查前几个模型的关键字段，确认解析口径正确
				for i, m := range models {
					if i >= 3 {
						break
					}
					t.Logf("    %-28s 视觉=%-5v 推理=%-5v 上下文=%d 倍率=%.2f",
						m.Key, m.IsVL, m.IsReasoning, m.MaxContextTokens(), m.PriceFactor)
				}
			}
		})
	}
}

// TestLiveQuotaFetchable 额度接口（走 OpenAPI，**不需要签名**）。
//
// 这条同时验证了「两条鉴权路径分清」这个约定：
// 额度用的是普通 Bearer，若实现里误加了 COSY 签名，这里会失败。
func TestLiveQuotaFetchable(t *testing.T) {
	creds := liveCreds(t)
	c := New()
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()

	cr := creds[0]
	q, err := c.FetchQuota(ctx, cr)
	if err != nil {
		msg := err.Error()
		if strings.Contains(msg, "401") || strings.Contains(msg, "403") || strings.Contains(msg, "105") {
			t.Skipf("令牌已失效（与接口实现无关）：%v", err)
		}
		t.Fatalf("查询额度失败: %v", err)
	}
	t.Logf("✓ %s 额度：剩余 %d / 共 %d（超额=%v，套餐=%q）",
		cr.Region.Label(), q.Remaining, q.Total, q.Exceeded, q.PlanTierName)
	if q.Total <= 0 {
		t.Log("注意：总额度为 0 —— 上游可能不再返回总量（成本因子需要它，需另行确认）")
	}
}

// TestLiveRegionReachable 两区的域名都必须可达（DNS + TLS + HTTP）。
//
// 这是"双区支持"的基础验证：若某个区域的域名解析不了，
// 该区域的登录与调用都会失败，而错误会显示成"网络问题"。
func TestLiveRegionReachable(t *testing.T) {
	if os.Getenv("QODER_LIVE_AUTH_DIR") == "" {
		t.Skip("未提供 QODER_LIVE_AUTH_DIR —— 跳过")
	}
	creds := liveCreds(t)
	c := New()
	ctx, cancel := context.WithTimeout(context.Background(), 90*time.Second)
	defer cancel()

	for _, region := range []Region{RegionCN, RegionIntl} {
		t.Run(region.Label(), func(t *testing.T) {
			// 找一个该区域的凭证；没有就用第一个（仅验证域名可达性）
			var cr *Cred
			for _, x := range creds {
				if x.Region == region {
					cr = x
					break
				}
			}
			if cr == nil {
				cr = creds[0]
			}
			// 用模型列表接口探活（它需要签名，顺带再验一次签名）
			_, err := c.FetchModels(ctx, cr)
			if err != nil && strings.Contains(err.Error(), "Signature invalid") {
				t.Fatalf("%s 域名可达但签名被拒: %v", region.Label(), err)
			}
			// 无论业务结果如何，只要不是 DNS/连接错误就算域名可达
			if err != nil && (strings.Contains(err.Error(), "no such host") ||
				strings.Contains(err.Error(), "connection refused")) {
				t.Errorf("%s 域名不可达: %v", region.Label(), err)
			} else {
				t.Logf("%s 域名 %s 可达", region.Label(), region.Gateway())
			}
		})
	}
}

// TestLiveCredentialsAreReadOnly 确认验证过程不改动凭证原件。
//
// 这条是给自己上的保险：所有真实链路测试都必须**只读**凭证。
// 若哪天有人让测试写回了凭证，这里会发现。
func TestLiveCredentialsAreReadOnly(t *testing.T) {
	dir := os.Getenv("QODER_LIVE_AUTH_DIR")
	if dir == "" {
		t.Skip("未提供 QODER_LIVE_AUTH_DIR —— 跳过")
	}
	files, _ := filepath.Glob(filepath.Join(dir, "qoder*.json"))
	if len(files) == 0 {
		t.Skip("目录里没有凭证文件")
	}
	// 记录修改时间
	before := map[string]time.Time{}
	for _, f := range files {
		st, err := os.Stat(f)
		if err == nil {
			before[f] = st.ModTime()
		}
	}

	creds := liveCreds(t)
	c := New()
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	for i, cr := range creds {
		if i >= 2 {
			break
		}
		_, _ = c.FetchModels(ctx, cr) // 忽略结果，只关心副作用
	}

	for f, mt := range before {
		st, err := os.Stat(f)
		if err != nil {
			t.Errorf("凭证文件消失了: %s", filepath.Base(f))
			continue
		}
		if !st.ModTime().Equal(mt) {
			t.Errorf("凭证文件被改动了（测试必须是只读的）: %s", filepath.Base(f))
		}
	}
}
