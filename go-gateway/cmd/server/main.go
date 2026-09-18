// main.go workbuddy2api 入口：加载配置、构建 pool、起调度器与 HTTP 服务。
package main

import (
	"context"
	"flag"
	"fmt"
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
	"workbuddy2api/internal/qoder"
	"workbuddy2api/internal/records"
	"workbuddy2api/internal/redisstore"
	"workbuddy2api/internal/scheduler"
	"workbuddy2api/internal/server"
	"workbuddy2api/internal/session"
	"workbuddy2api/internal/upstream"
	"workbuddy2api/internal/usage"
	"workbuddy2api/internal/zcode"
)

// qoderAuthOf 把 Qoder 凭证转成账号池用的 auth.Auth。
//
// 为什么两个结构不合并：Qoder 需要机器指纹（COSY 签名的必需输入）与
// dt/drt 的明确语义，而 WorkBuddy 不需要。合并会让 WorkBuddy 侧也长出
// 用不到的字段，且两者的令牌刷新逻辑完全不同（一个 OAuth refresh，
// 一个 deviceToken/refresh + drt 轮换）。
//
// 桥接点收在这里：池只认 auth.Auth（选号逻辑要跨产品共用）。
func qoderAuthOf(cr *qoder.Cred) *auth.Auth {
	return &auth.Auth{
		UID:          cr.UID,
		Nickname:     cr.Nickname,
		AccessToken:  cr.DT,
		RefreshToken: cr.DRT,
		ExpiresAt:    cr.DTExpiresAt,
		// 域名用于区域判定（qoder.sh = 国际版，qoder.com.cn = 国服），
		// 与 Qoder 自己的 RegionFromDomain 口径一致。
		Domain:   cr.Region.Domain(),
		FilePath: cr.FilePath,
		Product:  auth.ProductQoder,
	}
}

// zcodeAuthOf 把 ZCode 凭证转成账号池用的 auth.Auth。
//
// ## 字段映射的取舍
//
// ZCode 的凭证就是一个字符串（`{apiKey}.{secret}`），故复用 auth.Auth 的
// **AccessToken** 字段承载它（而不是新增一个只对 ZCode 有意义的字段）。
//
// **服务商**（Z.AI / 智谱）通过 Domain 承载 —— auth.Auth 已有这个字段
//（WorkBuddy 用它判区域），复用它避免给池加字段。下游用
// `zcode.ProviderOfDomain` 反解。
//
// ⚠ ZCode **没有令牌刷新**（凭证长期有效），故 RefreshToken / ExpiresAt 留空。
// 池的"临近过期先刷新"逻辑对它不生效 —— 这是正确的（没有可刷新的东西）。
func zcodeAuthOf(cr *zcode.Cred) *auth.Auth {
	return &auth.Auth{
		UID:      cr.UID,
		Nickname: cr.Nickname,
		// 凭证字符串复用 AccessToken 字段
		AccessToken: cr.Credential,
		// 服务商通过 Domain 承载（下游用 ProviderOfDomain 反解）
		Domain:   zcode.DomainOfProvider(cr.Provider),
		FilePath: cr.FilePath,
		Product:  auth.ProductZcode,
		// JWT 不放进 auth.Auth：它只用于额度查询（不在选号/转发热路径上），
		// 而池也不该关心它。额度查询由宿主侧直接读凭证文件完成。
	}
}

// maskUID 脱敏 uid（日志里不出现完整账号标识）。
//
// 本仓库是公开仓库，日志可能被用户贴到 issue 里 —— 只留前 8 位足够定位，
// 又不足以反查账号。
func maskUID(uid string) string {
	if len(uid) <= 8 {
		return uid
	}
	return uid[:8] + "…"
}

