package zcode

// 验证码求解的契约测试。
//
// # 背景（实测出来的三条硬约束）
//
// ZCode 对话端点回 `400 {"code":3007,"msg":"captcha verify failed"}`，
// 需要 `X-Aliyun-Captcha-Verify-Param`。而求解这件事有三个坑：
//
//  1. **param 是一次性的** —— 第二次用必回 3007
//  2. **求解会被限流** —— 连续几次后 `pe-stall`，需冷却
//  3. **调试信息在 stderr、结果在 stdout** —— 合并会误判成失败
//
// 这三条我都实际踩过，故逐条钉住。另外还有一条**它解决不了什么**：
//
//	带上有效 param 后 3007 消失，但会出现 3012「unusual activity」——
//	那是账号/风控层，与 deviceMid、param 复用都无关（三种 deviceMid 实测都 3012）。
//
// 测试要防止的是"以为集成完就万事大吉"—— 故也钉住"不承诺对话一定通"。

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// TestSolverWithoutDirIsUnavailable 没有组件目录时必须**如实报不可用**。
//
// 为什么重要：发行包漏带 assets 时，用户看到的现象会是"启用了验证码但一直
// 失败"。如实报错才能让他知道是组件缺失，而不是账号问题。
func TestSolverWithoutDirIsUnavailable(t *testing.T) {
	s := &CaptchaSolver{}
	if s.Available() {
		t.Error("没有目录时不该报告可用")
	}
	reason := s.UnavailableReason()
	if !strings.Contains(reason, "未安装") && !strings.Contains(reason, "缺失") {
		t.Errorf("不可用原因应点明组件缺失，实际：%s", reason)
	}
	// 求解应返回错误而不是 panic
	if _, err := s.Solve(context.Background()); err == nil {
		t.Error("没有目录时 Solve 应返回错误")
	}
}

// TestSolverMissingSolverJSReportsComponent 目录在但入口文件都不在 → 报组件缺失。
func TestSolverMissingSolverJSReportsComponent(t *testing.T) {
	dir := t.TempDir()
	s := &CaptchaSolver{dir: dir}
	if s.Available() {
		t.Error("缺少入口文件时不该报告可用")
	}
	got := s.UnavailableReason()
	// 报错要同时点明两个候选名 —— 只提一个是误导：
	// 排查者会以为"补上 solver.js 就行"，而实际优先用的是 bundle
	for _, want := range []string{captchaEntryBundle, captchaEntrySource} {
		if !strings.Contains(got, want) {
			t.Errorf("应点明 %s 缺失，实际：%s", want, got)
		}
	}
}

// TestSolverPrefersBundle 有 bundle 时优先用它（那是为安装提速打的）。
//
// # 为什么这条重要
//
// 打包成单文件是**为了把安装时的文件写入从 3353 次降到 1 次** ——
// 所有者专门反馈过「安装的时候那个 node_modules 解压速度超级慢」。
// 若代码仍去跑 solver.js（源码），而源码依赖 node_modules ——
// 那 bundle 就白打了，安装慢的问题会原样回来。
func TestSolverPrefersBundle(t *testing.T) {
	dir := t.TempDir()
	// 两个都放，验证优先选 bundle
	if err := os.WriteFile(filepath.Join(dir, captchaEntrySource), []byte("// source"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, captchaEntryBundle), []byte("// bundle"), 0o644); err != nil {
		t.Fatal(err)
	}
	s := &CaptchaSolver{dir: dir}
	got, err := s.entryFile()
	if err != nil {
		t.Fatalf("两个入口都在，不该报错：%v", err)
	}
	if got != captchaEntryBundle {
		t.Errorf("应优先用 %s，实际 %s", captchaEntryBundle, got)
	}
}

