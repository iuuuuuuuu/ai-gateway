package server

// 区域探测：**产品筛选** + **多账号重试**（所有者 2026-09-20 实测缺陷）。
//
// # 现场
//
// 所有者：「我按张湾打开,自动启动的兼容网关,点过来还是这个样子」——
// 网关刚启动，界面就报「国服未检测到可用真值」，而他**有 14 个正常国服账号**。
//
// # 根因链（每步可核）
//
//	① 网关把 Qoder 凭证也加载进同一个池（`qoder_auth_dir`）；
//	② 那个 Qoder 账号（domain=qoder.com.cn）没有 `product` 字段，
//	   `ProductOf()` 把空串归一成 workbuddy ⇒ 被当成 WorkBuddy 账号；
//	③ `qoder.com.cn` 不以 `.ai` 结尾 ⇒ `Region()` 判成 **cn**；
//	④ `ProbeUIDs()` 按 **uid 字典序**排序 ⇒ `zzzz0001…` 排在
//	   `{exp-uid}…` 之前 ⇒ **它被选中国服探测**；
//	⑤ 拿 Qoder 的 token 去打 WorkBuddy 的 `/v3/config` ⇒ **空清单**；
//	⑥ `len(infos)==0` ⇒ 判定国服"没拉到" ⇒ 全部 cn 侧模型带
//	   `unverified_regions=[cn]`。
//
// **两个错误叠加**：筛选缺失（不该用别的产品账号）+ 不重试（一个坏账号
// 就让整个区域变未知）。故两个都要修、两个都要锁。
//
// # 这个缺陷的教训：诊断信息是排查的前提
//
// 我最初只能看到 `last_error: <时间戳>`，查不出国服为什么失败 ——
// 我的独立探针用**同一个 exe、同一份配置**能拉到 16 个国服模型，
// 而网关拉不到。补上 `last_error_message` 后一步就定位到
// 「账号 zzzz0001 → models api returned empty list」。
// 故 `last_error_message` 也必须由测试锁定（见下）。

import (
	"net/http"
	"testing"
	"time"

	"workbuddy2api/internal/auth"
)

// TestProbeSkipsOtherProductsAccounts 探测不得使用别的产品的账号。
//
// 这是根因 ②③ 的直接回归：Qoder 的 token 对 WorkBuddy 端点无效，
// 拿它去探测会得到空清单，进而把整个区域误判成"没有真值"。
func TestProbeSkipsOtherProductsAccounts(t *testing.T) {
	// Qoder 账号的 uid 故意排在最前（字典序），复现所有者现场的选中顺序。
	qoderAcct := &auth.Auth{
		UID:         "zzzz0001-aaaa", // 字典序在 "cn-1" 之前（占位 uid）
		AccessToken: "dt-qoder-token",
		Domain:      "qoder.com.cn",
		Product:     auth.ProductQoder,
	}
	h := NewHandler(Config{
		Pool: testPoolWith(
			qoderAcct,
			&auth.Auth{UID: "cn-1", AccessToken: "t", Domain: "copilot.tencent.com", SoonestExpireAt: 1 << 40},
		),
		Upstream:  modelsFailUpstream(t),
		MaxRotate: 1,
	})

	got := h.probeAccountInRegion(auth.RegionCN)
	if got == nil {
		t.Fatal("国服有账号，不该返回 nil")
	}
	if got.ProductOf() != auth.ProductWorkBuddy {
		t.Errorf("探测只能用本产品账号，实际选中 %q（product=%q，domain=%q）—— "+
			"拿别的产品的 token 去探测会得到空清单，进而把整个区域误判成没有真值",
			got.UID, got.ProductOf(), got.Domain)
	}
	if got.UID == qoderAcct.UID {
		t.Errorf("绝不能用 Qoder 账号（uid=%s）探测 WorkBuddy 的区域真值", qoderAcct.UID)
	}
}

// TestProbeRetriesNextAccountOnFailure 一个账号失败要**试下一个**。
//
// 关键健壮性：即使筛选修好了，凭证目录里仍可能有异常账号
//（用户手动放的、旧版本残留的）。让"一个坏账号拖垮整个区域"不可能发生，
// 才是真正的修法 —— 所有者有 14 个国服账号，一个坏了不该全变未知。
func TestProbeRetriesNextAccountOnFailure(t *testing.T) {
	resetModelsCache()

	tried := []string{}
	up := newFakeUpstream(t, func(authz string) (int, string, bool) {
		tried = append(tried, authz)
		// 第一个账号（token=bad）返回**空清单** —— 复现 Qoder 账号的行为
		if authz == "Bearer bad" {
			return http.StatusOK, `{"code":0,"data":{"agents":[],"models":[]}}`, false
		}
		return http.StatusOK, v3WithModels("cn-only-model"), false
	})
	h := NewHandler(Config{
		Pool: testPoolWith(
			// uid 字典序：bad-1 在 good-1 之前 ⇒ 先试坏的
			&auth.Auth{UID: "bad-1", AccessToken: "bad", Domain: "copilot.tencent.com", SoonestExpireAt: 1 << 40},
			&auth.Auth{UID: "good-1", AccessToken: "good", Domain: "copilot.tencent.com", SoonestExpireAt: 1 << 40},
		),
		Upstream:  up,
		MaxRotate: 1,
	})

	infos := h.fetchModelsForRegion(auth.RegionCN)
	if len(infos) == 0 {
		t.Fatalf("第一个账号失败后应重试下一个并成功；实际拿到 0 个模型"+
			"（试过的 token：%v）—— 那正是所有者现场："+
			"一个坏账号让整个国服变成「未检测到真值」，而他其实有 14 个正常账号", tried)
	}
	if len(tried) < 2 {
		t.Errorf("应当试过至少 2 个账号，实际 %d 次：%v", len(tried), tried)
	}
}

