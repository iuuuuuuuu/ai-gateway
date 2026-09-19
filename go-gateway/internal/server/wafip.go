package server

// wafip.go 「WAF 403 拦的是出口 IP，不是账号」的识别与判定。
//
// # 要解决的问题（真实缺陷）
//
// 此前网关对 403 只有**账号级**软冷却，没有「这可能是出口 IP 被拦」的判断。
// 后果：所有号轮一遍全 403，而网关还在继续换号，把请求放大 MaxRotate 倍 ——
// 恰好是 WAF 最想惩罚的行为，于是封禁被越打越重。
//
// 上游 Sliverkiss/workbuddy2api 的实测记录：
//
//	WAF 403 拦的是网关出口 IP 而非账号 —— 3 个账号 1 秒内全 403
//
// 这正是本模块要捕捉的特征：**同一账号**反复 403 是账号问题（该罚该换），
// 而**多个不同账号**在短时间内一起 403，只能是它们共同的东西出了问题 ——
// 共用的出口 IP。账号本身是好的。
//
// # 为什么阈值是「不同账号数」而不是「403 次数」
//
// 单个坏账号在一分钟内可以轻易打出几十次 403。若按次数判定，一个账号就足以
// 让我们误判「整个出口 IP 被拦」，从而把好账号的请求也一起中止掉。
// 按**不同账号**计数则要求证据来自多个独立凭证，这才排除了账号自身的因素。
//
// # 为什么是滑动窗口而不是累计计数
//
// 出口 IP 被拦是**临时**状态（几分钟到几十分钟）。累计计数一旦过线就永久成立，
// 之后哪怕 IP 早已解封，网关也会一直拒绝换号。滑动窗口让判定自动过期。

import (
	"encoding/json"
	"net/http"
	"strconv"
	"strings"
	"sync"
	"time"
)

const (
	// wafIPWindow 滑动窗口长度：只看最近这段时间内的 403。
	//
	// 60 秒的依据：一次请求的整个轮转过程（含退避）在秒级完成，
	// 同一个出口 IP 上「多个账号几乎同时 403」这一特征只在这个尺度内成立。
	// 窗口再长会把不相关的偶发 403 凑成一次误判。
	wafIPWindow = 60 * time.Second

	// wafIPThreshold 窗口内**不同账号**吃到 WAF 403 的数量达到它即判定 IP 被拦。
	//
	// 取 2：一个账号的 403 无法区分「账号问题」与「IP 问题」，两个不同账号
	// 同时 403 才能排除账号自身因素。这也是上游用的口径。
	wafIPThreshold = 2
)

// wafIPState 出口 IP 级 WAF 403 的滑动窗口状态。
//
// 并发安全（sync.Mutex）：同一进程内多个请求会同时轮转，各自独立地 403，
// 正是它们的**并集**才能说明 IP 被拦 —— 故这份状态必须是进程级共享的。
// 这也意味着它**不该**挂在 Handler 上：出口 IP 是整个进程的属性，
// 不是某个 handler 实例的属性。
type wafIPState struct {
	mu        sync.Mutex
	window    time.Duration
	threshold int
	// now 时钟注入点；生产用 time.Now，单测注入假时钟以断言「窗口过期」。
	now func() time.Time
	// hits uid → 该账号**最近一次** WAF 403 的时刻。
	//
	// 用 map 而不是切片：map 的 key 天然去重，「不同账号数」就等于 len(hits)，
	// 不必再写一遍去重逻辑（那份逻辑最容易在「同一账号多次 403」上出错）。
	hits map[string]time.Time
}

// newWafIPState 构造一个窗口状态（window <= 0 / threshold <= 0 时取默认值）。
func newWafIPState(window time.Duration, threshold int) *wafIPState {
	if window <= 0 {
		window = wafIPWindow
	}
	if threshold <= 0 {
		threshold = wafIPThreshold
	}
	return &wafIPState{
		window:    window,
		threshold: threshold,
		now:       time.Now,
		hits:      map[string]time.Time{},
	}
}

// wafIP 进程级共享的出口 IP 判定状态。
//
// 包级变量而不是 Handler 字段：出口 IP 属于**整个进程**（同一个网关进程
// 只有一个出口），跨请求、跨 handler 实例共享才是正确语义。
// 本包已有同类先例（capability.go 的 regionModelCache）。
var wafIP = newWafIPState(wafIPWindow, wafIPThreshold)