// TestSolverFallsBackToSource 没有 bundle 时回退到源码入口。
//
// 保留源码是"打包产物不可读"的补偿：某台机器上 bundle 出问题时，
// 至少还能就地用源码诊断。
func TestSolverFallsBackToSource(t *testing.T) {
	dir := t.TempDir()
	if err := os.WriteFile(filepath.Join(dir, captchaEntrySource), []byte("// source"), 0o644); err != nil {
		t.Fatal(err)
	}
	s := &CaptchaSolver{dir: dir}
	got, err := s.entryFile()
	if err != nil {
		t.Fatalf("有源码入口时不该报错：%v", err)
	}
	if got != captchaEntrySource {
		t.Errorf("无 bundle 时应回退到 %s，实际 %s", captchaEntrySource, got)
	}
}

// TestSolverWithoutNodeReportsNode 有入口文件但没有 node → 报缺 Node。
//
// 这是**最可能出现的真实情形**：发行包带了组件，但用户机器没装 Node。
// 报错必须点明"要装 Node"，否则用户会以为是账号问题。
func TestSolverWithoutNodeReportsNode(t *testing.T) {
	dir := t.TempDir()
	// 放一个空的 bundle 让它过第一步检查
	if err := os.WriteFile(filepath.Join(dir, captchaEntryBundle), []byte("// stub"), 0o644); err != nil {
		t.Fatal(err)
	}
	// 把 PATH 清掉并指定一个不存在的 ZCODE_NODE_PATH，确保探测失败
	t.Setenv("PATH", "")
	t.Setenv("ZCODE_NODE_PATH", filepath.Join(dir, "no-such-node.exe"))

	s := &CaptchaSolver{dir: dir}
	if s.Available() {
		t.Error("没有 node 时不该报告可用")
	}
	got := s.UnavailableReason()
	if !strings.Contains(got, "node") {
		t.Errorf("应点明缺 node，实际：%s", got)
	}
}

// TestSolverCooldownBlocksRetry 冷却期内**直接拒绝**，不再去撞上游。
//
// 为什么必须有：实测连续求解会被限流（`pe-stall`）。没有冷却的话，
// 网关会不断重试 → 一直撞限流 → 反而更久拿不到。
func TestSolverCooldownBlocksRetry(t *testing.T) {
	s := &CaptchaSolver{dir: t.TempDir()}
	// 手动置入冷却
	s.cooldownUntil = time.Now().Add(30 * time.Second)
	s.cooldownReason = "测试：被上游限流"

	if left := s.CooldownLeft(); left <= 0 {
		t.Fatal("应处于冷却中")
	}

	_, err := s.Solve(context.Background())
	if err == nil {
		t.Fatal("冷却期内 Solve 应直接返回错误，而不是去请求上游")
	}
	if !strings.Contains(err.Error(), "冷却") {
		t.Errorf("错误应点明处于冷却，实际：%v", err)
	}
	// 报错里要带剩余时间，用户才知道等多久
	if !strings.Contains(err.Error(), "还需") {
		t.Errorf("错误应含剩余等待时间，实际：%v", err)
	}
}

// TestSolverCooldownExpires 冷却过期后可以再试。
func TestSolverCooldownExpires(t *testing.T) {
	s := &CaptchaSolver{dir: t.TempDir()}
	s.cooldownUntil = time.Now().Add(-time.Second) // 已过期
	if left := s.CooldownLeft(); left != 0 {
		t.Errorf("冷却已过期时 CooldownLeft 应为 0，实际 %v", left)
	}
}

// TestSetCaptchaConfigFromUpstream 上游配置可覆盖三个运营参数。
//
// scene/region/prefix 都是**运营参数**（实测来自
// `/api/v1/client/configs` 的 `configs.captcha`），上游随时可改。
// 硬编码会让上游一改配置我们就解不出来。
func TestSetCaptchaConfigFromUpstream(t *testing.T) {
	// ⚠ 必须用 NewCaptchaSolver()：直接 `&CaptchaSolver{}` 的
	// scene/region/prefix 都是空串，求解必然失败。这条测试最初就是
	// 因为构造函数没设默认值而失败 —— 它抓住了一个真实缺陷。
	s := NewCaptchaSolver()
	if s.scene != defaultCaptchaScene || s.region != defaultCaptchaRegion || s.prefix != defaultCaptchaPrefix {
		t.Fatalf("初始值应是实测默认：%s/%s/%s", s.scene, s.region, s.prefix)
	}
	s.SetCaptchaConfig("newscene", "sgp", "newprefix")
	if s.scene != "newscene" || s.region != "sgp" || s.prefix != "newprefix" {
		t.Errorf("配置没被覆盖：%s/%s/%s", s.scene, s.region, s.prefix)
	}
	// 空值不该覆盖（上游偶尔漏字段，不能把默认值冲掉）
	s.SetCaptchaConfig("", "", "")
	if s.scene != "newscene" || s.region != "sgp" {
		t.Errorf("空值不该覆盖已有配置：%s/%s/%s", s.scene, s.region, s.prefix)
	}
}

