package main

// 凭证目录热加载的配置层用例（pool.watch_auth_dir / pool.watch_auth_dir_interval）。
//
// 走 Load（= Default() → json.Unmarshal → normalize）而不是手工构造 Config：
//「键缺席时取什么值」正是本文件要测的东西，手工构造会绕过 Default()，
// 测出来的结论与真实启动路径无关。
//
// 与 proxy_scope_config_test.go 同一理由与同一写法。

import (
	"os"
	"path/filepath"
	"testing"
	"time"
)

// TestWatchAuthDirEnabledByDefault 键缺席时热加载**开启**。
//
// 为什么缺省必须是 true：这是**修缺陷**而不是加可选能力。账号池此前只在启动时
// 扫一次目录，新增账号必须手动重启网关才生效（用户实际报过的痛点）。
// 若缺省 false，所有既有用户升级后仍要手动重启 —— 等于缺陷没修。
//
// 它也不改变任何既有语义：热加载复用 SyncToDir/upsertLocked，
// 对已存在账号只换凭证、保留 credits/冷却/熔断/统计（有 pool 侧单测钉住）。
func TestWatchAuthDirEnabledByDefault(t *testing.T) {
	c := Default()
	if !c.Pool.WatchAuthDir {
		t.Error("pool.watch_auth_dir 缺省应为 true（否则新增账号仍需手动重启网关）")
	}
	if err := c.normalize(); err != nil {
		t.Fatalf("normalize: %v", err)
	}
	if c.WatchAuthDirIntervalD != 5*time.Second {
		t.Errorf("默认轮询周期=%v want 5s", c.WatchAuthDirIntervalD)
	}
}

// TestWatchAuthDirLegacyConfigKeepsEnabled 老配置（没有这两个键）加载后仍是启用态。
//
// 这条是「向后兼容」的落点：老 config 里根本没有 watch_auth_dir，
// 若加载路径把零值 false 当成用户意图，升级后热加载会静默失效。
func TestWatchAuthDirLegacyConfigKeepsEnabled(t *testing.T) {
	dir := t.TempDir()
	fp := filepath.Join(dir, "c.json")
	// 模拟老配置：只有既有的几个键。
	if err := os.WriteFile(fp, []byte(`{"listen":":7863","pool":{"rotation":false}}`), 0o600); err != nil {
		t.Fatal(err)
	}
	c, err := Load(fp)
	if err != nil {
		t.Fatal(err)
	}
	if !c.Pool.WatchAuthDir {
		t.Error("老配置缺该键时应保持启用（缺省 true）")
	}
	if c.WatchAuthDirIntervalD != 5*time.Second {
		t.Errorf("老配置下周期应回落 5s，实际 %v", c.WatchAuthDirIntervalD)
	}
}

// TestWatchAuthDirExplicitDisable 显式 false 才真正关掉（回滚点）。
func TestWatchAuthDirExplicitDisable(t *testing.T) {
	dir := t.TempDir()
	fp := filepath.Join(dir, "c.json")
	if err := os.WriteFile(fp, []byte(`{"pool":{"watch_auth_dir":false}}`), 0o600); err != nil {
		t.Fatal(err)
	}
	c, err := Load(fp)
	if err != nil {
		t.Fatal(err)
	}
	if c.Pool.WatchAuthDir {
		t.Error("显式 watch_auth_dir=false 应关掉热加载")
	}
}

// TestWatchAuthDirIntervalParsed 自定义周期被解析。
func TestWatchAuthDirIntervalParsed(t *testing.T) {
	dir := t.TempDir()
	fp := filepath.Join(dir, "c.json")
	if err := os.WriteFile(fp, []byte(`{"pool":{"watch_auth_dir_interval":"30s"}}`), 0o600); err != nil {
		t.Fatal(err)
	}
	c, err := Load(fp)
	if err != nil {
		t.Fatal(err)
	}
	if c.WatchAuthDirIntervalD != 30*time.Second {
		t.Errorf("周期=%v want 30s", c.WatchAuthDirIntervalD)
	}
}

// TestWatchAuthDirIntervalBadValueFallsBack 非法周期回落默认而**不报错**。
//
// 与 credit_refresh_interval 同一口径：调优键写错不该把网关拦停
//（写错周期与「签到任务配错小时」性质不同 —— 后者会做出无意义的上游请求，
// 前者最坏只是热加载慢一点）。但它也**不能**静默变成 0：0 周期会让
// time.NewTicker panic，那才是真正会把进程搞死的结果。
func TestWatchAuthDirIntervalBadValueFallsBack(t *testing.T) {
	for _, bad := range []string{"", "not-a-duration", "0s", "-5s"} {
		t.Run("bad="+bad, func(t *testing.T) {
			dir := t.TempDir()
			fp := filepath.Join(dir, "c.json")
			body := `{"pool":{"watch_auth_dir_interval":"` + bad + `"}}`
			if err := os.WriteFile(fp, []byte(body), 0o600); err != nil {
				t.Fatal(err)
			}
			c, err := Load(fp)
			if err != nil {
				t.Fatalf("非法周期不该拦住启动: %v", err)
			}
			if c.WatchAuthDirIntervalD <= 0 {
				t.Fatalf("周期必须为正（0 会让 NewTicker panic），实际 %v", c.WatchAuthDirIntervalD)
			}
			if c.WatchAuthDirIntervalD != 5*time.Second {
				t.Errorf("周期=%v want 回落 5s", c.WatchAuthDirIntervalD)
			}
		})
	}
}
