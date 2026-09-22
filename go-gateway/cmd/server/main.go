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
		// 用户手动禁用（宿主账号页的「停止接流量」开关，写在凭证文件里）。
		//
		// ⚠ 必须传下去：池的 `healthy()` 会因它排除该账号。漏掉的话，
		// 界面上的开关**完全不生效** —— 网关照常把请求路由到它
		//（ZCode 侧实测确认过同样的缺陷）。
		NoRoute: cr.NoRoute,
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
		// 用户手动禁用（宿主账号页的「停止接流量」开关，写在凭证文件里）。
		//
		// ⚠ 必须传下去：池的 `healthy()` 会因它排除该账号。漏掉的话，
		// 界面上的开关**完全不生效** —— 网关照常把请求路由到它。
		NoRoute: cr.NoRoute,
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
	// 「模型 → 允许的平台」白名单（所有者的需求：平台区分使用哪个平台的模型）。
	//
	// 空 = 不限制，行为与加该功能之前逐字相同 —— 这是回滚点。
	p.SetModelPlatforms(cfg.Pool.ModelPlatforms)
	if n := len(cfg.Pool.ModelPlatforms); n > 0 {
		log.Printf("模型平台白名单已启用：%d 个模型被限定平台", n)
	}
	// ZCode 验证码求解器（见 config.Pool.ZcodeCaptchaDir 的说明）。
	//
	// # 2026-09-20：改为**始终启用**（所有者决定，不再读开关）
	//
	// 原先这里判 `cfg.Pool.ZcodeCaptchaEnabled`，默认 false。两个问题：
	//
	//	① 那个"界面开关"**从来不存在**（前端与宿主搜 captcha 均 0 命中）
	//	② 实测 ZCode 对话通道对不带验证码的请求一律回
	//	   `HTTP 400 {"code":3007,"msg":"captcha verify failed"}`
	//
	// ⇒ "默认关闭"等于 **ZCode 对话从未成功过**。
	//
	// 所有者原话：「肯定要默认打开并且不能关闭啊，这是开源软件有什么在乎的？」
	//
	// ⚠ **刻意不读 `ZcodeCaptchaEnabled`**：老配置里可能留着 false，
	// 若据此关闭，升级后老用户仍然"功能永远关着" —— 那正是本次要修的状态。
	// 该字段现在只作为向后兼容的占位（网关侧仍会写 true）。
	if cfg.Pool.ZcodeCaptchaDir != "" {
		cs := zcode.SharedCaptchaSolver()
		cs.SetDir(cfg.Pool.ZcodeCaptchaDir)
		// 外部求解（宿主 WebView2）优先：真实浏览器环境，比模拟环境更快更稳。
		//
		// 见 ZcodeCaptchaSolverURL 的注释（含实测数据：929ms 拿到 param）。
		// 这里只做"有就配上"：没有配置时行为与改动前逐字相同（纯本地 Node）。
		if cfg.Pool.ZcodeCaptchaSolverURL != "" {
			cs.SetExternalSolver(cfg.Pool.ZcodeCaptchaSolverURL, cfg.Pool.ZcodeCaptchaSolverToken)
			log.Printf("ZCode 验证码求解：**外部服务优先**（宿主 WebView2）%s，本地求解作为回退",
				cfg.Pool.ZcodeCaptchaSolverURL)
		}
		if reason := cs.UnavailableReason(); reason != "" {
			// 组件不全 → **如实报**，而不是静默失效
			//（那会让用户看到 3007 却不知道为什么）
			log.Printf("⚠ ZCode 验证码求解已启用，但组件不可用：%s", reason)
		} else {
			log.Printf("ZCode 验证码求解已启用：%s", cfg.Pool.ZcodeCaptchaDir)
		}
	} else if cfg.Pool.ZcodeCaptchaSolverURL != "" {
		// 只有外部求解（没释放本地求解器）—— 这也是合法配置。
		cs := zcode.SharedCaptchaSolver()
		cs.SetExternalSolver(cfg.Pool.ZcodeCaptchaSolverURL, cfg.Pool.ZcodeCaptchaSolverToken)
		log.Printf("ZCode 验证码求解：仅外部服务（宿主 WebView2）%s",
			cfg.Pool.ZcodeCaptchaSolverURL)
	} else {
		// 目录为空 = 求解器没释放出来。这是**发行包缺陷**，必须显眼。
		log.Printf("⚠ ZCode 验证码求解器未配置（zcode_captcha_dir 为空）：" +
			"ZCode 对话会因 3007 人机验证而失败；发行包应包含 assets/zcode-captcha")
	}
	p.RestoreFromSnapshot() // 择新恢复：Redis 快照比本地新才采用，否则本地优先
	p.SyncToDir(auths)      // 与 auths 目录对齐：新账号加入、已删除文件账号剔除（状态保留）

	// 凭证目录热加载：运行中新增的凭证文件自动进池，免去「加完账号手动重启网关」。
	//
	// 位置**必须在 p.SyncToDir(auths) 之后**：监听建立的是**当前**目录指纹作基线，
	// 若排在 SyncToDir 之前，两者之间发生的目录变化会被当成「基线之内」而漏掉。
	//
	// 也必须排在下面「多产品 p.Add」**之前**吗？—— 不必，且不应：热加载的剔除
	// 已显式放过非 WorkBuddy 账号（见 pool.keepWorkBuddyOnly），
	// 两个顺序都对。这里贴着 SyncToDir 放，是为了让「启动对齐 + 运行期对齐」
	// 这对概念挨在一起读。
	//
	// 仅配置启用时启动（缺省 true；见 config.Pool.WatchAuthDir）。
	if cfg.Pool.WatchAuthDir {
		stopWatch := p.StartAuthDirWatchWithInterval(cfg.AuthDir, cfg.WatchAuthDirIntervalD)
		defer stopWatch() // 优雅退出：停掉轮询 goroutine（stop 幂等）
	} else {
		log.Printf("凭证目录热加载已禁用（pool.watch_auth_dir=false）：新增账号后需手动重启网关")
	}

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
		// Qoder 权益领取时点与开关（2026-09-22 新增）。
		//
		// ⚠ 用 `cfg.QoderClaimOn()` 而不是某个具体字段：它封装了
		// 「分产品键优先、缺席回落到总闸」的两级语义（见该方法说明）。
		// 这里直接读 `QoderClaimEnabled` 会把老配置（只有总闸）当成关闭。
		QoderClaimHours:    cfg.Schedule.QoderClaimHours,
		QoderClaimDisabled: !cfg.QoderClaimOn(),
		ActivityReportCount:  cfg.Schedule.ActivityReportCount,
		CheckinScope:         cfg.Schedule.CheckinScope,
		Records:              recorder,
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
	// claimSched ZCode 套餐自动领取调度器（见下方 newClaimScheduler 处）。
	var claimSched *zcode.ClaimScheduler
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
		// ⚠ 用**具体类型**局部变量再赋给接口变量。
		//
		// `zcodeDispatch` 声明为 `server.ProductUpstream`（接口），而
		// `SetAuthDir` / `newProductTasksRunner` 需要 `*zcode.Dispatch`
		// （具体类型）—— 直接调会被编译器拒绝（接口没有那个方法）。
		// 我第一版就是那样，报 `SetAuthDir undefined` 与
		// `cannot use … as *zcode.Dispatch`。
		//
		// ⚠⚠ **必须把代理装到 ZCode 的 client 上**（2026-09-20 实测缺陷）。
		//
		// ZCode 走的是**另一套 client**（`zcode.New()`），而上面那个
		// `up.SetProxy(cfg.Proxy)` 只作用于 `upstream.Client` —— 两者互不相干。
		// 于是 ZCode 的请求**永远直连**，即使配置里明明写了代理。
		//
		// 真实对话实测（同一份凭证、同一个模型）：
		//
		//	独立探针（zcode.New()，直连）        → **HTTP 200** ✓
		//	走网关（同一个 zcode 包）            → 503「无法连接上游（网络超时）」
		//
		// 只差代理。而 `zcode.z.ai` 以 `.ai` 结尾，本就会按 `proxy_scope`
		// 判成国际版走代理 —— 那份判断对，只是 ZCode 的 client 从没读它。
		//
		// ⚠ 顺序与 upstream 一致：先 SetProxy 再 SetProxyScope 的等价物。
		// 这里 `zcode.Client` 只有一个开关（不分区域），故按国际版的口径
		// 决定是否挂代理：`proxy_scope.intl` 为真且地址非空才挂。
		zc := zcode.New()
		if proxyAddr := strings.TrimSpace(cfg.Proxy); proxyAddr != "" && cfg.ProxyScope.Intl {
			if err := zc.SetProxy(proxyAddr); err != nil {
				log.Printf("⚠ ZCode 代理设置失败（将直连）：%v", err)
			} else {
				log.Printf("ZCode 走代理：%s", proxyAddr)
			}
		} else {
			// 显式直连 —— 与 upstream 的 newDirectTransport 同一口径
			//（空地址时也不回落环境变量，避免"关了还走代理"）。
			_ = zc.SetProxy("")
		}
		zd := zcode.NewDispatch(zc)
		// 告诉它凭证目录 —— 供自动领取遍历（见 SetAuthDir 的说明）。
		zd.SetAuthDir(zcodeDir)
		zcodeDispatch = zd

		// 自动领取调度器 —— **按参考实现的实际逻辑**（所有者明确要求：
		// 「zcode要按照实际逻辑去做啊，他那个仓库怎么做我们就怎么做」）。
		//
		// 语义在 internal/zcode/claim_scheduler.go 里逐条照搬：
		// 启动即跑 + 5 分钟轮询 + hold 硬闸 + 失败 cooldown +
		// 成功/已领后 hold 到套餐截止 + login_required 永久停止。
		//
		// ⚠ 它是**独立循环**，不走网关那套"按小时时点"的排程：
		// 参考实现要抢限量名额，节奏是分钟级；按小时排会变成"捡剩的"。
		claimSched = zcode.NewClaimScheduler(
			zcode.New(),
			zd.LoadCreds,
			// enabled 读配置（支持运行时改，不必重启）。
			//
			// ⚠ 2026-09-22 改为**分产品开关**：
			// 此前读的是总闸 `ProductTasksEnabled`，而 Qoder 与 ZCode
			// 的用户诉求完全不同（一个要"每天领额度"，一个要"抢限量套餐"），
			// 合成一个开关会让用户想关 A 却把 B 也关了。
			//
			// 所有者原话：「权益自动领取 qoder zcode 拆分开,不要合成一个」。
			//
			// `ZcodeClaimOn()` 内部处理了回落：分产品键缺席时跟总闸走，
			// 故老配置行为不变（见该方法的说明）。
			func() bool { return cfg.ZcodeClaimOn() },
		)
		// 手动「立即领取」与自动路径**共用同一套领取逻辑**（见 ClaimOnce）。
		//
		// ⚠ 必须把 Qoder 的凭证目录与开关一并传进去（2026-09-22 修正）：
		// 此前只传 ZCode，导致**排程路径永远不领 Qoder 的活动** ——
		// 而所有者明确要求「qoder改为 早十点,晚九点 两次触发」。
		sch.SetProductTasksRunner(newProductTasksRunner(
			zd, claimSched,
			qoderDir,          // Qoder 凭证目录（领取要遍历账号）
			cfg.QoderClaimOn(), // 分产品开关（缺席回落总闸）
		))
		log.Printf("ZCode 自动领取：启动即跑，之后每 %v 轮询（失败冷却 %v，开关=%v）",
			zcode.ClaimPollInterval, zcode.ClaimCooldown, cfg.ZcodeClaimOn())
		log.Printf("Qoder 权益领取：时点 %v（开关=%v，超时 %v 补一轮轮询）",
			cfg.Schedule.QoderClaimHours, cfg.QoderClaimOn(), cfg.Schedule.QoderClaimIntervalMinutes)

		// 成本维度：让各产品按"单位额度消耗率"参与加权（见 design.md §2.3）。
		p.SetMultiProduct(true, 0.3)

		// 把"各产品实际提供哪些模型"注入池 —— 用于**无前缀模型名**时
		// 排除"不提供该模型的产品"的账号。
		//
		// # 为什么必须有（2026-09-20 实测缺陷）
		//
		// 用户用 `deepseek-v4.1-flash`（WorkBuddy 的模型）发请求，却被路由到
		// **Qoder 账号**并失败两次 —— 因为无前缀时只按"账号当前可用"挑，
		// 不问"这个产品有没有这个模型"。
		//
		// ⚠ 传空值时 SetProductModels 会清成"不约束"（回到既有行为），
		// 不会误排除所有账号。
		//
		// ⚠⚠ 2026-09-21：宿主那份清单**可能缺 workbuddy**（它来自用户的
		// 「限制使用的模型」白名单，默认为空 ⇒ 不写那一项），而缺了会让
		// 裸名请求被路由到 WorkBuddy 账号 —— 见 pool.pickForModelAny 的注释
		//（所有者现场：`Qwen3.8-Flash` 报 11102，加 `qoder:` 前缀才好）。
		//
		// 故这里**补一份网关自己已知的 WorkBuddy 模型名**作为兜底：
		// 来源是网关**实际下发**给客户端的静态表（server.StaticWorkBuddyModelIDs），
		// 与 `/v1/models` 同源，不会两边分叉。
		//
		// ===================================================================
		// ⚠⚠⚠ 2026-09-22 修正：这份**兜底清单不能参与"声明"竞争**
		// ===================================================================
		//
		// 原注释写着「它是"声明"用的，不是"否定"用的 —— 多列几个不会让
		// 任何账号失去资格，所以宁可补全」。**那句话在引入 `declared`
		// 优先逻辑之后就失效了。**
		//
		// 所有者现场（2026-09-22）：
		//
		//	发 `GLM-5.3`   → 400 model_not_in_region
		//	发 `Auto`      → 400 model_not_in_region
		//	发 `Qwen3.8-Flash` → **200 正常**
		//
		// 差别在**重叠**：`Qwen3.8-Flash` 只有 qoder 声明（路由唯一），
		// 而 `GLM-5.3` / `Auto` 同时被 workbuddy（这份兜底）与 qoder 声明。
		//
		// `pickForModelAny` 的做法是"优先只在**明确声明**的产品里挑"，
		// 于是两个产品都进 `declared` → 一起参与竞争 → 池里
		// **19 个 WorkBuddy 账号 vs 1 个 qoder 账号** ⇒ 大概率选到
		// WorkBuddy，而它其实**没有** `GLM-5.3`（静态表是网关的**猜测**，
		// 不是上游的真实能力清单）⇒ 上游回 11102。
		//
		// ⇒ 修法：兜底清单**只用于"不排除"**（`productMayServe`），
		// 不用于"声明"（`productDeclaresModel`）。即"我不知道 WorkBuddy
		// 提供什么，所以别排除它；但也别声称它提供"。
		//
		// 实现见 `Pool.SetProductModelsFallback` —— 清单分成两份：
		//
		//	productModelSet          宿主给的**可信**清单（来自上游真实查询）
		//	productModelFallbackSet  网关补的**猜测**清单（只影响"不排除"）
		pm := cfg.Pool.ProductModels
		fallback := map[string][]string{}
		if len(pm["workbuddy"]) == 0 {
			fallback["workbuddy"] = server.StaticWorkBuddyModelIDs()
			log.Printf("product_models 缺 workbuddy：已补 %d 个模型作为**兜底**（只用于「不排除」，不参与「声明」竞争）",
				len(fallback["workbuddy"]))
		}
		p.SetProductModelsFallback(fallback)
		p.SetProductModels(pm)
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
		// 短信登录成功后登记账号（2026-09-22 新增）。
		//
		// ⚠ 必须在这里接线，否则 `/login/sms/verify` 会明确报
		// 「cannot register accounts」—— 表现为"验证码发得出、输入也对，
		// 但登录就是失败"，而错误信息只在服务端日志里。
		SMSLogin: newSMSLoginFunc(p, cfg.AuthDir),
		// 「限制使用的模型」白名单：三个工作模式都生效，空 = 不限制（默认）。
		//
		// 直接把已解析的切片传下去（不再按 rotation 过滤）：限制模型与「用哪些
		// 账号」是正交的两件事，只在轮转下生效会让自动/手动模式完全无法限制模型。
		// 老配置的 `allowed_model` 字符串由 AllowedModels.UnmarshalJSON 读成
		// 单元素切片，因此老配置升级后行为逐字不变。
		AllowedModels: cfg.Pool.AllowedModels,
		// 各产品**实际可用**的模型清单 —— 供 `/v1/models` 的 `channels`
		// 字段（"这个模型来自哪个平台"）。
		//
		// 由宿主透传（见 config.Pool.ProductModels 的说明）：
		// `/v1/models` 在请求路径上，不该在那里发外部请求去问上游。
		ProductModels: cfg.Pool.ProductModels,
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
	// ZCode 套餐自动领取：**独立循环**（启动即跑 + 5 分钟轮询 + 动态退避）。
	//
	// 与上面的 `sch.Run` 并列而不是并进去：两者的节奏完全不同
	//（网关排程按小时时点，本调度器按分钟级轮询抢限量名额）。
	startProductTasks(ctx, claimSched)
	// 积分到期巡检：独立于签到的高频刷新，驱动账号池「先烧快过期额度」的分层选号。
	if cfg.Pool.CreditRefreshEnabled {
		go sch.RunCreditRefreshLoop(ctx, cfg.CreditRefreshIntervalD)
		log.Printf("积分到期巡检已启用：每 %s 刷新一次（驱动到期分层选号）", cfg.CreditRefreshIntervalD)
	} else {
		log.Printf("积分到期巡检已禁用（pool.credit_refresh_enabled=false）：到期分层仅依赖签到与宿主同步")
	}

	// ── 配置与多产品凭证的热重载（2026-09-21 所有者报的缺陷）──────────
	//
	// # 缺陷现象
	//
	// 所有者原话：
	//
	//	「兼容网关启动之后，我再添加的 zcode 和 qoder 账号，模型清单路由
	//	  也没有显示 qoder 和 zcode 支持的账号，应该要自动重启或者热重载的」
	//	「而且 /v1/models 接口，也没有返回 qoder 和 zcode 支持的模型，这也是个 bug」
	//
	// # 根因链（逐层可核）
	//
	//	① 宿主在**新增/刷新账号**后会重写 `gateway_native_config.json`
	//	   （Rust 侧 `resync_native_config`），把 `pool.product_models`
	//	   更新为最新清单、并把新账号写进 qoder/zcode 的 auths 目录；
	//	② 但 Go 网关只在 main 开头 `Load(*cfgPath)` **读一次**，
	//	   多产品凭证目录也只在启动时扫一次；
	//	③ ⇒ 运行中的网关永远看不到新账号与它们的模型清单。
	//
	// 用户的期望是"加了账号就该能用"，而实际要手动重启整个应用
	//（重启还会掐断正在进行的对话）。
	//
	// # 为什么用轮询而不是 fsnotify
	//
	// 与 `pool.WatchAuthDir` 同一理由（见 internal/pool/watch.go 的包注释）：
	// 零新依赖，且在网络盘/容器里不丢事件。配置变更是**低频**事件
	//（用户点一次"刷新账号"），5 秒的延迟完全可以接受。
	if *cfgPath != "" {
		startConfigWatch(ctx, *cfgPath, h, p, cfg, qoderDispatch, zcodeDispatch)
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

// configWatchInterval 配置文件的轮询周期。
//
// 5 秒与 `pool.WatchAuthDir` 的默认值一致：配置变更是低频事件
//（用户点一次"刷新账号"），5 秒延迟无感；而更密只会带来无谓的 IO。
const configWatchInterval = 5 * time.Second

// startConfigWatch 轮询配置文件与多产品凭证目录，变化时热重载。
//
// # 为什么两件事放在同一个循环里
//
// 它们的触发源是**同一个用户动作**（"刷新账号"/"新增账号"）：
// 宿主会同时（a）重写配置里的 `product_models`、（b）往 qoder/zcode
// 的 auths 目录写新凭证。分成两个循环只会让"清单更新了但账号还没进池"
// 这个中间态持续更久 —— 那正是用户看到的"模型列表里有但选了说账号不可用"。
//
// # 为什么凭证热加载要单独做（不能复用 pool.WatchAuthDir）
//
// `pool.WatchAuthDir` 只监听 **WorkBuddy 的 auth_dir**，且其剔除逻辑
// 显式放过非 WorkBuddy 账号（见 pool.keepWorkBuddyOnly）。多产品的
// qoder/zcode 凭证目录**从来没被监听** —— 这是本次要补的缺口。
func startConfigWatch(
	ctx context.Context,
	cfgPath string,
	h *server.Handler,
	p *pool.Pool,
	bootCfg *Config,
	qoderDispatch, zcodeDispatch server.ProductUpstream,
) {
	// 基线：当前文件 mtime 与大小，以及各产品已加载的凭证指纹。
	lastCfg := fileStamp(cfgPath)
	lastCreds := map[string]string{
		"qoder": dirStamp(resolveAuthDir(bootCfg.Pool.QoderAuthDir, qoder.DefaultAuthDir, qoderDispatch != nil)),
		"zcode": dirStamp(resolveAuthDir(bootCfg.Pool.ZcodeAuthDir, zcode.DefaultAuthDir, zcodeDispatch != nil)),
	}

	go func() {
		ticker := time.NewTicker(configWatchInterval)
		defer ticker.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-ticker.C:
			}

			// ---- ① 配置热重载（product_models 等数据类字段）----
			if stamp := fileStamp(cfgPath); stamp != lastCfg {
				next, err := Load(cfgPath)
				if err != nil {
					// 解析失败**不覆盖**现有配置：宿主可能正写到一半
					//（非原子写）。保持旧配置比装载一份残缺的好。
					log.Printf("配置已变化但解析失败（保留当前配置，稍后重试）：%v", err)
				} else {
					h.ReloadDynamic(&server.DynamicConfig{
						ProductModels: next.Pool.ProductModels,
						PromptMode:    next.Prompt.Mode,
						PromptText:    next.PromptText,
					})
					lastCfg = stamp
					if n := len(next.Pool.ProductModels); n > 0 {
						log.Printf("配置热重载：模型清单已更新（%d 个产品）", n)
					} else {
						log.Printf("配置热重载：模型清单已更新（空）")
					}
				}
			}

			// ---- ② 多产品凭证热加载 ----
			for _, spec := range []struct {
				product string
				dir     string
				load    func() (int, error)
			}{
				{"qoder", resolveAuthDir(bootCfg.Pool.QoderAuthDir, qoder.DefaultAuthDir, qoderDispatch != nil),
					func() (int, error) { return reloadQoder(p, bootCfg) }},
				{"zcode", resolveAuthDir(bootCfg.Pool.ZcodeAuthDir, zcode.DefaultAuthDir, zcodeDispatch != nil),
					func() (int, error) { return reloadZcode(p, bootCfg) }},
			} {
				if spec.dir == "" {
					continue // 该产品未启用
				}
				stamp := dirStamp(spec.dir)
				if stamp == lastCreds[spec.product] {
					continue
				}
				lastCreds[spec.product] = stamp
				n, err := spec.load()
				if err != nil {
					log.Printf("%s 凭证热加载失败：%v", spec.product, err)
					continue
				}
				log.Printf("%s 凭证热加载：目录已变化，重新载入 %d 个账号", spec.product, n)
			}
		}
	}()
}