// noteWaf 记录「账号 uid 刚吃到一次 WAF 403」，返回**是否已判定为出口 IP 被拦**。
//
// 返回 true 的语义是：窗口内已有 >= threshold 个**不同**账号吃到 WAF 403，
// 调用方应立即中止轮转 —— 继续换号只会让 WAF 看到更多账号被打，放大风控。
//
// ⚠ 一旦跨过阈值，在窗口内的**后续调用也会返回 true**（包括那第一个账号
// 再次 403 时）。这是刻意的：判定结果在窗口内持续有效，正是它阻止了
// 「判定出来了但下一个请求又从 0 开始轮转」这种放大。窗口过后自动恢复。
//
// 同一账号重复 403 **只刷新它自己的时间戳**，不会增加不同账号数 ——
// 那是账号问题，不该被算成 IP 问题。
func (s *wafIPState) noteWaf(uid string) bool {
	now := s.now()
	s.mu.Lock()
	defer s.mu.Unlock()
	// 先清理过期条目：内存不会只增不减。
	//
	// 为什么用「顺手清理」而不是起一个后台定时器：条目上限本来就是账号总数
	//（key 是 uid，天然去重），量级极小；为它维护一个 goroutine 的生命周期
	//（启动 / 退出 / 测试里的泄漏）得不偿失。每次 noteWaf 都会清一次，
	// 而 noteWaf 正是唯一写入路径 —— 不存在「写了很多次却没清」的窗口。
	s.evictLocked(now)
	s.hits[uid] = now
	return len(s.hits) >= s.threshold
}

// distinct 返回窗口内吃到过 WAF 403 的**不同账号**数（顺带清理过期条目）。
// 仅用于把数量写进给用户看的错误文案。
func (s *wafIPState) distinct() int {
	now := s.now()
	s.mu.Lock()
	defer s.mu.Unlock()
	s.evictLocked(now)
	return len(s.hits)
}

// reset 清空窗口（仅供单测隔离用例之间的状态）。
func (s *wafIPState) reset() {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.hits = map[string]time.Time{}
}

// evictLocked 丢弃窗口外的条目。调用方须持有 s.mu。
func (s *wafIPState) evictLocked(now time.Time) {
	for uid, at := range s.hits {
		if now.Sub(at) >= s.window {
			delete(s.hits, uid)
		}
	}
}

// wafForbiddenMarkers 拦截页 / 边缘节点的**强特征**（小写比较）。
//
// ⚠ 这里**刻意不收敛**的泛词：`forbidden` / `denied` / 单独的「拦截」在
// 账号被封的业务文案里同样常见（"your account has been blocked"、
// 「账号已被拦截」）。收进来会把账号问题误判成 IP 问题 ——
// 那正好是本模块要区分开的两件事。故只登记**只可能出自边缘节点**的特征。
//
// 这份列表只在「响应体看起来是业务信封」时才需要发力（见 isWafForbidden）：
// 非信封的 403 本来就已判定为 WAF。它要覆盖的真实形态是边缘节点返回的
// JSON，例如：
//
//	403 {"code":403,"msg":"Request blocked by WAF"}
//
// 这条看着像信封（有 code），但说的是 WAF —— 靠列表才能识别出来。
var wafForbiddenMarkers = []string{
	"cloudflare",
	"<html",
	"<!doctype html",
	"安全策略",
	"异常流量",
	"访问被拦截",
	"请求被拦截",
}

// isWafForbidden 报告一次 403 是否**像是边缘 WAF 的拦截**（而非上游业务拒绝）。
//
// 判据（任一成立）：
//
//  1. 响应体**不是**上游的业务 JSON 信封（主判据，见下）；
//  2. 响应体含拦截页强特征（见 wafForbiddenMarkers）。
//
// 为什么第 1 条是主判据：上游业务拒绝（如 ZCode 的
// `403 {"code":3101,"msg":"coding plan is required"}`）一律是
// `{code, msg, data}` 形状的信封，能解析出业务码与文案 —— 那说明请求
// **已经到达应用层**，被拦的与该账号/该请求有关，与出口 IP 无关。
// 反过来，拿不到业务信封的 403（HTML 拦截页、纯文本、空体）说明请求
// 在应用层之前就被挡下了，那才是出口 IP 层面的问题。
//
// # 判据的**顺序**很关键（防误判）
//
// 先看信封、再看特征，而不是反过来。若先扫特征，`{"code":1,"msg":"wafer
// quota exceeded"}` 这种正文里恰好含 "waf" 子串的业务响应就会被误判成
// IP 问题，让整个网关停止轮转。反过来「先判信封、再让特征覆盖」则两边都安全：
// 业务信封默认按业务处理，只有真的带边缘节点特征时才改判。
//
// ⚠ 空体 403 也算「像 WAF」：上游应用层的拒绝从不空体，空体只出现在
// 请求被边缘节点直接掐断时。这一条是判断而非事实，故最终给用户的文案
// 用的是「**可能**被拦」，且只有跨过「2 个不同账号」的阈值才会触发。
func isWafForbidden(status int, body string) bool {
	if status != http.StatusForbidden {
		return false
	}
	if !isBusinessEnvelope(body) {
		return true
	}
	// 是业务信封：只有带边缘节点特征时才改判为 WAF。
	lower := strings.ToLower(body)
	for _, m := range wafForbiddenMarkers {
		if strings.Contains(lower, m) {
			return true
		}
	}
	// "waf" 单独成一个词才算（"Request blocked by WAF" 命中，
	// 而 "wafer" / "dwarf" 这类只是含子串的单词不命中）。
	return containsWord(lower, "waf")
}

