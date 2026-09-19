package pool

// auths 目录热加载（watch.go）的回归用例。
//
// 缺陷背景：账号池此前只在**进程启动时**扫一次凭证目录，运行中新增的凭证文件
// 不会进池 —— 表现为宿主界面显示「已添加」而网关侧「未加载」，必须手动点
// 「重启」才生效（宿主代码里把这条限制写成了注释）。
//
// 本文件锁住四件事：
//  1. 写一个新凭证文件 → 等一轮 → 池里出现该账号（缺陷本身）；
//  2. 文件没变时不重复加载（幂等），且热加载**不重置**既有账号的运行态；
//  3. 指纹对「新增 / 修改 / 删除」三种变化都敏感（否则热加载会漏触发）；
//  4. 剔除范围 —— 只扫 WorkBuddy 目录的热加载**不得**删掉 Qoder / ZCode 账号。

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
	"time"

	"workbuddy2api/internal/auth"
)

// writeWatchAuthFile 落一个最小可解析的凭证文件（嵌套形，与 internal/auth 一致）。
func writeWatchAuthFile(t *testing.T, dir, uid string) string {
	t.Helper()
	doc := map[string]any{
		"account": map[string]any{"uid": uid, "nickname": "昵称-" + uid},
		"auth": map[string]any{
			"accessToken":  "tok-" + uid,
			"refreshToken": "rt-" + uid,
			"expiresAt":    time.Now().Add(24 * time.Hour).Unix(),
			"domain":       "www.workbuddy.cn",
		},
	}
	raw, err := json.Marshal(doc)
	if err != nil {
		t.Fatal(err)
	}
	fp := filepath.Join(dir, "workbuddy-"+uid+".json")
	if err := os.WriteFile(fp, raw, 0o600); err != nil {
		t.Fatal(err)
	}
	return fp
}

// loadAndSync 走生产同一条路径：auth.LoadDir → p.SyncToDir（建立启动基线）。
func loadAndSync(t *testing.T, p *Pool, dir string) {
	t.Helper()
	auths, err := auth.LoadDir(dir)
	if err != nil {
		t.Fatalf("LoadDir(%s): %v", dir, err)
	}
	p.SyncToDir(auths)
}

// TestWatchAddsNewAccountFile 缺陷主路径：写一个新凭证文件 → 等一轮 → 池里出现该账号。
//
// 这里刻意用**真实轮询周期**（20ms）而不是直接调 reloadAuthDir：
// 要验证的正是「监听循环确实会在指纹变化后触发加载」这条链路，
// 只测 reloadAuthDir 会把「Ticker 没跑起来 / 指纹比较写反」这类缺陷放过去。
func TestWatchAddsNewAccountFile(t *testing.T) {
	dir := t.TempDir()
	writeWatchAuthFile(t, dir, "u1")

	p := New(filepath.Join(t.TempDir(), "state.json"))
	loadAndSync(t, p, dir)
	if n := len(p.AllUIDs()); n != 1 {
		t.Fatalf("前置：初始账号数=%d want 1", n)
	}

	stop := p.StartAuthDirWatchWithInterval(dir, 20*time.Millisecond)
	defer stop()

	// 新增第二个账号：不重启、不手动同步，只等监听循环自己发现。
	writeWatchAuthFile(t, dir, "u2")

	if !waitFor(2*time.Second, func() bool { return len(p.AllUIDs()) == 2 }) {
		t.Fatalf("新账号未在监听周期内进池，当前 %v", p.AllUIDs())
	}
	if _, ok := p.Status("u2"); !ok {
		t.Errorf("u2 应在池中: %v", p.AllUIDs())
	}
	if got := p.Pick(); got == nil {
		t.Error("热加载后池应可选出账号")
	}
}

// TestWatchIdempotentWhenUnchanged 文件没变时不重复加载（幂等性）。
//
// 「不重复加载」怎么观测：本用例用 **mtime 之外**的手段 —— 直接比对
// 池内账号的凭证指针是否被换掉。若每轮都无条件重扫，u1 的 *auth.Auth
// 会被新解析的实例替换（指针变化），而正确的实现应当**一次都不换**。
//
// 为什么这条重要：热加载每轮都重建凭证对象会让「正在刷新的 token」
// 与池内对象分叉（刷新写的是旧对象，池里已是新对象），表现为
//「保活刷了 token 但请求仍用旧的」。指纹比对就是为此存在的。
func TestWatchIdempotentWhenUnchanged(t *testing.T) {
	dir := t.TempDir()
	writeWatchAuthFile(t, dir, "u1")

	p := New(filepath.Join(t.TempDir(), "state.json"))
	loadAndSync(t, p, dir)
	before := p.AuthByUID("u1")
	if before == nil {
		t.Fatal("前置：u1 应在池中")
	}

	stop := p.StartAuthDirWatchWithInterval(dir, 20*time.Millisecond)
	defer stop()

	// 等足够多轮（≥5 轮）确认监听循环确实在跑，但目录内容一直没变。
	time.Sleep(150 * time.Millisecond)

	if after := p.AuthByUID("u1"); after != before {
		t.Error("目录未变化时不该重新加载（凭证对象被换掉了）—— 指纹比对未生效")
	}
}

