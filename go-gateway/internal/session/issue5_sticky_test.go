package session

import (
	"fmt"
	"testing"
	"time"
)

// issue #5 回归：hashIndex 在 n 为偶数时不得把可用账号砍半。
//
// 修复前：FNV-1a 的最低位 == 初值最低位 XOR 所有输入字节最低位（乘法因子与异或
// 都不改变最低位），因此 h 的奇偶性完全由 key 各字节的奇偶性决定；n 为偶数时
// h%n 的奇偶性 == h 的奇偶性 → 末字节为偶数的 key 只命中偶数下标账号，
// 末字节为奇数的 key 只命中奇数下标账号。实测 14 个账号只能用到 7 个。
func TestIssue5_HashIndexCoversAllAccounts(t *testing.T) {
	// 直接验证 14（偶数）下所有下标都可达。
	const n = 14
	seen := map[int]bool{}
	for i := 0; i < 3000; i++ {
		key := fmt.Sprintf("conv-%04d", i)
		seen[hashIndex(key, n)] = true
	}
	if len(seen) != n {
		t.Fatalf("hashIndex 只覆盖 %d/%d 个下标（奇偶锁定未修复）: %v", len(seen), n, seen)
	}

	// 更直接地锁定「末字节奇偶 → 下标奇偶」的相关性（旧实现下该相关性为 100%）：
	// 末字节偶数与末字节奇数的两组 key，各自命中的下标奇偶都应接近 50/50。
	// 注意必须统计一批 key 而非单个 key —— 单个 key 落成同奇偶只是正常巧合。
	evenKeys := map[int]int{} // 末字节为偶数的一组 key 的下标奇偶分布
	oddKeys := map[int]int{}  // 末字节为奇数的一组 key 的下标奇偶分布
	for i := 0; i < 2000; i++ {
		evenKeys[hashIndex(fmt.Sprintf("conv-key-%d", i*2), n)%2]++
		oddKeys[hashIndex(fmt.Sprintf("conv-key-%d", i*2+1), n)%2]++
	}
	for name, dist := range map[string]map[int]int{"末字节偶数": evenKeys, "末字节奇数": oddKeys} {
		if len(dist) != 2 {
			t.Errorf("%s 的 key 只命中单一奇偶性的下标（账号被砍半）: %v", name, dist)
			continue
		}
		// 两侧占比都应落在 30%~70%（旧实现下是 100%/0%）。
		for parity, c := range dist {
			if ratio := float64(c) / 2000; ratio < 0.3 || ratio > 0.7 {
				t.Errorf("%s 的 key 命中下标奇偶=%d 的占比 %.1f%%，分布严重偏斜: %v",
					name, parity, ratio*100, dist)
			}
		}
	}
}

// 粘性语义不得因哈希修复而破坏：同一 key 必须恒定映射到同一账号。
func TestIssue5_HashIndexIsStable(t *testing.T) {
	const n = 14
	for _, key := range []string{"conv-1", "abc", "会话键-x", "a-very-long-conversation-identifier-1234567890"} {
		first := hashIndex(key, n)
		for i := 0; i < 50; i++ {
			if got := hashIndex(key, n); got != first {
				t.Fatalf("key=%q 不稳定: %d != %d", key, got, first)
			}
		}
		if first < 0 || first >= n {
			t.Fatalf("key=%q 越界: %d ∉ [0,%d)", key, first, n)
		}
	}
}

// 验证 mix32 的雪崩性：任一输入位翻转都应让约半数输出位变化。
// 这是「消除低位相关性」的量化依据（理想均值 16/32）。
func TestIssue5_Mix32Avalanche(t *testing.T) {
	var totalBits, samples int
	for i := 0; i < 512; i++ {
		h := uint32(i) * 2654435761
		base := mix32(h)
		for b := 0; b < 32; b++ {
			flipped := mix32(h ^ (1 << uint(b)))
			diff := 0
			for x := base ^ flipped; x != 0; x >>= 1 {
				diff += int(x & 1)
			}
			totalBits += diff
			samples++
		}
	}
	avg := float64(totalBits) / float64(samples)
	t.Logf("单位翻转平均改变 %.2f/32 位（理想≈16）", avg)
	if avg < 12 || avg > 20 {
		t.Errorf("mix32 雪崩性不佳: %.2f（无法有效消除低位相关性）", avg)
	}
}

// issue #5 症状 1 的另一条路径：粘性会话分配（Resolve 的 hashIndex）是否也会
// 把大量不同会话挤到少数几个账号上。
//
// 注意：hashIndex 是**确定性** FNV-1a 取模，与 Pool.Pick 的加权随机是两套独立机制。
// 若会话键分布不均（例如 conversation_id 前缀高度相似），可能造成同样的偏斜。
func TestIssue5_StickyHashDistribution(t *testing.T) {
	const n = 14
	uids := make([]string, 0, n)
	for i := 1; i <= n; i++ {
		uids = append(uids, fmt.Sprintf("acct%02d", i))
	}
	r := routerWith(nil, uids, time.Hour)

	// 模拟大量真实会话键（conversation_id 形态：uuid 风格，前后缀有共性）。
	counts := map[string]int{}
	for i := 0; i < 3000; i++ {
		key := fmt.Sprintf("conv-%04d-4f2a-9c31-%012d", i, i)
		uid, ok := r.Resolve(key)
		if !ok {
			t.Fatalf("resolve failed for %s", key)
		}
		counts[uid]++
	}

	if len(counts) != n {
		t.Errorf("粘性路由只用到 %d/%d 个账号: %v", len(counts), n, counts)
	}
	ideal := 3000 / n
	for uid, c := range counts {
		if c < ideal/3 || c > ideal*3 {
			t.Errorf("账号 %s 分摊 %d 次，偏离理想值 %d 过多: %v", uid, c, ideal, counts)
		}
	}
}

// 验证：当绑定账号冷却后，Resolve 能否重分配（这是「长对话出错后卡死」的关键路径）。
func TestIssue5_StickyReassignsWhenBoundAccountUnavailable(t *testing.T) {
	uids := []string{"a1", "a2", "a3", "a4", "a5", "a6"}
	avail := append([]string{}, uids...)
	r := routerWith(nil, avail, time.Hour)

	// 用一个自定义 Available 以便动态移除账号。
	r.cfg.Available = func() []string { return avail }

	key := "conv-1"
	first, ok := r.Resolve(key)
	if !ok {
		t.Fatal("resolve failed")
	}
	t.Logf("首次分配: %s", first)

	// 该账号冷却 → 从可用集合移除。
	var rest []string
	for _, u := range avail {
		if u != first {
			rest = append(rest, u)
		}
	}
	avail = rest

	second, ok := r.Resolve(key)
	if !ok {
		t.Fatal("resolve failed after bound account became unavailable")
	}
	if second == first {
		t.Fatalf("绑定账号 %s 已不可用，仍被重新分配到它（长对话会持续失败）", first)
	}
	t.Logf("重分配: %s → %s", first, second)
}