// TestTraceHeadersAreStartPlanChannel 追踪头**只发三个**。
//
// 参考实现（zcode2api 的 identity.py）明确记载：
//
//	「误发会触发上游 3012 "unusual activity"：
//	  start-plan（JWT 通道）只发 x-request-id / x-zcode-session-type /
//	  x-zcode-trace-id 三个头，**不发** x-query-id / x-session-id。」
//
// 我们走 JWT 通道，故多一个都可能是错的。
func TestTraceHeadersAreStartPlanChannel(t *testing.T) {
	h := Identity{}.TraceHeaders()

	if len(h) != 3 {
		t.Errorf("start-plan 通道应只发 3 个追踪头，实际 %d 个：%v", len(h), keysOf(h))
	}
	for _, want := range []string{"x-request-id", "x-zcode-session-type", "x-zcode-trace-id"} {
		if h[want] == "" {
			t.Errorf("缺少必需追踪头 %q", want)
		}
	}
	// 这两个**必须不发**（发了会触发 3012）
	for _, forbidden := range []string{"x-query-id", "x-session-id"} {
		if _, ok := h[forbidden]; ok {
			t.Errorf("%q 属于 coding-plan 通道，JWT 通道发了会触发 3012", forbidden)
		}
	}
	if h["x-zcode-session-type"] != "main" {
		t.Errorf("x-zcode-session-type 应为 main，实际 %q", h["x-zcode-session-type"])
	}
}

// TestTraceHeadersAreFreshPerCall 每次调用要生成**新的** id。
//
// 追踪头标识单次请求；复用同一个 id 会让上游看到"同一请求发了两次"。
func TestTraceHeadersAreFreshPerCall(t *testing.T) {
	a := Identity{}.TraceHeaders()
	b := Identity{}.TraceHeaders()
	if a["x-request-id"] == b["x-request-id"] {
		t.Error("两次调用的 x-request-id 不该相同")
	}
	if a["x-zcode-trace-id"] == b["x-zcode-trace-id"] {
		t.Error("两次调用的 x-zcode-trace-id 不该相同")
	}
	// 形态必须是 UUID（上游对非 UUID 会拒）
	if !IsUUID(a["x-request-id"]) {
		t.Errorf("x-request-id 应是 UUID 形态，实际 %q", a["x-request-id"])
	}
}

// TestCaptchaParamIsNotPersisted 验证码不该被写进凭证文件。
//
// 它是**一次性**的瞬时值，不是账号属性。若被持久化：
//   · 下次启动会拿一个已作废的 param 去请求 → 必然 3007
//   · 而且它会落盘成明文，是不必要的秘密扩散
func TestCaptchaParamIsNotPersisted(t *testing.T) {
	dir := t.TempDir()
	c := &Cred{
		UID:           "zcode-test",
		Credential:    "a.b",
		CaptchaParam:  "should-not-persist",
		CaptchaRegion: "cn",
		FilePath:      filepath.Join(dir, "zcode-test.json"),
	}
	if err := c.SaveAtomic(); err != nil {
		t.Fatalf("保存失败: %v", err)
	}
	raw, err := os.ReadFile(filepath.Join(dir, "zcode-test.json"))
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(raw), "should-not-persist") {
		t.Error("验证码是瞬时的，不该被写进凭证文件")
	}
}

func keysOf(m map[string]string) []string {
	out := make([]string, 0, len(m))
	for k := range m {
		out = append(out, k)
	}
	return out
}
