package zcode

// claim_headers_test.go claim / preview 请求头与真机一致性的回归测试。
//
// # 守的是什么（2026-09-20 按参考实现对齐）
//
// 参考实现（TriDefender/zcode-api `src/claim/client.ts`）的 claim 与 preview
// 是**两个不同的头集**，而我此前给两者都发了 `ControlPlaneHeaders` 整套身份头。
//
// 直觉上"多发头像更完整"，但上游风控的判据是**与真机一致**：
// 官方客户端在这条路径上只发那几个头，我们多发一堆反而是**可区分特征**。
// 参考实现特意记载过这个反复 —— 早期发全套（0828 活动期"经验上被接受"），
// 3.12.3 起**改回逐字镜像客户端**。
//
// # 两个具体的缺陷（本次修的）
//
//	① claim 缺 `X-ZCode-App-Version` + `X-Platform`
//	② preview 与 claim 发了**同一套**（本该 preview 更简）
//
// 另有第 ③ 条同样是缺陷：claim 路径本该用 **JWT**，而我此前靠
// `ControlPlaneHeaders` 间接带上凭证，语义不清。
import (
	"strings"
	"testing"
)

// testIdentity 一个取值齐备的 Identity（便于断言具体值）。
func testIdentity() Identity {
	return Identity{
		AppVersion:     "3.14.0",
		Platform:       "win32",
		Arch:           "x64",
		OSVersion:      "10.0.19045",
		SourceTitle:    "Z Code@1.0.0",
		RefererOrigin:  "https://zcode.z.ai",
		ClientLanguage: "zh-CN",
	}
}

// TestClaimHeadersHasAppVersionAndPlatform claim 必须带这两个头。
//
// 这是本轮修的核心缺陷：缺 `X-ZCode-App-Version` / `X-Platform`
// 会让我们的 claim 请求与真机不一致。
func TestClaimHeadersHasAppVersionAndPlatform(t *testing.T) {
	h := testIdentity().ClaimHeaders("11111111-2222-4333-8444-555555555555")
	if h["X-ZCode-App-Version"] != "3.14.0" {
		t.Errorf("claim 必须带 X-ZCode-App-Version=3.14.0，实际 %q（缺它 = 与真机不一致）",
			h["X-ZCode-App-Version"])
	}
	if h["X-Platform"] == "" {
		t.Error("claim 必须带 X-Platform（参考实现逐字包含它）")
	}
	if h["X-Device-Mid"] == "" {
		t.Error("claim 必须带 X-Device-Mid —— 缺它上游回 biz 3001 parameter error")
	}
}

// TestClaimHeadersAreMinimal claim **不该**带身份头 bundle。
//
// 这是"多送头是风险"的那条：参考实现的 claim 只有 6~7 个头。
// 若这里出现 User-Agent / X-Title / HTTP-Referer 等，说明又退回成
// `ControlPlaneHeaders` 了 —— 而那正是被对齐掉的旧行为。
func TestClaimHeadersAreMinimal(t *testing.T) {
	h := testIdentity().ClaimHeaders("11111111-2222-4333-8444-555555555555")
	// 这些是身份头 bundle 的成员，claim 路径不该有
	for _, forbidden := range []string{
		"User-Agent", "X-Title", "HTTP-Referer", "X-ZCode-Agent",
		"X-Os-Category", "X-Os-Version", "X-Client-Language", "X-Client-Timezone",
	} {
		if _, ok := h[forbidden]; ok {
			t.Errorf("claim 路径**不该**带 %s（参考实现只发 6~7 个头；"+
				"多发身份头是与真机可区分的特征）", forbidden)
		}
	}
}

// TestPreviewHeadersAreMinimal preview 只有 device-mid（其余由调用方补 Authorization）。
func TestPreviewHeadersAreMinimal(t *testing.T) {
	h := testIdentity().PreviewHeaders("11111111-2222-4333-8444-555555555555")
	if h["X-Device-Mid"] == "" {
		t.Error("preview 活动期也要求 X-Device-Mid（与 claim 同一实测结论）")
	}
	// preview 比 claim 更简：连 app-version / platform 都不发
	for _, forbidden := range []string{
		"X-ZCode-App-Version", "X-Platform",
		"User-Agent", "X-Title", "HTTP-Referer", "X-ZCode-Agent",
		"Content-Type", "Accept",
	} {
		if _, ok := h[forbidden]; ok {
			t.Errorf("preview **不该**带 %s（参考实现的 preview 是极简头，"+
				"甚至不发 Content-Type/Accept）", forbidden)
		}
	}
}

// TestPreviewHeadersWithoutDeviceMidIsEmpty 没有 deviceMid 时 preview 头为空。
//
// 匿名 preview 在参考实现里是**合法**的（拿到空清单，不是 401）。
func TestPreviewHeadersWithoutDeviceMidIsEmpty(t *testing.T) {
	h := testIdentity().PreviewHeaders("")
	if len(h) != 0 {
		t.Errorf("无 deviceMid 时 preview 头应为空，实际 %v", h)
	}
}

// TestClaimHeadersWithoutDeviceMidOmitsIt deviceMid 为空时**不发**该头。
//
// 保留"调用方可显式关闭"的能力（与 ControlPlaneHeaders 的既有语义一致）。
func TestClaimHeadersWithoutDeviceMidOmitsIt(t *testing.T) {
	h := testIdentity().ClaimHeaders("")
	if _, ok := h["X-Device-Mid"]; ok {
		t.Error("deviceMid 为空时不该发 X-Device-Mid")
	}
	// 但 app-version / platform 仍应存在（它们与设备无关）
	if h["X-ZCode-App-Version"] == "" || h["X-Platform"] == "" {
		t.Error("app-version / platform 与设备无关，应始终存在")
	}
}

// TestDefaultAppVersionFollowsClient 默认版本号必须跟上官方客户端。
//
// 参考实现最新提交把 3.12.3 → 3.14.0，作者原话是
// "每次客户端发版都要跟上，否则 UA 与 X-ZCode-App-Version 会变成可区分的特征"。
//
// ⚠ 这条断言会在上游再次发版时需要**手动更新** —— 那是刻意的：
// 它提醒维护者"该核对版本号了"，而不是让一个过旧的值悄悄留在代码里。
func TestDefaultAppVersionFollowsClient(t *testing.T) {
	if DefaultAppVersion != "3.14.0" {
		t.Errorf("DefaultAppVersion 应为 3.14.0（参考实现 master 的值）；"+
			"实际 %q —— 若上游又发版了，请核对后更新本条断言", DefaultAppVersion)
	}
	// 版本号形状应是 x.y.z
	parts := strings.Split(DefaultAppVersion, ".")
	if len(parts) != 3 {
		t.Errorf("版本号应为 x.y.z 形状，实际 %q", DefaultAppVersion)
	}
}

// TestControlPlaneHeadersUnchanged 控制面头（额度/账单）**行为不变**。
//
// 本次只改 claim/preview 两条路径；ControlPlaneHeaders 仍用于额度查询等，
// 它带整套身份头是**既有且正确**的行为（那些端点官方客户端确实发全套）。
func TestControlPlaneHeadersUnchanged(t *testing.T) {
	h := testIdentity().ControlPlaneHeaders("11111111-2222-4333-8444-555555555555")
	if h["X-Device-Mid"] == "" {
		t.Error("控制面头仍必须带 X-Device-Mid")
	}
	// 它**应当**带身份头（与 claim/preview 相反）
	if h["User-Agent"] == "" {
		t.Error("控制面头应带 User-Agent（它走身份 bundle，与 claim/preview 不同）")
	}
}
