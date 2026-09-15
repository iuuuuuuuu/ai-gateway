// main.go workbuddy2api 入口：加载配置、构建 pool、起调度器与 HTTP 服务。
package main

import (
	"context"
	"flag"
	"log"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"strings"
	"syscall"
	"time"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/pool"
	"workbuddy2api/internal/redisstore"
	"workbuddy2api/internal/scheduler"
	"workbuddy2api/internal/server"
	"workbuddy2api/internal/session"
	"workbuddy2api/internal/upstream"
	"workbuddy2api/internal/usage"
)

func main() {
	cfgPath := flag.String("config", "config.json", "path to config json")
	flag.Parse()

	cfg, err := Load(*cfgPath)
	if err != nil {
		// 配置文件不存在时给一次机会用纯默认 + env
		if os.IsNotExist(err) {
			log.Printf("config %s not found, using defaults+env", *cfgPath)
			cfg, err = Load("")
		}
		if err != nil {
			log.Fatalf("load config: %v", err)
		}
	}

	auths, err := auth.LoadDir(cfg.AuthDir)
	if err != nil {
		log.Fatalf("load auths: %v", err)
	}
	log.Printf("loaded %d account(s) from %s", len(auths), cfg.AuthDir)

	// redisstore：未配置/连接失败 → Noop（纯内存模式，一切功能照常）。
	store := redisstore.New(cfg.Upstash.URL, cfg.Upstash.Token)

	p := pool.New(cfg.StateFile)
	defer p.Flush() // 进程退出前强制落盘（后台 flush 每 5s 一次，退出时补一次）
	p.SetStore(store)
	p.RestoreFromSnapshot() // 择新恢复：Redis 快照比本地新才采用，否则本地优先
	p.SyncToDir(auths)      // 与 auths 目录对齐：新账号加入、已删除文件账号剔除（状态保留）

	// Token 用量统计：与 state.json 同目录的 usage.json（独立文件，避免与池状态互相迁移）。
	usagePath := ""
	if cfg.StateFile != "" {
		usagePath = filepath.Join(filepath.Dir(cfg.StateFile), "usage.json")
	}
	usageStore := usage.New(usagePath)
	defer usageStore.Flush()

	// 熔断器 + 在途上限 + 三因子加权调优（从 config 注入，非正值回退默认）。
	p.SetBreaker(cfg.Pool.BreakerThreshold, cfg.BreakerCooldownDur, cfg.BreakerCooldownMaxD)
	p.SetMaxInFlight(cfg.Pool.MaxInFlight)
	p.SetWeights(cfg.Pool.IdleWeightPerHour, cfg.Pool.IdleWeightMax)

	// 「单一模型 + 积分轮转」模式（缺省关闭 = 负载均衡，老配置行为不变）。
	if cfg.Pool.Rotation {
		p.SetRotation(true)
		if m := strings.TrimSpace(cfg.Pool.AllowedModel); m != "" {
			log.Printf("pool: 已启用「单一模型 + 积分轮转」模式，锁定模型 %s（其他模型一律拒绝）", m)
		} else {
			// 未锁模型时轮转仍可用，但语义不完整：客户端可换模型绕过额度控制。
			log.Printf("pool: 已启用「单一模型 + 积分轮转」模式，但未指定模型（pool.allowed_model 为空）—— 建议在界面选择模型")
		}
	}

	// 会话粘性路由（可配关闭）。
	var sessRouter *session.Router
	redisMode := "noop"
	if _, ok := store.(redisstore.Noop); !ok {
		redisMode = "upstash"
	}
	if cfg.SessionSticky.Enabled {
		sessRouter = session.New(session.Config{
			TTL:        cfg.SessionTTL,
			GCInterval: cfg.SessionGCInterval,
			Store:      store,
			Available:  p.AvailableUIDs,
		})
		sessRouter.LoadFromStore() // 启动时从 Redis 恢复粘性（读操作仅此处）
		sessRouter.StartGC()
		defer sessRouter.StopGC()
	}
	sessCount := func() int {
		if sessRouter != nil {
			return sessRouter.Count()
		}
		return 0
	}

	up := upstream.New()
	// 出站代理：必须在 New() 之后、其它 transport 调优之前设置 ——
	// SetProxy 会重建 Transport，之后的调优（ResponseHeaderTimeout）才作用在新实例上。
	//
	// 为什么需要：国际版（workbuddy.ai）在国内直连不稳定（实测 wsarecv 超时），
	// 走代理才稳。宿主把「设置 → 更新代理」里已填的地址复用到此处，用户无需配两遍。
	// 地址无效不致命：记日志并继续直连，避免一个配置项导致网关起不来。
	if proxy := strings.TrimSpace(cfg.Proxy); proxy != "" {
		if err := up.SetProxy(proxy); err != nil {
			log.Printf("proxy: 配置无效，忽略并直连：%v", err)
		} else {
			log.Printf("proxy: 出站请求经 %s", proxy)
		}
	} else {
		log.Printf("proxy: 未配置（国际版账号在部分网络下可能超时，可在软件的「设置 → 更新代理」中填写）")
	}
	// 短 RPC 总时长上限（refresh/checkin/balance/FetchModels），语义不变。
	up.HTTP.Timeout = time.Duration(cfg.Upstream.TimeoutSeconds) * time.Second
	// 聊天 SSE 首字节前（响应头）上限：cfg 已 normalize（缺省回落 timeout_seconds）。
	up.HeaderTimeout = time.Duration(cfg.Upstream.HeaderTimeoutSeconds) * time.Second
	if tr, ok := up.ChatHTTP.Transport.(*http.Transport); ok {
		tr.ResponseHeaderTimeout = up.HeaderTimeout
	}
	// 聊天 SSE 流中空闲上限（S3 空闲监控读取）。
	up.IdleTimeout = time.Duration(cfg.Upstream.IdleTimeoutSeconds) * time.Second
	up.SanitizeFingerprints = cfg.Features.SanitizeBlacklistFingerprints

	sch := scheduler.New(scheduler.Config{
		Pool:                p,
		Upstream:            up,
		CheckinHours:        cfg.Schedule.CheckinHours,
		KeepaliveHours:      cfg.Schedule.KeepaliveHours,
		ActivityHours:       cfg.Schedule.ActivityHours,
		NightOwlHours:       cfg.Schedule.NightOwlHours,
		SchoolHours:         cfg.Schedule.SchoolHours,
		TrialHours:          cfg.Schedule.TrialHours,
		CheckinDisabled:     !cfg.Schedule.CheckinEnabled,
		KeepaliveDisabled:   !cfg.Schedule.KeepaliveEnabled,
		ActivityDisabled:    !cfg.Schedule.ActivityEnabled,
		NightOwlDisabled:    !cfg.Schedule.NightOwlEnabled,
		SchoolDisabled:      !cfg.Schedule.SchoolEnabled,
		TrialDisabled:       !cfg.Schedule.TrialEnabled,
		ActivityReportCount: cfg.Schedule.ActivityReportCount,
		CheckinScope:        cfg.Schedule.CheckinScope,
	})
	if normalizeCheckinScope(cfg.Schedule.CheckinScope) == "all" {
		log.Printf("签到与猫猫旅行范围：国服 + 国际版（schedule.checkin_scope=all）")
	} else {
		log.Printf("签到与猫猫旅行范围：仅国服（schedule.checkin_scope=cn，国际版账号自动跳过）")
	}
	switch {
	case !cfg.Schedule.CheckinEnabled:
		log.Printf("签到已禁用（schedule.checkin_enabled=false）：猫猫旅行同时停摆（搭签到便车）")
	case len(cfg.Schedule.CheckinHours) == 0:
		log.Printf("猫猫旅行已合并到签到时点执行：签到 + 派猫 + 领取旅行奖励")
	default:
		log.Printf("猫猫旅行已合并到签到时点执行：签到 + 派猫 + 领取旅行奖励（%v 点）", cfg.Schedule.CheckinHours)
	}
	if !cfg.Schedule.KeepaliveEnabled {
		log.Printf("token 保活已禁用（schedule.keepalive_enabled=false）")
	}
	if cfg.Schedule.ActivityEnabled {
		log.Printf("活跃上报已启用（%v 点，每号 %d 条）：点亮连登天数并解锁领养前置",
			cfg.Schedule.ActivityHours, cfg.Schedule.ActivityReportCount)
	} else {
		log.Printf("活跃上报已禁用（schedule.activity_enabled=false）：连登天数将不再增长")
	}
	if cfg.Schedule.NightOwlEnabled {
		log.Printf("夜猫子任务已启用（%v 点）：夜猫窗口 23:00-08:00 CST 内补一次任务",
			cfg.Schedule.NightOwlHours)
	} else {
		log.Printf("夜猫子任务已禁用（schedule.nightowl_enabled=false）")
	}
	if cfg.Schedule.SchoolEnabled {
		log.Printf("开学季活动任务已启用（%v 点）：只领取已达标的奖励", cfg.Schedule.SchoolHours)
	} else {
		log.Printf("开学季活动任务已禁用（schedule.school_enabled=false）")
	}
	if cfg.Schedule.TrialEnabled {
		log.Printf("trial 加油包领取已启用（%v 点，仅国际版）：已领过的账号幂等跳过", cfg.Schedule.TrialHours)
	} else {
		log.Printf("trial 加油包领取已禁用（schedule.trial_enabled=false）")
	}

	h := server.NewHandler(server.Config{
		Pool:         p,
		Upstream:     up,
		APIKey:       cfg.APIKey,
		Session:      sessRouter,
		StickyCount:  sessCount,
		RedisMode:    redisMode,
		SoftCooldown: cfg.SoftRateDur,
		Usage:        usageStore,
		// 单一模型锁定：仅轮转模式下生效（负载均衡不限制模型，保持原有行为）。
		AllowedModel: func() string {
			if cfg.Pool.Rotation {
				return strings.TrimSpace(cfg.Pool.AllowedModel)
			}
			return ""
		}(),
	})

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	go sch.Run(ctx)
	// 积分到期巡检：独立于签到的高频刷新，驱动账号池「先烧快过期额度」的分层选号。
	if cfg.Pool.CreditRefreshEnabled {
		go sch.RunCreditRefreshLoop(ctx, cfg.CreditRefreshIntervalD)
		log.Printf("积分到期巡检已启用：每 %s 刷新一次（驱动到期分层选号）", cfg.CreditRefreshIntervalD)
	} else {
		log.Printf("积分到期巡检已禁用（pool.credit_refresh_enabled=false）：到期分层仅依赖签到与宿主同步")
	}

	srv := &http.Server{
		Addr:              cfg.Listen,
		Handler:           h,
		ReadHeaderTimeout: 30 * time.Second,
	}
	go func() {
		<-ctx.Done()
		p.Flush()         // 信号触发：先落盘再做优雅停机
		usageStore.Flush() // Token 用量同样在退出前补一次落盘
		shutdownCtx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		_ = srv.Shutdown(shutdownCtx)
	}()

	log.Printf("workbuddy2api listening on %s (api_key=%v)", cfg.Listen, cfg.APIKey != "")
	if err := srv.ListenAndServe(); err != nil && err != http.ErrServerClosed {
		log.Fatalf("http: %v", err)
	}
	log.Printf("bye")
}
