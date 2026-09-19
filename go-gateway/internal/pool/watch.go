// watch.go auths 目录热加载：目录内容变化时自动重新对齐账号池，
// 免去「新增凭证后必须重启网关」。
//
// # 为什么需要
//
// SyncToDir 此前只在进程启动时被调用一次（cmd/server/main.go）：运行中新增的
// 凭证文件**不会进池**，表现为宿主界面显示「已添加」而网关侧账号「未加载」，
// 必须手动点「重启」才生效。宿主代码里也把这条限制写成了注释
//（crates/ai-gateway-core/src/modules/gateway.rs 的 switch_mode：
// 「网关的账号池是启动时扫描 gateway_auths/ 建立的……必须手动点重启才真正生效」）。
//
// # 实现选择：轮询而非 fsnotify
//
//   - 零新依赖：本仓库依赖面刻意保持极窄（仅 go-redis + golang.org/x/sys），
//     为一个低频人工操作引第三方库不划算。
//   - 语义更稳：fsnotify 在容器 / 网络文件系统上有丢事件与 inotify 句柄耗尽的老问题；
//     轮询只看目录内容指纹，漏不掉，也不会因事件风暴抖动。
//   - 代价可接受：目录里只有几十个凭证文件，每轮只做一次 ReadDir + 名字比对，
//     默认 5s 周期下开销可忽略。
//
// # 幂等性
//
// 依赖 SyncToDir 的既有语义：upsertLocked 对已存在账号只换凭证、保留
// credits / cooling / 熔断 / 统计。因此「指纹没变就不重新加载」与
//「即使加载也不重置任何运行态」这两层都成立（有单测钉住，见 watch_test.go）。
package pool

import (
	"log"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"time"

	"workbuddy2api/internal/auth"
)

// watchInterval 目录轮询周期。默认 5s，可经配置项 pool.watch_interval 覆盖
//（见 cmd/server/config.go 与 StartAuthDirWatchWithInterval）。
//
// 取 5s 的理由：与池状态落盘周期（flushInterval）同量级，而加账号是低频人工操作，
// 5s 内生效足够；再短只是徒增 IO。
var watchInterval = 5 * time.Second

// StartAuthDirWatch 以默认周期启动 auths 目录监听。
// 等价于 StartAuthDirWatchWithInterval(dir, 0)。
func (p *Pool) StartAuthDirWatch(dir string) (stop func()) {
	return p.StartAuthDirWatchWithInterval(dir, 0)
}

// StartAuthDirWatchWithInterval 启动 auths 目录监听，返回停止函数。
//
// interval <= 0 时用 watchInterval（默认 5s）。停止函数**幂等**：重复调用不 panic
// —— defer stop() 与显式调用撞在一起是常见写法，不能在那里炸掉进程。
//
// 首次调用即建立基线指纹，**不触发同步**：启动路径已经 SyncToDir 过一次
//（cmd/server/main.go），此处再同步纯属重复工作。
//
// dir 为空或不可读时只记一条日志并返回 no-op 停止函数：监听失败不该拖垮网关，
// 而且「手动重启」这条既有退路仍然可用。
//
// ⚠ 剔除范围见 reloadAuthDir 的 keepWorkBuddyOnly —— 池是三个产品共用的，
// 运行期热加载只扫 WorkBuddy 的 auths 目录，剔除时必须放过另外两个产品的账号。
func (p *Pool) StartAuthDirWatchWithInterval(dir string, interval time.Duration) (stop func()) {
	if dir == "" {
		log.Printf("[watch] auths 目录未配置，跳过热加载监听")
		return func() {}
	}
	if interval <= 0 {
		interval = watchInterval
	}
	base, ok := dirFingerprint(dir)
	if !ok {
		log.Printf("[watch] auths 目录 %s 不可读，跳过热加载监听（加账号后需手动重启）", dir)
		return func() {}
	}
	log.Printf("[watch] auths 目录监听已启用（每 %s 检查一次，新增账号自动加载，无需重启）", interval)

	done := make(chan struct{})
	stopped := false
	go func() {
		t := time.NewTicker(interval)
		defer t.Stop()
		last := base
		for {
			select {
			case <-done:
				return
			case <-t.C:
				cur, ok := dirFingerprint(dir)
				// 目录暂不可读（如正在原子替换）**不当作变化**：把它当成「所有文件
				// 都被删了」会误剔除账号。宁可这轮不处理，下一轮再比。
				if !ok || cur == last {
					continue
				}
				last = cur
				p.reloadAuthDir(dir)
			}
		}
	}()

	return func() {
		if stopped {
			return
		}
		stopped = true
		close(done)
	}
}

