package zcode

// external_solver_test.go —— 钉住「外部求解（宿主 WebView2）的接线」。
//
// # 为什么需要它（2026-09-21 实测踩到的缺陷）
//
// 所有者提出的方案：宿主自带 WebView2（Win10/11 预装，零体积），在**真实
// 浏览器环境**里跑阿里云官方 SDK —— 与官方 ZCode 客户端同一做法。
// 实测 929ms 拿到 param（本地 Node + happy-dom 模拟环境要约 3 秒，成功率仅 40%）。
//
// 接线过程中踩到一个**典型缺陷**：`UnavailableReason()` 只看本地组件
//（目录/入口文件/node），不知道外部求解的存在。于是配置好外部服务后：
//
//	client.go::solveCaptcha
//	  → UnavailableReason() 非空 → 直接失败   ← 走到这里就返回了
//	  → （永远到不了的）Solve() → solveViaExternal()  ← 外面那条路才是好的
//
// 实测症状：`求解器不可用：求解器组件未安装（发行包应包含 assets/zcode-captcha）`
// 而外部服务端日志显示**它根本没被调用过**。
//
// 本文件把「可用性判断必须把外部求解算进来」以及「外部优先、本地回退」
// 两条契约钉死。

import (
	"context"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

// newSolverWithExternal 造一个只配了外部求解的 solver（无本地目录）。
func newSolverWithExternal(t *testing.T, h http.HandlerFunc) (*CaptchaSolver, *httptest.Server) {
	t.Helper()
	srv := httptest.NewServer(h)
	t.Cleanup(srv.Close)
	s := &CaptchaSolver{}
	s.SetExternalSolver(srv.URL+"/solve", "test-token")
	return s, srv
}

// TestExternalSolverMakesAvailable 配了外部求解 ⇒ 即便本地组件缺失也算可用。
//
// 这是上面那个缺陷的直接回归测试：旧实现下本用例会红（返回"组件未安装"），
// 而调用方据此**根本不会走到 Solve**。
func TestExternalSolverMakesAvailable(t *testing.T) {
	s, _ := newSolverWithExternal(t, func(w http.ResponseWriter, r *http.Request) {
		_, _ = w.Write([]byte(`{"param":"p","region":"cn"}`))
	})

	if got := s.UnavailableReason(); got != "" {
		t.Errorf("配了外部求解时不该报不可用，实际：%s", got)
	}
	// Available 与 UnavailableReason 必须同口径 —— 否则调用方各取一个就会分叉。
	if !s.Available() {
		t.Error("Available 与 UnavailableReason 口径不一致（前者说不可用）")
	}
}

// TestExternalSolverPreferred 外部求解优先于本地。
//
// 外部是**真实浏览器环境**，成功率与速度都优于本地模拟环境，
// 故配置了就必须先用它。
func TestExternalSolverPreferred(t *testing.T) {
	var called atomic.Int32
	s, _ := newSolverWithExternal(t, func(w http.ResponseWriter, r *http.Request) {
		called.Add(1)
		_, _ = w.Write([]byte(`{"param":"FROM_EXTERNAL","region":"cn"}`))
	})
	// 也给一个本地目录（内容不存在，故意让它失败）——
	// 以此证明「外部成功时不会去碰本地」。
	s.SetDir(t.TempDir())

	got, err := s.Solve(context.Background())
	if err != nil {
		t.Fatalf("应成功（外部可用），实际：%v", err)
	}
	if got != "FROM_EXTERNAL" {
		t.Errorf("应取外部返回值，实际 %q", got)
	}
	if called.Load() == 0 {
		t.Error("外部服务未被调用 —— 说明没有优先走外部")
	}
}

// TestExternalSolverSendsToken 请求必须带共享令牌。
//
// 端口在同机上人人可扫，而每次求解都会向上游发请求。没有令牌校验就可能
// 被同机其它进程当免费求解器刷。
func TestExternalSolverSendsToken(t *testing.T) {
	var gotToken string
	s, _ := newSolverWithExternal(t, func(w http.ResponseWriter, r *http.Request) {
		gotToken = r.Header.Get("X-Captcha-Token")
		_, _ = w.Write([]byte(`{"param":"p","region":"cn"}`))
	})

	if _, err := s.Solve(context.Background()); err != nil {
		t.Fatalf("应成功：%v", err)
	}
	if gotToken != "test-token" {
		t.Errorf("应带令牌 test-token，实际 %q", gotToken)
	}
}

// TestExternalSolverSendsSceneParams 请求要带上 scene/region/prefix。
//
// 这三个是上游的运营参数（可能变）。显式传避免两边默认值漂移 ——
// 上游改了参数时只改一处。
func TestExternalSolverSendsSceneParams(t *testing.T) {
	var q string
	s, _ := newSolverWithExternal(t, func(w http.ResponseWriter, r *http.Request) {
		q = r.URL.RawQuery
		_, _ = w.Write([]byte(`{"param":"p","region":"cn"}`))
	})
	s.SetCaptchaConfig("SCENE_X", "REGION_Y", "PREFIX_Z")

	if _, err := s.Solve(context.Background()); err != nil {
		t.Fatalf("应成功：%v", err)
	}
	for _, want := range []string{"scene=SCENE_X", "region=REGION_Y", "prefix=PREFIX_Z"} {
		if !strings.Contains(q, want) {
			t.Errorf("查询串应含 %s，实际 %q", want, q)
		}
	}
}

// TestExternalSolverFailureIsReported 外部返回错误时如实上报。
func TestExternalSolverFailureIsReported(t *testing.T) {
	s, _ := newSolverWithExternal(t, func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusBadGateway)
		_, _ = w.Write([]byte(`{"error":"SDK 起不来"}`))
	})

	_, err := s.Solve(context.Background())
	if err == nil {
		t.Fatal("外部失败时应返回错误")
	}
	// 错误信息要能让用户看出**是外部求解失败**（而不是"组件没装"）——
	// 两者的处置完全不同。
	if !strings.Contains(err.Error(), "宿主") && !strings.Contains(err.Error(), "外部") {
		t.Errorf("错误信息应点明是外部求解的问题，实际：%v", err)
	}
}

