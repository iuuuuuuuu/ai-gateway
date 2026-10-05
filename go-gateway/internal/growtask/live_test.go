//go:build live

// live_test.go 真实账号联调（**只在显式指定 live 构建标签时编译与运行**）。
//
// 为什么用构建标签而不是普通的 _test.go：
//   - 它会打真实上游、做真实写操作（报名 / 领奖 / 发对话），
//     绝不能被 `go test ./...` 顺带跑到；
//   - 同时它又不是一次性的临时脚本 —— 上游改判据、加任务时，
//     需要能一条命令重跑同一套口径，把「怎么验的」固化下来。
//
// 运行方式（PowerShell）：
//
//	$env:WB_LIVE_ACCOUNTS = "D:\...\test-bin\growth-iso\accounts.json"
//	$env:WB_LIVE_ACCOUNT  = "<uid前8位>"     # 缺省用第一个国服账号
//	go test -tags live ./internal/growtask/ -run TestLive -v
//
// 写操作分级（默认最保守）：
//
//	（无额外变量）        只读：拉任务列表，打印真实形状
//	WB_LIVE_ACCEPT=1      报名：幂等、不扣资源、不产生进度
//	WB_LIVE_RUN=1         一键完成：会发真实对话/消耗额度，务必只对指定账号小范围试
//	WB_LIVE_TASK=<code>   配合 RUN=1 时只跑单个任务
//
// 账号库**只读**：从 WB_LIVE_ACCOUNTS 读入后转成网关的凭证形态写入临时目录，
// 绝不回写源文件（源文件是所有者的生产账号库）。
package growtask

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"testing"
	"time"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/upstream"
)

// liveAccount 所有者账号库里的单条记录（snake_case 字段名，与网关凭证形态不同）。
type liveAccount struct {
	AccessToken  string `json:"access_token"`
	RefreshToken string `json:"refresh_token"`
	UID          string `json:"uid"`
	Domain       string `json:"domain"`
	ExpiresAt    int64  `json:"expiresAt"`
	EnterpriseID string `json:"enterpriseId"`
	Nickname     string `json:"nickname"`
}

// IsIntl 报告该账号是否属于国际版 —— **委托生产的区域判定**，不在这里另写一套。
//
// 为什么必须委托：国际版域名白名单（auth.intlDomains）在 auth 包里，且它的注释
// 记录过一次真实事故 —— 旧的近似判据 `HasSuffix(".ai")` 把 Qoder 国际版的
// `qoder.sh` 误判成国服，导致代理策略对它不生效。联调代码若自己写一套近似判据，
// 就会出现"联调时按国服跑、生产按国际版跑"的分叉，而联调的全部价值恰恰是
// **复现生产行为**。故这里构造一个最小 Auth 走同一条判定。
//
// （本方法此前缺失，导致 `-tags live` 整体编译失败 —— 联调基建一度不可用。）
func (a liveAccount) IsIntl() bool {
	return (&auth.Auth{Domain: a.Domain}).IsIntl()
}

// loadLiveAccount 读取真实账号并转成网关的凭证形态。
//
// 转换而非直接解析：账号库用的是 snake_case 扁平形，而 auth.Parse 认的是
// camelCase（扁平）或嵌套形。走一次转换才能用**生产同一套**解析逻辑，
// 否则联调验证的是"另一条解析路径"，与线上行为不一致。
func loadLiveAccount(t *testing.T, uid8 string) *auth.Auth {
	t.Helper()
	path := os.Getenv("WB_LIVE_ACCOUNTS")
	if path == "" {
		t.Skip("未设置 WB_LIVE_ACCOUNTS，跳过真实账号联调")
	}
	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("读取账号库失败: %v", err)
	}
	var all []liveAccount
	if err := json.Unmarshal(raw, &all); err != nil {
		t.Fatalf("解析账号库失败: %v", err)
	}
	if len(all) == 0 {
		t.Fatal("账号库为空")
	}

	// 选号：指定 uid8 前缀优先，否则取第一个国服账号。
	// 默认不碰国际版 —— 它的成长域是另一套任务集（无奖励）。
	var pick *liveAccount
	for i := range all {
		a := &all[i]
		if uid8 != "" {
			if strings.HasPrefix(a.UID, uid8) {
				pick = a
				break
			}
			continue
		}
		if !a.IsIntl() {
			pick = a
			break
		}
	}
	if pick == nil {
		t.Fatalf("账号库里找不到匹配 uid8=%q 的账号", uid8)
	}

	// 转成嵌套形写入临时目录，再由 auth.LoadDir 走生产解析路径。
	dir := t.TempDir()
	doc := map[string]any{
		"auth": map[string]any{
			"accessToken":  pick.AccessToken,
			"refreshToken": pick.RefreshToken,
			"expiresAt":    pick.ExpiresAt,
			"domain":       pick.Domain,
		},
		"account": map[string]any{
			"uid":          pick.UID,
			"enterpriseId": pick.EnterpriseID,
			"nickname":     pick.Nickname,
		},
	}
	enc, _ := json.MarshalIndent(doc, "", "  ")
	file := filepath.Join(dir, "workbuddy-live.json")
	if err := os.WriteFile(file, enc, 0o600); err != nil {
		t.Fatalf("写入临时凭证失败: %v", err)
	}
	auths, err := auth.LoadDir(dir)
	if err != nil || len(auths) != 1 {
		t.Fatalf("加载临时凭证失败: %v (n=%d)", err, len(auths))
	}
	return auths[0]
}

