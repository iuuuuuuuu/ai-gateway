package upstream

import (
	"net/http"
	"testing"
	"time"
)

// TestTransportSkeletonKeepsConnectionsAlive 钉住连接池的三个关键取值。
//
// # 为什么值得单独钉
//
// 这三项**不会让任何测试变红**，坏了也不报错 —— 表现只是「用起来变卡」：
// 每次请求重付一次 TLS 握手（跨境链路实测 ~0.8 秒）。属于典型的
// 「静默性能退化」，只能靠断言守住。
//
// 实测依据（2026-09-22，走代理打 www.workbuddy.ai）：
//
//	新建连接  TLS 0.78s  总 2.08s
//	复用连接  TLS 0     总 1.15s
func TestTransportSkeletonKeepsConnectionsAlive(t *testing.T) {
	tr := newTransportSkeleton()
	if tr == nil {
		t.Fatal("newTransportSkeleton 不该返回 nil")
	}

	// ---- 1. 空闲连接要留得够久 ----
	//
	// 原来 90 秒。「想一会儿再问」是对话常态，90 秒太短 ——
	// 用户停顿一分半，下一条消息就要重握手。
	//
	// 这里断言 ≥3 分钟而不是写死 5 分钟：值是调优参数，可以再调，
	// 但不能退回「分钟级以下」那个会明显影响体验的区间。
	if tr.IdleConnTimeout < 3*time.Minute {
		t.Errorf("IdleConnTimeout 应 ≥3 分钟（实测 90 秒会导致停顿后重握手），实际 %v",
			tr.IdleConnTimeout)
	}

	// ---- 2. 必须显式开启 HTTP/2 ----
	//
	// ⚠ 这是最容易在重构中丢掉的一项：Go 在自定义 Transport 时
	// 默认**关掉** h2，而「忘了写这一行」不会有任何报错 ——
	// 只是悄悄退化成 HTTP/1.1，失去多路复用。
	if !tr.ForceAttemptHTTP2 {
		t.Error("必须设 ForceAttemptHTTP2=true：" +
			"自定义 Transport 时 Go 默认关闭 h2，会让连续对话退化成 HTTP/1.1 串行")
	}

	// ---- 3. 连接池容量 ----
	//
	// 19 个账号可能同时打不同的 host（国服/国际版/各产品），
	// MaxIdleConns 太小会把刚建好的连接挤掉。
	if tr.MaxIdleConns < 20 {
		t.Errorf("MaxIdleConns 太小（%d），会被多账号并发挤掉空闲连接", tr.MaxIdleConns)
	}
	if tr.MaxIdleConnsPerHost < 4 {
		t.Errorf("MaxIdleConnsPerHost 太小（%d），同一 host 并发时会频繁重建连接",
			tr.MaxIdleConnsPerHost)
	}

	// ---- 4. 三条路径必须共用同一份参数 ----
	//
	// 注释里写着「抽出来是为了只差 Proxy 一个字段」，那就必须真的只差它。
	// 否则某一路会静默退回默认值（表现为「只有走代理那一挂会卡」）。
	env := newTransport(nil)
	direct := newDirectTransport()
	if env.IdleConnTimeout != tr.IdleConnTimeout || direct.IdleConnTimeout != tr.IdleConnTimeout {
		t.Error("三条路径的 IdleConnTimeout 必须一致（否则只有某一路会重握手）")
	}
	if !env.ForceAttemptHTTP2 || !direct.ForceAttemptHTTP2 {
		t.Error("三条路径都必须开 ForceAttemptHTTP2")
	}
}

// TestTransportSkeletonHasNoKeepAliveDisable 反向断言：不得有人图省事关掉复用。
//
// `DisableKeepAlives = true` 会让**每个请求**都新建连接 ——
// 在这条跨境链路上等于每次都多付 ~0.8 秒。它有时被用来"规避连接复用
// 导致的串号/串扰问题"，但在本仓库的用法下没有这个需要
//（每个请求都带自己的 Authorization，连接本身不携带身份状态）。
func TestTransportSkeletonHasNoKeepAliveDisable(t *testing.T) {
	for name, tr := range map[string]*http.Transport{
		"skeleton": newTransportSkeleton(),
		"env":      newTransport(nil),
		"direct":   newDirectTransport(),
	} {
		if tr.DisableKeepAlives {
			t.Errorf("%s：DisableKeepAlives 不该打开 —— 每个请求重付一次跨境握手（~0.8s）", name)
		}
	}
}