// TestExternalSolverFallsBackToLocal 外部失败 ⇒ 回退本地（而不是直接放弃）。
//
// 外部服务可能因为宿主还没起、端口被占、用户关了主窗口等原因不可用，
// 那种情况下本地求解仍有机会成功 —— 没有理由让整个功能失效。
//
// 本用例造一个「外部必失败 + 本地目录存在但求解器是假脚本」的场景，
// 断言**本地确实被尝试过**（通过错误信息区分：不再是外部那条错误）。
func TestExternalSolverFallsBackToLocal(t *testing.T) {
	s, _ := newSolverWithExternal(t, func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusInternalServerError)
		_, _ = w.Write([]byte(`{"error":"宿主未就绪"}`))
	})
	// 本地目录为空 ⇒ entryFile 会报"既没有 X 也没有 Y"，能据此判断走到了本地
	s.SetDir(t.TempDir())

	_, err := s.Solve(context.Background())
	if err == nil {
		t.Fatal("两边都不可用时应失败")
	}
	// 错误信息应来自**本地**那条（说明尝试过回退），而不是直接返回外部错误
	if strings.Contains(err.Error(), "宿主未就绪") {
		t.Errorf("外部失败后应继续尝试本地，实际直接返回了外部错误：%v", err)
	}
}

// TestExternalSolverNotConfiguredKeepsOldBehavior 未配置时行为与改动前一致。
//
// 这是**回滚点**：没有外部服务（老版本宿主、或用户手动起的网关）时，
// 一切照旧走本地 Node 求解。
func TestExternalSolverNotConfiguredKeepsOldBehavior(t *testing.T) {
	s := &CaptchaSolver{}
	reason := s.UnavailableReason()
	if reason == "" {
		t.Fatal("未配外部且无本地组件时，应报不可用")
	}
	if !strings.Contains(reason, "组件未安装") {
		t.Errorf("错误措辞应与改动前一致（组件未安装），实际：%s", reason)
	}
}

// TestExternalSolverTimeoutBounded 外部服务挂起时不会无限等。
//
// 求解在**请求路径**上：若外部服务卡住而不设超时，用户的对话请求会一直挂着。
func TestExternalSolverTimeoutBounded(t *testing.T) {
	if testing.Short() {
		t.Skip("跳过：本用例需要等超时")
	}
	s, _ := newSolverWithExternal(t, func(w http.ResponseWriter, r *http.Request) {
		// 挂住不返回，直到测试结束（模拟"宿主无响应"）
		<-r.Context().Done()
	})

	done := make(chan struct{})
	go func() {
		_, _ = s.Solve(context.Background())
		close(done)
	}()

	select {
	case <-done:
		// 正常：在合理时间内返回（外部超时 20s，之后回退本地、本地又失败）
	case <-time.After(60 * time.Second):
		t.Fatal("外部服务挂起时 Solve 未在 60 秒内返回 —— 缺少超时保护")
	}
}