// shortUID 脱敏：只保留 uid 前 8 位。报告与日志里不出现完整 uid。
func shortUID(uid string) string {
	if len(uid) > 8 {
		return uid[:8]
	}
	return uid
}

// liveClient 生产默认上游客户端（真实域名）。
func liveClient() *upstream.Client { return upstream.New() }

// liveAbsenceExpected 记录「在真实列表里缺席属正常」的任务码。
//
// 白名单只收**有实测依据**的两类，不收「跑不通就加进来」的任务 ——
// 后者会让这条漂移断言慢慢失效：
//
//   - school_season：校园日活动。活动到期后上游把整条任务撤下。
//     2026-09-30 实测已消失；解析器本身正常，只是没数据可解。
//   - Sequential_Tasks_2 / _3：成长任务集的后续两环。_1 未完成前上游
//     在两种列表口径都查不到它们（2026-09-30 实测）；_1 完成后 _2 会露面
//     但带日期锁（unlock_at），_3 仍未露面。缺席属设计如此。
//
// 注意 _1 不在白名单里：它是这一串的入口，任何时候都应当可见；
// 若它消失，说明整组任务被撤了，那是真漂移，必须报出来。
var liveAbsenceExpected = map[string]bool{
	"school_season":      true,
	"Sequential_Tasks_2": true,
	"Sequential_Tasks_3": true,
}

// TestLiveListTasksShape 只读：拉取真实任务列表，核对解析结果。
//
// 这一步的价值是**校验解析器对真实响应的适配**：形状认错了，
// 后面所有动作的判据都会错，而单测里的假上游是我自己写的，认不出这一点。
func TestLiveListTasksShape(t *testing.T) {
	a := loadLiveAccount(t, os.Getenv("WB_LIVE_ACCOUNT"))
	c := liveClient()
	t.Logf("账号 uid8=%s realm=%s domain=%s", shortUID(a.UID), a.Realm(), a.Domain)

	tasks, err := c.ListGrowthTasks(a)
	if err != nil {
		t.Fatalf("拉取任务列表失败: %v", err)
	}
	if len(tasks) == 0 {
		t.Fatal("任务列表为空 —— 区域或鉴权可能不对")
	}

	// 逐条打印（脱敏：只有任务码与进度，没有账号信息）。
	t.Logf("共 %d 个任务：", len(tasks))
	claimable, needsAccept, claimed := 0, 0, 0
	for _, task := range tasks {
		t.Logf("  %-24s accept=%-13s progress=%-10s reward=%dc/%de has_progress=%v",
			task.Code, task.AcceptStatus, task.ProgressText(),
			task.Credit, task.Energy, task.HasProgress)
		if task.Claimable() {
			claimable++
		}
		if task.NeedsAccept() {
			needsAccept++
		}
		if task.Claimed() {
			claimed++
		}
	}
	t.Logf("汇总：可领奖 %d、待报名 %d、已领奖 %d", claimable, needsAccept, claimed)

	// 动作表里的可自动任务应当都在真实列表里出现。缺失说明上游任务集变了，
	// 动作表需要同步 —— 这是必须被发现的漂移。
	//
	// 但有两类任务缺席**不是**漂移，混在一起判失败会让这条断言长期红着，
	// 最后谁都不看了（那就等于没有断言）：
	//
	//  1. 活动任务：活动一到期，上游把整条任务撤下。2026-09-30 实测
	//     school_season 已消失 —— 而解析器对它的处理完全正常，只是没数据。
	//  2. 成长任务集的后续两环 _2/_3：_1 未完成前上游根本不下发它们
	//     （实测两种列表口径都查不到）；_1 完成后 _2 才露面（且带日期锁），
	//     _3 仍未露面。缺席是设计如此，不是解析漏了。
	//
	// 所以这两类缺席只打提示。真正要抓的漂移是「本来该在的常规任务不见了」。
	have := map[string]bool{}
	for _, task := range tasks {
		have[task.Code] = true
	}
	var missing, missingExpected []string
	for _, code := range SupportedCodes() {
		if have[code] {
			continue
		}
		if liveAbsenceExpected[code] {
			missingExpected = append(missingExpected, code)
			continue
		}
		missing = append(missing, code)
	}
	sort.Strings(missing)
	sort.Strings(missingExpected)
	if len(missing) > 0 {
		t.Errorf("动作表里的任务在真实列表中缺失（任务集可能已变更）: %v", missing)
	}
	if len(missingExpected) > 0 {
		t.Logf("以下任务缺席属预期（活动已结束 / 成长任务集尚未露面），不作为失败：%v", missingExpected)
	}
}