// TestWatchPreservesRuntimeState 热加载不得重置既有账号的冷却/统计。
//
// 这是本特性最关键的契约：upsertLocked 对已存在账号只换凭证。若哪天被改成
// 「重建条目」，用户每次加号都会把全池冷却清空（等于绕过限流惩罚），
// 而且从界面上完全看不出来 —— 属于严重回归，必须有用例钉住。
func TestWatchPreservesRuntimeState(t *testing.T) {
	dir := t.TempDir()
	writeWatchAuthFile(t, dir, "u1")

	p := New(filepath.Join(t.TempDir(), "state.json"))
	loadAndSync(t, p, dir)

	// 给 u1 制造状态：硬冷却 + 计数。
	p.Cooldown("u1", CoolHard, time.Hour, "余额不足")
	p.NoteError("u1")
	p.NoteSuccess("u1")
	before, _ := p.Status("u1")

	stop := p.StartAuthDirWatchWithInterval(dir, 20*time.Millisecond)
	defer stop()

	// 改 u1 的凭证内容（同名覆盖）+ 新增 u2 → 触发一次真实热加载。
	time.Sleep(10 * time.Millisecond) // 拉开 mtime，确保指纹确实变化
	writeWatchAuthFile(t, dir, "u1")
	writeWatchAuthFile(t, dir, "u2")

	if !waitFor(2*time.Second, func() bool { return len(p.AllUIDs()) == 2 }) {
		t.Fatalf("热加载未触发，当前 %v", p.AllUIDs())
	}

	after, _ := p.Status("u1")
	if !after.Cooling || after.CoolKind != before.CoolKind {
		t.Errorf("热加载清掉了冷却: %+v → %+v", before, after)
	}
	if after.Credits != before.Credits {
		t.Errorf("credits 被重置: %d → %d", before.Credits, after.Credits)
	}
	if after.SuccessCount != before.SuccessCount || after.ErrTotal != before.ErrTotal {
		t.Errorf("计数被重置: succ %d→%d, err %d→%d",
			before.SuccessCount, after.SuccessCount, before.ErrTotal, after.ErrTotal)
	}
}

// TestWatchRemovesDeletedFile 凭证文件被删除的账号应从池中剔除。
//
// 与 SyncToDir 的既有语义一致：删号要真的从池里消失，否则它会一直参与选号，
// 而它已经没有可用凭证了（表现为持续 401/12153）。
func TestWatchRemovesDeletedFile(t *testing.T) {
	dir := t.TempDir()
	writeWatchAuthFile(t, dir, "u1")
	fp2 := writeWatchAuthFile(t, dir, "u2")

	p := New(filepath.Join(t.TempDir(), "state.json"))
	loadAndSync(t, p, dir)
	if n := len(p.AllUIDs()); n != 2 {
		t.Fatalf("前置：账号数=%d want 2", n)
	}

	stop := p.StartAuthDirWatchWithInterval(dir, 20*time.Millisecond)
	defer stop()

	if err := os.Remove(fp2); err != nil {
		t.Fatal(err)
	}
	if !waitFor(2*time.Second, func() bool { return len(p.AllUIDs()) == 1 }) {
		t.Fatalf("删除文件后账号未剔除，当前 %v", p.AllUIDs())
	}
	if _, ok := p.Status("u2"); ok {
		t.Error("u2 的凭证已删除，不该留在池中")
	}
}