// TestProbeFailureMessageNamesAccountAndReason 失败时必须留下**可排查**的原因。
//
// 没有它，我最初只能看到一个时间戳，查不出国服为什么失败 ——
// 而这正是这个缺陷拖了几轮才找到的**直接原因**。
func TestProbeFailureMessageNamesAccountAndReason(t *testing.T) {
	resetModelsCache()

	up := newFakeUpstream(t, func(string) (int, string, bool) {
		return http.StatusOK, `{"code":0,"data":{"agents":[],"models":[]}}`, false
	})
	h := NewHandler(Config{
		Pool: testPoolWith(
			&auth.Auth{UID: "zzzz0001-cccc", AccessToken: "t", Domain: "copilot.tencent.com", SoonestExpireAt: 1 << 40},
		),
		Upstream:  up,
		MaxRotate: 1,
	})

	if got := h.fetchModelsForRegion(auth.RegionCN); len(got) != 0 {
		t.Fatalf("夹具应当让拉取失败，实际拿到 %d 个模型", len(got))
	}

	regionModelCache.Lock()
	msg := ""
	if rm := regionModelCache.byRegion[auth.RegionCN]; rm != nil {
		msg = rm.lastErrMsg
	}
	regionModelCache.Unlock()

	if msg == "" {
		t.Fatal("失败必须记下原因（last_error_message）—— " +
			"只记时间戳等于没记：我最初就是因为这个查不出国服为什么失败")
	}
	// 必须指认**哪个账号**（短 uid 足够）
	if !contains(msg, "zzzz0001") {
		t.Errorf("原因里应指认失败的账号（前 8 位 uid），实际：%s", msg)
	}
	// 必须说清**为什么**（空清单 / 错误）
	if !contains(msg, "0 个模型") && !contains(msg, "→") {
		t.Errorf("原因里应说清失败形态，实际：%s", msg)
	}
}

// TestProbeStopsAfterLimit 账号很多时不能无限试（避免打上游）。
func TestProbeStopsAfterLimit(t *testing.T) {
	resetModelsCache()

	calls := 0
	up := newFakeUpstream(t, func(string) (int, string, bool) {
		calls++
		return http.StatusOK, `{"code":0,"data":{"agents":[],"models":[]}}`, false
	})
	var auths []*auth.Auth
	for _, uid := range []string{"a-1", "b-2", "c-3", "d-4", "e-5", "f-6"} {
		auths = append(auths, &auth.Auth{
			UID: uid, AccessToken: "t", Domain: "copilot.tencent.com", SoonestExpireAt: 1 << 40,
		})
	}
	h := NewHandler(Config{Pool: testPoolWith(auths...), Upstream: up, MaxRotate: 1})

	_ = h.fetchModelsForRegion(auth.RegionCN)
	if calls > maxProbeAccounts {
		t.Errorf("探测最多试 %d 个账号，实际 %d 次 —— 账号多的用户会被打很多上游请求",
			maxProbeAccounts, calls)
	}
	if calls == 0 {
		t.Error("至少该试一个账号")
	}
}

// TestProbeSuccessClearsFailureMessage 成功要把失败状态**清干净**。
//
// 否则界面会一边显示真值、一边留着一条过期的失败原因，误导排查。
func TestProbeSuccessClearsFailureMessage(t *testing.T) {
	resetModelsCache()

	fail := true
	up := newFakeUpstream(t, func(string) (int, string, bool) {
		if fail {
			return http.StatusOK, `{"code":0,"data":{"agents":[],"models":[]}}`, false
		}
		return http.StatusOK, v3WithModels("cn-only-model"), false
	})
	h := NewHandler(Config{
		Pool: testPoolWith(
			&auth.Auth{UID: "cn-1", AccessToken: "t", Domain: "copilot.tencent.com", SoonestExpireAt: 1 << 40},
		),
		Upstream:  up,
		MaxRotate: 1,
	})

	_ = h.fetchModelsForRegion(auth.RegionCN)

	// 把失败时刻拨到退避窗口外，再让它成功
	regionModelCache.Lock()
	if rm := regionModelCache.byRegion[auth.RegionCN]; rm != nil {
		if rm.lastErrMsg == "" {
			regionModelCache.Unlock()
			t.Fatal("第一轮失败应留下原因")
		}
		rm.lastErr = time.Now().Add(-time.Hour)
	}
	regionModelCache.Unlock()

	fail = false
	if got := h.fetchModelsForRegion(auth.RegionCN); len(got) == 0 {
		t.Fatal("第二轮应当成功")
	}

	regionModelCache.Lock()
	msg := ""
	if rm := regionModelCache.byRegion[auth.RegionCN]; rm != nil {
		msg = rm.lastErrMsg
	}
	regionModelCache.Unlock()

	if msg != "" {
		t.Errorf("成功后应清掉失败原因（否则界面会留着过期的错误误导排查），实际残留：%s", msg)
	}
}

// contains 是 strings.Contains 的短别名，避免本文件再引一个 import。
func contains(s, sub string) bool {
	return len(sub) == 0 || (len(s) >= len(sub) && indexOf(s, sub) >= 0)
}

func indexOf(s, sub string) int {
	for i := 0; i+len(sub) <= len(s); i++ {
		if s[i:i+len(sub)] == sub {
			return i
		}
	}
	return -1
}