// TestLiveAcceptIdempotent 写操作（安全）：报名幂等且不产生进度。
//
// 报名是「一键完成」里唯一可以放心重放的写操作：它不扣资源、不产生进度，
// 重复调用返回 already_accepted。这里验证的正是这条前提 ——
// 它是整个「一键完成」可以无脑重跑的基石。
func TestLiveAcceptIdempotent(t *testing.T) {
	if os.Getenv("WB_LIVE_ACCEPT") != "1" {
		t.Skip("未设置 WB_LIVE_ACCEPT=1，跳过报名写操作")
	}
	a := loadLiveAccount(t, os.Getenv("WB_LIVE_ACCOUNT"))
	c := liveClient()
	// 走 Runner 的报名分派而不是直接调上游：小程序限定任务（如
	// Sequential_Tasks_1）必须带 X-Client-Platform: miniprogram 才会登记，
	// 缺头时上游返回 HTTP 200 + code=0，逐条却是 {"status":"error"}。
	// 用生产路径才能测出这条真实差别 —— 直接调 c.AcceptGrowthTasks 会
	// 把它算成「报名后仍无 progress」，那是**测试自己走错口径**的假失败。
	r := New(Options{Upstream: c})

	before, err := c.ListGrowthTasks(a)
	if err != nil {
		t.Fatalf("拉取任务列表失败: %v", err)
	}
	var pending []string
	for _, task := range before {
		if task.NeedsAccept() {
			pending = append(pending, task.Code)
		}
	}
	if len(pending) == 0 {
		t.Skip("没有待报名的任务")
	}
	t.Logf("uid8=%s 待报名 %d 个：%v", shortUID(a.UID), len(pending), pending)

	// 第一次：应为新报名。
	first, err := r.acceptTasks(a, pending)
	if err != nil {
		t.Fatalf("首次报名失败: %v", err)
	}
	for _, one := range first {
		t.Logf("  首次报名 %-24s → %s", one.Code, one.Status)
	}

	// 第二次：必须幂等（already_accepted），且不报错。
	second, err := r.acceptTasks(a, pending)
	if err != nil {
		t.Fatalf("重复报名应为幂等而非报错: %v", err)
	}
	for _, one := range second {
		if one.Status != "already_accepted" && one.Status != upstream.TaskAcceptAccepted {
			t.Errorf("重复报名 %s 返回意外状态 %q", one.Code, one.Status)
		}
	}

	// 回读：报名后 progress 必须出现 —— 这是「必须先报名」这条设计的实证。
	//
	// 例外：Expert_Philanthropy 报名后**依然不下发 progress**（实测）。
	// 这正是它无法自动完成的第二个证据：判据不是进度计数，
	// 而是服务端侧的捐赠回执，因此上游根本不给它进度字段。
	// 把它算作失败会掩盖这条真实规律，故单独识别并记录。
	after, err := c.ListGrowthTasks(a)
	if err != nil {
		t.Fatalf("回读任务列表失败: %v", err)
	}
	var noProgress []string
	for _, task := range after {
		if !contains(pending, task.Code) {
			continue
		}
		if !task.HasProgress {
			noProgress = append(noProgress, task.Code)
			if task.Code != "Expert_Philanthropy" {
				t.Errorf("报名后 %s 仍无 progress —— 「先报名」的前提不成立", task.Code)
			}
			t.Logf("  报名后 %-24s accept=%-10s progress=（无进度字段）", task.Code, task.AcceptStatus)
			continue
		}
		t.Logf("  报名后 %-24s accept=%-10s progress=%s", task.Code, task.AcceptStatus, task.ProgressText())
	}
	if len(noProgress) > 0 {
		sort.Strings(noProgress)
		t.Logf("报名后仍无进度的任务：%v", noProgress)
	}
	// 至少绝大多数任务应当出现进度，否则说明报名没有真正生效。
	if len(noProgress) >= len(pending) {
		t.Errorf("报名后没有任何任务出现进度，报名可能未生效")
	}
}

