package auth

import "testing"

// 端到端确认：**凭证里的真实域名**会被判成正确的区域。
//
// 这里刻意写死真实凭证里出现过的域名值（不是构造的），
// 因为本次缺陷的教训正是「测试用的是自己编的域名，所以没发现 qoder.sh 漏了」。
//
// ⚠ 域名取值来源（2026-09-22 读本机 21 个账号文件得到）：
//
//	gateway: www.workbuddy.ai ×5 / copilot.tencent.com ×11 / www.codebuddy.cn ×3
//	qoder:   qoder.sh ×1（国际版） / qoder.com.cn ×1（国服）
//	zcode:   （文件里无 domain，由 DomainOfProvider 写入 api.z.ai / open.bigmodel.cn）
func TestRealCredentialDomainsMapToExpectedRegion(t *testing.T) {
	// 这张表就是"线上真实分布"，改动它前先去读一遍真实凭证。
	realWorld := []struct {
		product string
		domain  string
		region  Region
	}{
		{"workbuddy", "www.workbuddy.ai", RegionIntl},
		{"workbuddy", "copilot.tencent.com", RegionCN},
		{"workbuddy", "www.codebuddy.cn", RegionCN},
		{"qoder", "qoder.sh", RegionIntl},      // ← 本次修的就是它
		{"qoder", "qoder.com.cn", RegionCN},
		{"zcode", "api.z.ai", RegionIntl},
		{"zcode", "open.bigmodel.cn", RegionCN},
	}

	for _, r := range realWorld {
		got := (&Auth{Domain: r.domain, Product: r.product}).Region()
		if got != r.region {
			t.Errorf("%s 账号 domain=%q：Region()=%v，应为 %v",
				r.product, r.domain, got, r.region)
		}
		// 代理分流用的就是 IsIntl，两者必须一致 —— 否则"被判成国际版"
		// 与"走代理"会脱节，正是本次缺陷的形态。
		wantIntl := r.region == RegionIntl
		if (&Auth{Domain: r.domain}).IsIntl() != wantIntl {
			t.Errorf("%s domain=%q：IsIntl()=%v，应为 %v（代理分流依赖它）",
				r.product, r.domain, !wantIntl, wantIntl)
		}
	}
}