// reloadAuthDir 重新扫描目录并对齐账号池。
//
// 全量重扫而非增量：目录只有几十个文件，全扫的代价远低于维护增量状态
//（增量要处理「文件改名」「写了一半」等边界）。auth.LoadDir 自身跳过解析失败的
// 文件，故半写入的临时文件不会造成误判 —— 而宿主两处导出都是「先写临时文件再
// rename」的原子替换（Go 侧 auth.SaveAtomic / Rust 侧 atomic_write），
// 因此读取方几乎不可能看到半个 JSON。
func (p *Pool) reloadAuthDir(dir string) {
	auths, err := auth.LoadDir(dir)
	if err != nil {
		log.Printf("WARN: [watch] 重新加载 %s 失败: %v（保持现有池状态）", dir, err)
		return
	}
	before := len(p.AllUIDs())
	p.mu.Lock()
	// keep 是**保守的**（宁可留着也不误删），逐条理由见 keepWorkBuddyOnly。
	p.syncToDirLocked(auths, keepWorkBuddyOnly(dir))
	p.mu.Unlock()
	after := len(p.AllUIDs())
	available := len(p.AvailableUIDs())
	if after != before {
		log.Printf("[watch] auths 目录变化：账号数 %d → %d（已热加载，无需重启；当前可选 %d）",
			before, after, available)
	} else {
		// 数量不变但内容变了（凭证刷新 / 文件改名）：仍要落一条，便于对账。
		log.Printf("[watch] auths 目录变化：账号数保持 %d（已热加载凭证更新；当前可选 %d）",
			after, available)
	}
}

// keepWorkBuddyOnly 返回 syncToDirLocked 的保留谓词：
// 「池里存在、但本次 dir 扫描未见」的账号是否应**保留而不剔除**。
//
// # 为什么必须有这个口子（本项目的真实约束，不是防御性编程）
//
// SyncToDir 的剔除是**按 uid 全量比对**的：没交出去的账号一律当成「凭证文件已删除」
// 删掉。启动路径满足这个前提 —— main.go 先 SyncToDir(WorkBuddy auths)，之后才用
// p.Add 塞入 Qoder / ZCode 账号（见 cmd/server/main.go 的多产品路由段）。
// 但**运行期热加载不满足**：它只扫 WorkBuddy 的 auths 目录，而池里同时混着
// Qoder / ZCode 账号。若这里直接调 SyncToDir，一次热加载就会把另外两个平台的
// 账号全部删出池子 —— 症状是「往 WorkBuddy 加了个账号，Qoder 和 ZCode 的账号
// 全不见了」，且要重启才回来。
//
// # 判定规则（两条，都指向「保留」）
//
//  1. 非 WorkBuddy 产品：本目录根本不管它们。
//  2. 凭证来源不在本目录下（FilePath 为空、或指向别处）：本目录看不到它
//     「被删了」，据此剔除纯属越权。FilePath 为空只可能是被 p.Add 直接塞进来的
//     （多产品路径 / 单测）。
//
// 只有「FilePath 就在 dir 下、而这次扫描没扫到」才算真的被删除 —— 那种情况下
// 文件确实没了，剔除是对的（防止已删账号滞留，见 SyncToDir 的注释）。
//
// 目录比较用 filepath.Clean 归一而非字符串直接比较：配置里可能写成 `./auths/`，
// 而 LoadDir 用 Glob 拼出的路径在 Windows 上是 `auths\workbuddy-x.json`。
func keepWorkBuddyOnly(dir string) func(*entry) bool {
	want := filepath.Clean(dir)
	return func(e *entry) bool {
		if e.a == nil || e.a.ProductOf() != auth.ProductWorkBuddy {
			return true // 另外两个产品的账号不归本目录管
		}
		if e.a.FilePath == "" {
			return true // 无凭证文件来源（p.Add 直接塞入）
		}
		return filepath.Clean(filepath.Dir(e.a.FilePath)) != want
	}
}

// dirFingerprint 生成目录内容指纹：文件名 + 修改时间 + 大小，排序后拼接。
//
// 为什么不用纯文件名：凭证刷新（宿主覆盖同一个 uid 的文件）时文件名不变，
// 只比名字会漏掉这类更新。mtime + size 能覆盖「凭证被刷新」与「文件被替换」。
//
// **刻意不读文件内容**：那是账号秘密（access / refresh token），让一个周期性扫描
// 反复读取密钥只会扩大暴露面，而且读内容慢得多。mtime + size 足以判定变化。
//
// 返回 (指纹, 目录是否可读)。**必须用独立的 ok，而不是「空串 = 不可读」**：
// 空目录（凭证被全部删掉）的合法指纹就是空串，若与「不可读」共用哨兵，
// 清空目录会被误判成读失败而跳过同步，已删账号就永远滞留在池里。
func dirFingerprint(dir string) (string, bool) {
	ents, err := os.ReadDir(dir)
	if err != nil {
		return "", false
	}
	parts := make([]string, 0, len(ents))
	for _, e := range ents {
		if e.IsDir() || !strings.HasSuffix(e.Name(), ".json") {
			continue
		}
		info, err := e.Info()
		if err != nil {
			continue
		}
		parts = append(parts, e.Name()+"|"+info.ModTime().Format(time.RFC3339Nano)+"|"+
			strconv.FormatInt(info.Size(), 10))
	}
	sort.Strings(parts)
	return strings.Join(parts, "\n"), true
}