// TestLiveRunOne 写操作（**消耗资源**）：对单个任务执行一键完成。
//
// 默认不跑：会发真实对话 / 消耗额度。仅在显式设置 WB_LIVE_RUN=1 时执行，
// 且必须配合 WB_LIVE_TASK 指定单个任务 —— 避免"一条命令跑满 17 个任务"
// 这种不可控的消耗。
func TestLiveRunOne(t *testing.T) {
	if os.Getenv("WB_LIVE_RUN") != "1" {
		t.Skip("未设置 WB_LIVE_RUN=1，跳过一键完成写操作（会消耗资源）")
	}
	code := os.Getenv("WB_LIVE_TASK")
	if code == "" {
		t.Fatal("WB_LIVE_RUN=1 时必须用 WB_LIVE_TASK 指定单个任务（避免一次跑满全部任务）")
	}
	a := loadLiveAccount(t, os.Getenv("WB_LIVE_ACCOUNT"))
	c := liveClient()

	// 真实节奏：轮询预算按生产默认（4×3 秒），因为这里要验证的
	// 恰恰是"异步计分要不要等" —— 把等待压掉就失去了联调的意义。
	r := New(Options{Upstream: c})
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Minute)
	defer cancel()

	t.Logf("uid8=%s 执行任务 %s …", shortUID(a.UID), code)
	start := time.Now()
	item := r.RunOne(ctx, a, code)
	t.Logf("结果（耗时 %s）：status=%s claimed=%v progress %s → %s",
		time.Since(start).Round(time.Second), item.Status, item.Claimed,
		item.ProgressBefore, item.ProgressAfter)
	t.Logf("  说明：%s", item.Message)
	if item.Credit > 0 || item.Energy > 0 {
		t.Logf("  到账：+%d 积分 +%d 能量", item.Credit, item.Energy)
	}
	if item.ClaimError != "" {
		t.Logf("  领奖失败：%s", item.ClaimError)
	}
	// 不把 status != done 当失败：该账号没有此任务、或时段不对都是正常结果。
	// 联调要看的是**真实发生了什么**，不是测试通过与否。
}