func main() {
	// 子命令：宿主（Tauri）通过 `gateway qoder-login ...` 完成 Qoder 的设备流登录。
	//
	// 为什么走子命令而不是 HTTP 接口：登录属于**配置阶段**的事，
	// 而网关可能因为配置不完整还没启动 —— 子命令无此依赖，更可靠。
	// 放在 flag.Parse 之前拦截，避免与网关自身的 -config 等参数冲突。
	if len(os.Args) > 1 && os.Args[1] == "qoder-login" {
		// 凭证目录的默认值与网关配置一致（~/.wb-switch/qoder/auths），
		// 这样宿主不传 --auth-dir 时也能落到正确位置。
		defaultAuthDir := qoder.DefaultAuthDir()
		os.Exit(qoder.RunLoginCLI(os.Args[2:], defaultAuthDir))
	}
	// ZCode 的登录子命令（同上的理由）。
	//
	// 与 Qoder 的差异：ZCode 的凭证是**用户可复制的字符串**，
	// 故 `zcode-login import` 是主路径，OAuth 是备选。
	if len(os.Args) > 1 && os.Args[1] == "zcode-login" {
		defaultAuthDir := zcode.DefaultAuthDir()
		os.Exit(zcode.RunLoginCLI(os.Args[2:], defaultAuthDir))
	}

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
	}

	// 「限制使用的模型」白名单：**三个工作模式都生效**（不再只在轮转下）。
	//
	// 这条日志是用户排查「为什么客户端被拒」的第一现场：网关子进程的 stdout
	// 在 GUI 里可能被丢弃，因此把**生效的完整名单**打出来，用户从日志就能看出
	// 自己配的是哪几个，而不是只能看到「被拒了」。
	if list := cfg.Pool.AllowedModels; len(list) > 0 {
		log.Printf("pool: 已限制可使用的模型，只放行 %s（其他模型一律拒绝；改配置后需重启网关）",
			strings.Join(list, "、"))
	} else if cfg.Pool.Rotation {
		// 轮转但未限制模型：语义不完整（客户端可换模型绕过额度控制），
		// 但这是合法配置，只提示不拦。
		log.Printf("pool: 已启用「积分轮转」模式，但未限制模型（pool.allowed_model 为空）—— 建议在界面「放行模型」里选择")
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
	//
	// 适用范围**由 proxy_scope 的两个开关决定**（设置页里「国内版 / 国际版」两格）：
	//   开 → 该区域走上面的显式代理；关 → 该区域**真直连**（连环境变量代理也不用）。
	// 缺省（老配置没有 proxy_scope 键）= 国际版开、国服关，即本次改动前的行为。
	// 因此在 config.go 的 Default() 里把这两个值写死，键缺席时不会翻转既有行为。
	//
	// 顺序**必须**是 SetProxy → SetProxyScope：后者不解析地址（避免
	// 「host:port 自动补 http://」这类容错在两处各写一份而分叉），只按开关布置。
	if proxy := strings.TrimSpace(cfg.Proxy); proxy != "" {
		if err := up.SetProxy(proxy); err != nil {
			log.Printf("proxy: 配置无效，忽略并直连：%v", err)
		} else if err := up.SetProxyScope(cfg.ProxyScope.CN, cfg.ProxyScope.Intl); err != nil {
			// 走到这里说明地址在 SetProxy 通过、在这里却失败（不应发生）；
			// 记日志并保留 SetProxy 的结果，不让一个开关把网关拦停。
			log.Printf("proxy: 适用范围设置失败，按默认分流（国际版走代理、国服直连）：%v", err)
		} else {
			// 日志必须**如实**写明两个区域各自的走向：只说「出站请求经 X」会让
			// 用户以为国服也在绕道，从而误判国服变慢的原因（反之亦然）。
			log.Printf("proxy: %s", describeProxyScope(proxy, cfg.ProxyScope.CN, cfg.ProxyScope.Intl))
		}
	} else {
		log.Printf("proxy: 未配置（国际版账号在部分网络下可能超时，可在软件的「设置 → 更新代理」中填写）")
	}
	// 短 RPC 总时长上限（refresh/checkin/balance/FetchModels），语义不变。
	// 两套 client（国服 / 国际版）都要设：只设 c.HTTP 会让国际版的
	// 短 RPC 悄悄退回 120s 硬编码上限，与配置不符且无法从界面上看出来。
	rpcTimeout := time.Duration(cfg.Upstream.TimeoutSeconds) * time.Second
	up.HTTP.Timeout = rpcTimeout
	up.SetRPCTimeout(rpcTimeout)
	// 聊天 SSE 首字节前（响应头）上限：cfg 已 normalize（缺省回落 timeout_seconds）。
	up.HeaderTimeout = time.Duration(cfg.Upstream.HeaderTimeoutSeconds) * time.Second
	// 遍历**所有** transport（含国际版代理那一个）——只改 ChatHTTP.Transport
	// 会漏掉国际版，表现为「国服按新上限超时、国际版仍干等 120s」。
	up.ApplyResponseHeaderTimeout(up.HeaderTimeout)
	// 聊天 SSE 流中空闲上限（S3 空闲监控读取）。
	up.IdleTimeout = time.Duration(cfg.Upstream.IdleTimeoutSeconds) * time.Second
	up.SanitizeFingerprints = cfg.Features.SanitizeBlacklistFingerprints

	// 账号记录回写：这 4 个养号任务的日志只写 stdout，而宿主启动子进程时
	// 把 stdout/stderr 丢进了 Stdio::null —— 界面上一条执行痕迹都没有。
	// 改为写宿主已经在读的 account_records.json，记录就能与签到并列显示。
	// 路径与账号身份均由宿主经配置透传（见 config.AccountRecords）。
	recorder := records.New(
		cfg.AccountRecords.File,
		cfg.AccountRecords.RetentionDays,
		cfg.RecordIdentities(),
	)
	if recorder.Enabled() {
		log.Printf("账号记录回写已启用：%s（保留 %d 天）",
			recorder.Path(), cfg.AccountRecords.RetentionDays)
	} else {
		log.Printf("账号记录回写未启用（配置缺少 account_records.file）：任务照跑，但界面不会有记录")
	}

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
		Records:             recorder,
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

	// ---- 多产品路由（默认关闭）----
	//
	// 开启后：加载其它产品的账号进同一个池，并注入它们的请求派发实现。
	// 关闭时两者都不做 —— 行为与单产品时代逐字相同（回滚点）。
	var qoderDispatch server.ProductUpstream
	var zcodeDispatch server.ProductUpstream
	if cfg.Pool.MultiProduct {
		// ---- Qoder ----
		qoderDir := cfg.Pool.QoderAuthDir
		if qoderDir == "" {
			qoderDir = qoder.DefaultAuthDir()
		}
		creds, failed, err := qoder.LoadDir(qoderDir)
		if err != nil {
			log.Printf("多产品路由已开启，但读取 Qoder 凭证目录失败（%s）：%v", qoderDir, err)
		}
		// 解析失败的文件必须**报出来**：用户会以为"账号导入了但没生效"，
		// 而真正原因是格式不对（Qoder 凭证可能是手写的）。
		for _, f := range failed {
			log.Printf("Qoder 凭证解析失败，已跳过：%s", f)
		}
		added := 0
		for _, cr := range creds {
			if cr.UID == "" {
				continue
			}
			if cr.EnsureFingerprint() {
				// 指纹是 COSY 签名的必需输入，且必须跨重启稳定 —— 生成后落盘。
				if err := cr.SaveAtomic(); err != nil {
					log.Printf("Qoder 账号 %s 保存机器指纹失败：%v", maskUID(cr.UID), err)
				}
			}
			p.Add(qoderAuthOf(cr))
			added++
		}
		log.Printf("多产品路由已开启：Qoder 凭证目录 %s，载入 %d 个账号（解析失败 %d 个）",
			qoderDir, added, len(failed))

		qc := qoder.New()
		// Qoder 的网关对 HTTP/2 不友好，New() 里已禁用 h2（见 qoder.New 的注释）。
		qoderDispatch = qoder.NewDispatch(qc)

		// ---- ZCode ----
		zcodeDir := cfg.Pool.ZcodeAuthDir
		if zcodeDir == "" {
			zcodeDir = zcode.DefaultAuthDir()
		}
		zcreds, zfailed, zerr := zcode.LoadDir(zcodeDir)
		if zerr != nil {
			log.Printf("多产品路由已开启，但读取 ZCode 凭证目录失败（%s）：%v", zcodeDir, zerr)
		}
		for _, f := range zfailed {
			log.Printf("ZCode 凭证解析失败，已跳过：%s", f)
		}
		zadded := 0
		for _, cr := range zcreds {
			if cr.UID == "" {
				continue
			}
			p.Add(zcodeAuthOf(cr))
			zadded++
		}
		log.Printf("多产品路由已开启：ZCode 凭证目录 %s，载入 %d 个账号（解析失败 %d 个）",
			zcodeDir, zadded, len(zfailed))
		zcodeDispatch = zcode.NewDispatch(zcode.New())

		// 成本维度：让各产品按"单位额度消耗率"参与加权（见 design.md §2.3）。
		p.SetMultiProduct(true, 0.3)
	} else {
		log.Printf("多产品路由已关闭（pool.multi_product=false）：只使用 WorkBuddy 账号")
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
		// 养号任务手动触发：宿主（GUI/webui）的「立即执行」按钮经此转到调度器。
		// 传方法值而非 *Scheduler —— server 包只需这一个能力，不必知道调度器结构。
		// 第二参数为账号 uid：空 = 全部账号（右上角一键操作），非空 = 仅该账号
		//（账号卡片菜单里的入口）。
		RunTaskFor: sch.RunTaskFor,
		// 成长任务「一键完成」：17 个可自动任务的列表 / 单账号执行 / 全账号执行。
		//
		// 必须有这个入口，否则 growtask 包会被链接器的死代码消除剔出二进制
		// —— 表现为「代码写了、测试也过了，但运行时根本调不到」。
		// 实测验证方式：`strings gateway.exe | findstr growth/tasks` 应有命中。
		GrowthTasks: newGrowthTaskAPI(p, up, recorder),
		// 「限制使用的模型」白名单：三个工作模式都生效，空 = 不限制（默认）。
		//
		// 直接把已解析的切片传下去（不再按 rotation 过滤）：限制模型与「用哪些
		// 账号」是正交的两件事，只在轮转下生效会让自动/手动模式完全无法限制模型。
		// 老配置的 `allowed_model` 字符串由 AllowedModels.UnmarshalJSON 读成
		// 单元素切片，因此老配置升级后行为逐字不变。
		AllowedModels: cfg.Pool.AllowedModels,
		// 系统提示词替换：mode 缺省 passthrough（透传客户端原始 system），
		// custom 时用 PromptText（normalizePrompt 已读完盘并缓存）替换
		// 客户端的 system/developer 消息。
		PromptMode: cfg.Prompt.Mode,
		PromptText: cfg.PromptText,
		// Qoder 产品派发：仅在多产品路由开启时注入。
		//
		// 关闭时（默认）此字段为 nil → dispatchUpstream 对所有账号返回
		// "非该产品" → 全部走既有 WorkBuddy 路径，行为与单产品时代逐字相同。
		// 这是 design.md §5 要求的回滚点：产品维度出问题就关掉开关，
		// 立刻回到已验证的行为，不需要回滚代码。
		Qoder: qoderDispatch,
		// ZCode 产品派发（同上）。
		Zcode: zcodeDispatch,
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

// describeProxyScope 拼一行**如实**的代理适用范围日志。
//
// 为什么值得单独一个函数并配单测：这行日志是用户排查「某一路为什么走了/没走
// 代理」的第一现场（网关子进程的 stdout 在 GUI 里可能被丢弃，用户能看到的
// 往往只有这里）。写错方向的代价是把他引到完全错误的排查路径上 ——
// 例如国服其实直连、日志却说「经代理」，他会去查代理为什么慢。
//
// 三种状态都要能读出来：走显式代理 / 真直连（连环境变量也不用）/ 只跟环境变量。
func describeProxyScope(addr string, cn, intl bool) string {
	// 措辞刻意区分「显式代理」与「环境变量代理」：两者都可能让流量绕道，
	// 但只有前者是用户在设置页里填的，混为一谈就没法解释现象。
	state := func(enabled bool) string {
		if enabled {
			return "经 " + addr
		}
		return "真直连（连 HTTPS_PROXY 等环境变量代理也不用）"
	}
	return fmt.Sprintf("国服账号（*.cn / copilot.tencent.com）%s；国际版账号（*.ai）%s",
		state(cn), state(intl))
}