// resolveAuthDir 算出某产品的凭证目录（配置为空时用默认值；产品未启用时返回空）。
func resolveAuthDir(configured string, fallback func() string, enabled bool) string {
	if !enabled {
		return ""
	}
	if configured != "" {
		return configured
	}
	return fallback()
}

// fileStamp 文件的"变更指纹"（mtime + 大小）。
//
// 只比 mtime 会漏掉"同一秒内改两次"（mtime 精度到秒时），
// 加上大小能挡住绝大多数漏判；而真正的原子替换（写临时文件再 rename）
// 两种都会变。取不到文件时返回空串 —— 与"文件不存在"这一态对应。
func fileStamp(path string) string {
	if path == "" {
		return ""
	}
	st, err := os.Stat(path)
	if err != nil {
		return ""
	}
	return fmt.Sprintf("%d:%d", st.ModTime().UnixNano(), st.Size())
}

// dirStamp 目录内文件的"变更指纹"（各文件 mtime+大小的汇总）。
//
// 汇总成单个字符串：调用方只需判断"变了没有"，
// 逐文件比对会让每个周期都分配一堆字符串。
//
// 目录为空/不存在时返回空串 —— 与"该产品未启用"这一态一致
//（调用方对空串直接跳过，不再尝试加载）。
func dirStamp(dir string) string {
	if dir == "" {
		return ""
	}
	entries, err := os.ReadDir(dir)
	if err != nil {
		return ""
	}
	var b strings.Builder
	for _, e := range entries {
		if e.IsDir() {
			continue
		}
		info, err := e.Info()
		if err != nil {
			continue
		}
		fmt.Fprintf(&b, "%s=%d:%d;", e.Name(), info.ModTime().UnixNano(), info.Size())
	}
	return b.String()
}