// TestWatchKeepsOtherProducts 热加载**不得**剔除 Qoder / ZCode 账号。
//
// 这是本项目特有的约束（上游没有多产品）：池是三个产品共用的，Qoder / ZCode
// 账号由 main.go 用 p.Add 直接塞进同一个池，而热加载只扫 WorkBuddy 的 auths
// 目录。若热加载直接调 SyncToDir（按 uid 全量剔除），一次热加载就会把另外
// 两个平台的账号全部删出池子 —— 症状是「往 WorkBuddy 加了个号，Qoder 和
// ZCode 的账号全不见了」，且要重启才回来。
func TestWatchKeepsOtherProducts(t *testing.T) {
	dir := t.TempDir()
	writeWatchAuthFile(t, dir, "wb-1")

	p := New(filepath.Join(t.TempDir(), "state.json"))
	loadAndSync(t, p, dir)

	// 模拟 main.go 的多产品加载：凭证来自**别的**目录，走 p.Add。
	p.Add(&auth.Auth{
		UID: "qoder-1", AccessToken: "dt", Product: auth.ProductQoder,
		FilePath: filepath.Join(t.TempDir(), "qoder-auths", "qoder-qoder-1.json"),
	})
	p.Add(&auth.Auth{
		UID: "zcode-1", AccessToken: "cred", Product: auth.ProductZcode,
		FilePath: filepath.Join(t.TempDir(), "zcode-auths", "zcode-zcode-1.json"),
	})
	if n := len(p.AllUIDs()); n != 3 {
		t.Fatalf("前置：账号数=%d want 3 (%v)", n, p.AllUIDs())
	}

	stop := p.StartAuthDirWatchWithInterval(dir, 20*time.Millisecond)
	defer stop()

	// 触发一次真实热加载：改 WorkBuddy 目录内容。
	writeWatchAuthFile(t, dir, "wb-2")
	if !waitFor(2*time.Second, func() bool { return len(p.AllUIDs()) == 4 }) {
		t.Fatalf("热加载未按预期加入 wb-2，当前 %v", p.AllUIDs())
	}

	for _, uid := range []string{"qoder-1", "zcode-1"} {
		if _, ok := p.Status(uid); !ok {
			t.Errorf("%s 被热加载误剔除（池是三个产品共用的）: %v", uid, p.AllUIDs())
		}
	}
}

// TestWatchDoesNotDropPoolOnUnreadableDir 目录暂不可读时**不清空**池。
//
// 原子替换 / 网络盘抖动的瞬间目录可能读不到。若把「读不到」当成「所有文件都被
// 删了」，一次抖动就会清空整个账号池（随后所有请求 503），而且要重启才恢复。
// 监听循环必须把这一轮当成「没有变化」跳过。
//
// 这里走的是**运行期真实路径**：先以可读目录启动监听，运行中把目录移走
//（Windows 上等价于「暂不可读」，且比改权限更可靠、不需要管理员），
// 再确认若干轮之后池里一个账号都没少。
func TestWatchDoesNotDropPoolOnUnreadableDir(t *testing.T) {
	dir := t.TempDir()
	writeWatchAuthFile(t, dir, "u1")

	p := New(filepath.Join(t.TempDir(), "state.json"))
	loadAndSync(t, p, dir)

	stop := p.StartAuthDirWatchWithInterval(dir, 20*time.Millisecond)
	defer stop()

	// 运行中把目录整体移走：此后 dirFingerprint 返回 ok=false。
	gone := filepath.Join(t.TempDir(), "gone")
	if err := os.Rename(dir, gone); err != nil {
		t.Fatal(err)
	}
	if _, ok := dirFingerprint(dir); ok {
		t.Fatal("前置：目录已移走，指纹应报告不可读")
	}

	// 等足够多轮（≥5 轮）：若循环把「不可读」当成「目录已空」，账号会被剔除。
	time.Sleep(150 * time.Millisecond)

	if n := len(p.AllUIDs()); n != 1 {
		t.Errorf("目录不可读时不该剔除账号，当前 %v", p.AllUIDs())
	}

	// 恢复目录后必须重新开始热加载（说明只是跳过，不是永久停摆）。
	if err := os.Rename(gone, dir); err != nil {
		t.Fatal(err)
	}
	writeWatchAuthFile(t, dir, "u2")
	if !waitFor(2*time.Second, func() bool { return len(p.AllUIDs()) == 2 }) {
		t.Fatalf("目录恢复后监听应继续工作，当前 %v", p.AllUIDs())
	}
}

