package auth

// 区域判定必须按**完整域名**匹配，不能用 `HasSuffix(".ai")`。
//
// # 这组用例钉的是一个真实缺陷（2026-09-22 所有者现场）
//
// 所有者问「qoder 也有国外的那个,他应该也要代理吧?」—— 对，而它**没走代理**。
//
// 根因：判据是 `HasSuffix(domain, ".ai")`，而 Qoder 国际版的域名是
// `qoder.sh`（`.sh` 不是 `.ai`）⇒ 被判成**国服** ⇒ `proxy_scope.intl`
// 对它不生效 ⇒ 永远直连。实测 `openapi.qoder.sh`：
//
//	直连        → 10.3s
//	走 7890 代理 →  1.2s   （慢 8.7 倍）
//
// 而 `cmd/server/main.go` 的 `qoderAuthOf` 注释写着
// 「域名用于区域判定（qoder.sh = 国际版）」—— 写那段时以为判据认得出它。
// **两处口径分叉**，故这里把域名清单钉死。
//
// ⚠ 新增产品/域名时必须同步 auth.go 的 intlDomains 与这里的用例 ——
// 漏掉的表现是「某个产品的国际版特别慢」，而其它产品都正常。

import "testing"

// TestIsIntlMatchesEveryRealDomain 每个产品**真实的**域名都要判对。
//
// 域名取值来自代码里的字面量（不是猜的）：
//   - WorkBuddy: smslogin.go 的 www.workbuddy.ai / www.workbuddy.cn，
//     以及凭证里的 copilot.tencent.com / www.codebuddy.cn
//   - Qoder: region.go 的 Region.Domain() → qoder.sh / qoder.com.cn
//   - ZCode: dispatch.go 的 DomainOfProvider → api.z.ai / open.bigmodel.cn
func TestIsIntlMatchesEveryRealDomain(t *testing.T) {
	cases := []struct {
		domain string
		want   bool
		why    string
	}{
		// ---- 国际版：必须判 true，否则拿不到代理 ----
		{"www.workbuddy.ai", true, "WorkBuddy 国际版（短信登录域）"},
		{"workbuddy.ai", true, "裸根域"},
		// ⚠ codebuddy.ai 在任何**真实凭证**里都没观察到（我查过本机 21 个账号
		// 文件），但本仓库多处注释把它与 workbuddy.ai 并列为国际版，
		// 且 upstream 有测试依赖它 ⇒ 保留在清单里。
		{"www.codebuddy.ai", true, "注释里并列为国际版（未在真实凭证中出现）"},
		{"qoder.sh", true, "Qoder 国际版（凭证里写的就是它）"},
		{"api3.qoder.sh", true, "Qoder 国际版网关域"},
		{"openapi.qoder.sh", true, "Qoder 国际版 OpenAPI 域"},
		{"qoder.com", true, "Qoder 国际版授权页"},
		{"api.z.ai", true, "ZCode（Z.AI）API 域"},
		{"z.ai", true, "Z.AI 裸根域"},

		// ---- 国服：必须判 false，否则会被无谓地绕进代理 ----
		{"copilot.tencent.com", false, "WorkBuddy 国服"},
		{"www.codebuddy.cn", false, "CodeBuddy 国服"},
		{"www.workbuddy.cn", false, "WorkBuddy 国服（短信登录用）"},
		{"qoder.com.cn", false, "Qoder 国服"},
		{"openapi.qoder.com.cn", false, "Qoder 国服 OpenAPI 域"},
		{"gateway.qoder.com.cn", false, "Qoder 国服网关域"},
		{"open.bigmodel.cn", false, "ZCode 国服（智谱）"},
		{"", false, "空域名按国服（历史行为）"},
	}

	for _, c := range cases {
		a := &Auth{Domain: c.domain}
		if got := a.IsIntl(); got != c.want {
			t.Errorf("IsIntl(%q) = %v，应为 %v（%s）",
				c.domain, got, c.want, c.why)
		}
		// Region() 必须与 IsIntl 一致（两者同源，别再分叉）。
		wantRegion := RegionCN
		if c.want {
			wantRegion = RegionIntl
		}
		if got := a.Region(); got != wantRegion {
			t.Errorf("Region(%q) = %v，应与 IsIntl 一致（%v）",
				c.domain, got, wantRegion)
		}
	}
}

// TestIsIntlDoesNotMatchLookalikeDomains 反向：**形似但无关**的域名不得命中。
//
// # 为什么必须测这一条
//
// 后缀匹配的另一个毛病是"过宽"：`HasSuffix(domain, ".ai")` 会把任何
// `.ai` 域名当成国际版。若我改成 `HasSuffix(domain, "qoder.sh")` 这种
// 宽松子串匹配，`notqoder.sh` 也会命中 —— 那是**另一类**假阳性。
//
// 正确的匹配是「等于，或以 `.＋清单项` 结尾」，故：
//
//	api3.qoder.sh   ✅ 命中（子域）
//	notqoder.sh     ❌ 不命中（不是子域，是不同域名）
//	qoder.sh.evil.com ❌ 不命中（清单项在中间）
func TestIsIntlDoesNotMatchLookalikeDomains(t *testing.T) {
	for _, d := range []string{
		"notqoder.sh",
		"qoder.sh.evil.com",
		"evil-workbuddy.ai.com", // .ai 在中间，TLD 是 .com
		"fakez.ai",
		"z.ai.evil.com",
		"qoder.shx",
		"myqoder.com",
	} {
		if (&Auth{Domain: d}).IsIntl() {
			t.Errorf("IsIntl(%q) 不应为 true —— 它只是形似，不是我们的域名", d)
		}
	}
}

// TestIsIntlIsCaseAndNoiseTolerant 凭证里的域名可能有大小写、空格、协议、端口。
//
// 这些不是假想：手写凭证与旧版导出都出现过带 URL 形态的 domain。
// 判错会让账号静默走直连 —— 与本次修的是同一类后果。
func TestIsIntlIsCaseAndNoiseTolerant(t *testing.T) {
	for _, d := range []string{
		"WWW.WorkBuddy.AI",
		"  www.workbuddy.ai  ",
		"https://www.workbuddy.ai",
		"https://www.workbuddy.ai/v2/chat",
		"www.workbuddy.ai:443",
		"https://qoder.sh/",
	} {
		if !(&Auth{Domain: d}).IsIntl() {
			t.Errorf("IsIntl(%q) 应为 true —— 归一化后它就是国际版域名", d)
		}
	}
	// 国服侧同样要容忍噪声（别把带端口的国服域误判成国际版）。
	for _, d := range []string{
		"HTTPS://Copilot.Tencent.COM",
		"copilot.tencent.com:443",
		"https://qoder.com.cn/path",
	} {
		if (&Auth{Domain: d}).IsIntl() {
			t.Errorf("IsIntl(%q) 不应为 true（是国服域名）", d)
		}
	}
}