// TestLiveRunAllIdempotent 写操作：整轮「一键完成」及其**幂等性**。
//
// 这是本功能最关键的一条需求验证：重复点「一键完成」不得重复扣资源。
// 做法是**连跑两轮**并对比 —— 第二轮应当几乎全是跳过，
// 且不再产生真实对话、不再重复领奖。
//
// 之所以敢连跑：行为事件按天幂等、任务状态会被跳过，第二轮的实际消耗
// 只有那些"本来就还没做完"的任务（如夜猫子按差额补足）。
func TestLiveRunAllIdempotent(t *testing.T) {
	if os.Getenv("WB_LIVE_RUN_ALL") != "1" {
		t.Skip("未设置 WB_LIVE_RUN_ALL=1，跳过整轮实测（会消耗资源）")
	}
	a := loadLiveAccount(t, os.Getenv("WB_LIVE_ACCOUNT"))
	c := liveClient()
	r := New(Options{Upstream: c})

	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Minute)
	defer cancel()

	type round struct {
		items   []ItemResult
		claimed int
		credit  int64
		energy  int64
	}
	runRound := func(label string) round {
		var out round
		t.Logf("---- %s ----", label)
		start := time.Now()
		out.items = r.RunAll(ctx, a)
		for _, it := range out.items {
			t.Logf("  %-22s %-9s %-14s %s",
				it.Code, it.Status, it.ProgressBefore+" → "+it.ProgressAfter, it.Message)
			if it.Claimed {
				out.claimed++
				out.credit += it.Credit
				out.energy += it.Energy
			}
		}
		t.Logf("  %s 汇总：领奖 %d 项 +%d 积分 +%d 能量（耗时 %s）",
			label, out.claimed, out.credit, out.energy, time.Since(start).Round(time.Second))
		return out
	}

	first := runRound("第一轮")
	if len(first.items) == 0 {
		t.Fatal("整轮没有任何结果")
	}

	// 第二轮：幂等性验证。
	second := runRound("第二轮（幂等性验证）")

	// 第一轮领过的任务，第二轮不得再领一次（否则就是重复扣/重复发）。
	for _, it := range second.items {
		if it.Claimed {
			t.Errorf("第二轮又领奖了 %s —— 幂等性被破坏", it.Code)
		}
	}
	t.Logf("幂等性结论：第一轮领奖 %d 项，第二轮领奖 %d 项", first.claimed, second.claimed)
}
//
// 做法：对一个**已领奖**的任务调用领奖接口。若端点正确，上游回
// already_claimed=true；若端点错误（如打在 CLI 域），会回 HTTP 400 之类的
// 路由错误。两者极易混淆成"任务未完成"，故必须显式区分并记录下来。
// TestLiveClaimEndpointCrossCheck 只读交叉验证：确认领奖端点确实在 Web 域。
//
// 做法：对一个**已领奖**的任务调用领奖接口。若端点正确，上游回
// already_claimed=true；若端点错误（如打在 CLI 域），会回 HTTP 400 之类的
// 路由错误。两者极易混淆成"任务未完成"，故必须显式区分并记录下来。
func TestLiveClaimEndpointCrossCheck(t *testing.T) {
	if os.Getenv("WB_LIVE_CLAIM_CHECK") != "1" {
		t.Skip("未设置 WB_LIVE_CLAIM_CHECK=1，跳过领奖端点交叉验证")
	}
	a := loadLiveAccount(t, os.Getenv("WB_LIVE_ACCOUNT"))
	c := liveClient()

	tasks, err := c.ListGrowthTasks(a)
	if err != nil {
		t.Fatalf("拉取任务列表失败: %v", err)
	}
	var claimedCode string
	for _, task := range tasks {
		if task.Claimed() {
			claimedCode = task.Code
			break
		}
	}
	if claimedCode == "" {
		t.Skip("该账号没有已领奖的任务，无法做幂等交叉验证")
	}

	credit, energy, already, err := c.ClaimGrowthTask(a, claimedCode)
	if err != nil {
		t.Fatalf("对已领奖任务调领奖接口应回 already_claimed，实际报错: %v\n"+
			"（若为 HTTP 400，很可能是端点/基址不对，而不是『任务未完成』）", err)
	}
	t.Logf("uid8=%s 已领奖任务 %s 的领奖响应：already=%v credit=%d energy=%d",
		shortUID(a.UID), claimedCode, already, credit, energy)
	if !already {
		t.Errorf("已领奖任务应回 already_claimed=true，实际 already=%v", already)
	}
}

// contains 判断字符串切片是否含某值。
func contains(list []string, want string) bool {
	for _, s := range list {
		if s == want {
			return true
		}
	}
	return false
}

// TestLiveRealmInventory 只读：清点各区域账号的任务集差异（不打印任何账号标识）。
func TestLiveRealmInventory(t *testing.T) {
	path := os.Getenv("WB_LIVE_ACCOUNTS")
	if path == "" {
		t.Skip("未设置 WB_LIVE_ACCOUNTS，跳过")
	}
	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("读取账号库失败: %v", err)
	}
	var all []liveAccount
	if err := json.Unmarshal(raw, &all); err != nil {
		t.Fatalf("解析账号库失败: %v", err)
	}
	c := liveClient()
	byRealm := map[string]int{}
	for i := range all {
		realm := "cn"
		if all[i].IsIntl() {
			realm = "global"
		}
		byRealm[realm]++
	}
	var realms []string
	for k := range byRealm {
		realms = append(realms, k)
	}
	sort.Strings(realms)
	for _, r := range realms {
		t.Logf("区域 %s：%d 个账号", r, byRealm[r])
	}

	// 各区域的**任务集**差异：只打印任务码，不涉及账号标识。
	for _, realm := range realms {
		var probe *liveAccount
		for i := range all {
			isIntl := all[i].IsIntl()
			if (realm == "global") == isIntl {
				probe = &all[i]
				break
			}
		}
		if probe == nil {
			continue
		}
		a := &auth.Auth{
			AccessToken: probe.AccessToken, UID: probe.UID,
			Domain: probe.Domain, EnterpriseID: probe.EnterpriseID, Nickname: probe.Nickname,
		}
		tasks, err := c.ListGrowthTasks(a)
		if err != nil {
			t.Logf("区域 %s：拉取失败（%v）", realm, err)
			continue
		}
		codes := make([]string, 0, len(tasks))
		withReward := 0
		for _, task := range tasks {
			codes = append(codes, task.Code)
			if task.Credit > 0 {
				withReward++
			}
		}
		sort.Strings(codes)
		t.Logf("区域 %s：%d 个任务，其中 %d 个带奖励", realm, len(tasks), withReward)
		t.Logf("  任务码：%s", strings.Join(codes, ", "))
	}
}