// containsWord 报告 s 中是否**以独立单词**的形式出现 word（s 须已小写）。
//
// 用边界判定而不是裸 strings.Contains：后者会把 "wafer"、"dwarf" 也当成命中，
// 而本函数的调用点在 403 判定路径上 —— 一次误判就会让整个网关停止轮转，
// 代价远大于多写这几行。
func containsWord(s, word string) bool {
	for i := 0; ; {
		j := strings.Index(s[i:], word)
		if j < 0 {
			return false
		}
		j += i
		if !isWordByte(byteAt(s, j-1)) && !isWordByte(byteAt(s, j+len(word))) {
			return true
		}
		i = j + 1
		if i >= len(s) {
			return false
		}
	}
}

// byteAt 取 s[i]，越界返回 0（调用方用它表示「没有相邻字符」= 边界）。
func byteAt(s string, i int) byte {
	if i < 0 || i >= len(s) {
		return 0
	}
	return s[i]
}

// isWordByte 报告 b 是否为「ASCII 单词字符」（字母/数字/下划线）。
//
// ⚠ 非 ASCII 一律返回 false（= 视为边界），这个方向是刻意的：
//
//   - 中文等文字**不用空格分词**，"WAF拦截" 里的 WAF 必须能被认出来，
//     把汉字当单词字符会让它漏判；
//   - 而需要防的误判（"wafer" / "dwarf"）其相邻字符必然是 ASCII 字母，
//     与本分支无关。
//
// 两个方向的收益不对称：漏判只是回到改动前的行为（继续轮转），
// 误判会让整个网关停止轮转。
func isWordByte(b byte) bool {
	switch {
	case b >= 'a' && b <= 'z', b >= 'A' && b <= 'Z', b >= '0' && b <= '9', b == '_':
		return true
	default:
		return false
	}
}

// isBusinessEnvelope 报告响应体是否为上游的**业务 JSON 信封**。
//
// 认两种形状（网关上游实际用到的全部）：
//
//	{"code":3101,"msg":"coding plan is required","data":...}   上游统一信封
//	{"error":{"code":...,"message":...,"type":...}}             OpenAI 形状
//
// 空体、非 JSON、JSON 数组/标量都返回 false（= 不是业务信封）。
func isBusinessEnvelope(body string) bool {
	t := strings.TrimSpace(body)
	if t == "" || !strings.HasPrefix(t, "{") {
		return false
	}
	var doc map[string]json.RawMessage
	if err := json.Unmarshal([]byte(t), &doc); err != nil {
		return false
	}
	// 上游统一信封：顶层有 code 即认定（msg 允许缺失）。
	if _, ok := doc["code"]; ok {
		return true
	}
	// OpenAI 形状：error 是**对象**且带任一业务字段。
	// error 为字符串（如 {"error":"Forbidden"}）不算 —— 那是边缘节点的写法。
	if raw, ok := doc["error"]; ok {
		var e map[string]json.RawMessage
		if json.Unmarshal(raw, &e) == nil {
			for _, k := range []string{"code", "message", "msg", "type", "data"} {
				if _, ok := e[k]; ok {
					return true
				}
			}
		}
	}
	return false
}

// egressIPBlockedMessage 生成「出口 IP 可能被 WAF 拦截」的客户端文案。
//
// 必须同时说清三件事，缺一件用户就会走错排查方向：
//
//	现象   —— 多个不同账号在短时间内一起 403
//	结论   —— 问题在出口 IP，不在账号（所以**换号无用**）
//	出路   —— 查本机网络出口（代理 / VPN / 公网 IP）后重试
//
// 刻意用「可能」而不是断定：网关能看到的是「多个账号同时 403」这个事实，
// 至于它是 WAF 拦 IP、还是上游整体故障，从响应体上无法完全区分。
func egressIPBlockedMessage(distinct int) string {
	return "上游疑似 WAF 拦截：最近 " + strconv.Itoa(int(wafIPWindow/time.Second)) +
		" 秒内已有 " + strconv.Itoa(distinct) +
		" 个**不同账号**收到 403，被拦的可能是本机出口 IP 而非账号" +
		"（同一账号反复 403 才是账号问题）。继续换号无用，已中止本次轮转以免放大风控。" +
		"请检查本机网络出口（代理 / VPN / 公网 IP）后重试。"
}