// reloadQoder 重新扫描 Qoder 凭证目录并同步进池。
func reloadQoder(p *pool.Pool, cfg *Config) (int, error) {
	dir := resolveAuthDir(cfg.Pool.QoderAuthDir, qoder.DefaultAuthDir, true)
	if dir == "" {
		return 0, nil
	}
	creds, failed, err := qoder.LoadDir(dir)
	if err != nil {
		return 0, err
	}
	for _, f := range failed {
		log.Printf("Qoder 凭证解析失败，已跳过：%s", f)
	}
	n := 0
	for _, cr := range creds {
		if cr.UID == "" {
			continue
		}
		if cr.EnsureFingerprint() {
			if err := cr.SaveAtomic(); err != nil {
				log.Printf("Qoder 账号 %s 保存机器指纹失败：%v", maskUID(cr.UID), err)
			}
		}
		p.Add(qoderAuthOf(cr))
		n++
	}
	return n, nil
}

// reloadZcode 重新扫描 ZCode 凭证目录并同步进池。
func reloadZcode(p *pool.Pool, cfg *Config) (int, error) {
	dir := resolveAuthDir(cfg.Pool.ZcodeAuthDir, zcode.DefaultAuthDir, true)
	if dir == "" {
		return 0, nil
	}
	creds, failed, err := zcode.LoadDir(dir)
	if err != nil {
		return 0, err
	}
	for _, f := range failed {
		log.Printf("ZCode 凭证解析失败，已跳过：%s", f)
	}
	n := 0
	for _, cr := range creds {
		if cr.UID == "" {
			continue
		}
		p.Add(zcodeAuthOf(cr))
		n++
	}
	return n, nil
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