// TestDirFingerprintDetectsChanges 指纹对「新增 / 修改 / 删除」都必须敏感。
//
// 漏掉任一种都会表现为「某些加号方式不生效」：只比文件名会漏掉凭证刷新，
// 只比 mtime 会在同秒内的替换上漏判（故指纹里带 size）。
func TestDirFingerprintDetectsChanges(t *testing.T) {
	dir := t.TempDir()

	base, ok := dirFingerprint(dir)
	if !ok {
		t.Fatal("空目录应可读（ok=true）")
	}
	if base != "" {
		t.Errorf("空目录指纹应为空串，实际 %q", base)
	}

	// 新增
	fp1 := writeWatchAuthFile(t, dir, "u1")
	afterAdd, _ := dirFingerprint(dir)
	if afterAdd == base {
		t.Error("新增文件后指纹应变化")
	}

	// 内容修改（同名覆盖，模拟凭证刷新）：文件名不变，仅 mtime/size 变
	time.Sleep(10 * time.Millisecond)
	if err := os.WriteFile(fp1, []byte(`{"auth":{"accessToken":"new"},"account":{"uid":"u1","nickname":"改过的名字"}}`), 0o600); err != nil {
		t.Fatal(err)
	}
	afterMod, _ := dirFingerprint(dir)
	if afterMod == afterAdd {
		t.Error("同名覆盖后指纹应变化（凭证刷新场景）")
	}

	// 非 .json 文件不参与指纹（避免日志等噪音触发无谓重扫）
	if err := os.WriteFile(filepath.Join(dir, "readme.txt"), []byte("x"), 0o600); err != nil {
		t.Fatal(err)
	}
	if fp, _ := dirFingerprint(dir); fp != afterMod {
		t.Error("非 .json 文件不应影响指纹")
	}

	// 删除
	if err := os.Remove(fp1); err != nil {
		t.Fatal(err)
	}
	if fp, _ := dirFingerprint(dir); fp != base {
		t.Errorf("删除后应回到空目录指纹，实际 %q", fp)
	}
}

// TestDirFingerprintUnreadable 目录不可读返回 ok=false（调用方据此跳过本轮）。
func TestDirFingerprintUnreadable(t *testing.T) {
	if _, ok := dirFingerprint(filepath.Join(t.TempDir(), "nope")); ok {
		t.Error("不可读目录应返回 ok=false")
	}
}

// TestWatchStopIsIdempotent 停止函数可重复调用（defer stop() 与显式调用撞车是常见写法）。
func TestWatchStopIsIdempotent(t *testing.T) {
	dir := t.TempDir()
	writeWatchAuthFile(t, dir, "u1")

	p := New(filepath.Join(t.TempDir(), "state.json"))
	loadAndSync(t, p, dir)

	stop := p.StartAuthDirWatchWithInterval(dir, 20*time.Millisecond)
	stop()
	stop() // 不得 panic

	// 停止后不再热加载。
	writeWatchAuthFile(t, dir, "u2")
	time.Sleep(120 * time.Millisecond)
	if n := len(p.AllUIDs()); n != 1 {
		t.Errorf("停止后不该继续热加载，当前 %v", p.AllUIDs())
	}
}

// TestWatchNoopOnEmptyOrBadDir 空目录配置与不可读目录都返回可用的 no-op 停止函数。
//
// 「监听失败不该拖垮网关」：加账号仍可用「手动重启」这条既有退路，
// 因此这里只记日志，绝不 panic、绝不拦住启动。
func TestWatchNoopOnEmptyOrBadDir(t *testing.T) {
	p := New(filepath.Join(t.TempDir(), "state.json"))

	stop := p.StartAuthDirWatch("")
	stop()
	stop()

	stop2 := p.StartAuthDirWatchWithInterval(filepath.Join(t.TempDir(), "missing"), time.Millisecond)
	stop2()
	stop2()
}

// TestWatchDefaultIntervalIsFiveSeconds 默认周期必须是 5s（上游口径）。
//
// 这条不是凑数：默认值写在两处（watchInterval 与 config 的 "5s"），
// 两者一旦分叉，日志里报的周期与真实周期就会不一致，排查「为什么没热加载」
// 时会往错误方向查。
func TestWatchDefaultIntervalIsFiveSeconds(t *testing.T) {
	if watchInterval != 5*time.Second {
		t.Errorf("默认轮询周期=%v want 5s", watchInterval)
	}
}

// waitFor 在 timeout 内轮询 cond，满足即返回 true（超时返回 false）。
//
// 用轮询而不是固定 Sleep：热加载走真实 Ticker，固定睡眠在慢机器上会 flake
//（本仓库既有测试吃过这个亏）。上限 2s 远大于 20ms 的周期，正常机器上是毫秒级返回。
func waitFor(timeout time.Duration, cond func() bool) bool {
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		if cond() {
			return true
		}
		time.Sleep(5 * time.Millisecond)
	}
	return cond()
}
