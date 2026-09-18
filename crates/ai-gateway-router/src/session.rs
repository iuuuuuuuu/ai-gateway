//! 会话粘性路由：同一会话尽量绑定同一账号。
//!
//! 对应 Go 源文件 `internal/session/session.go`。
//!
//! # 设计要点
//!
//! - 命中走读锁快查（绝大多数请求已绑定）
//! - 未命中/失效走写锁 re-check 后分配，避免同 key 并发重复分配（TOCTOU 防护）
//! - 分配优先「空闲账号」（未绑定任何会话的可用号）哈希，其次全池哈希（双段策略）
//! - `last_active` 滚动续期，TTL 过期由 GC 或快路径惰性过期清理
//!
//! # 与 Go 的差异：不做 Redis 镜像
//!
//! Go 侧每次绑定变更都 fire-and-forget 镜像到 redisstore（防重启丢粘性）。
//! 本移植版**暂不接 Redis**：粘性是尽力而为的优化（丢了只是重新分配账号，
//! 不影响正确性），而 Redis 是可选组件（未配置时为 Noop）。
//! 待 Redis 快照一并移植时再接，避免现在引入一条无法测试的异步路径。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 单条会话绑定。
#[derive(Debug, Clone)]
struct Entry {
    uid: String,
    last_active: Instant,
}

/// 可用账号来源：返回「健康且未占满在途」的有序 uid 列表。
pub type AvailableFn = Arc<dyn Fn() -> Vec<String> + Send + Sync>;

/// 会话粘性路由器。
pub struct Router {
    inner: Mutex<HashMap<String, Entry>>,
    ttl: Duration,
    available: AvailableFn,
}

impl Router {
    /// 构造路由器。`ttl` 非正时回落 30 分钟。
    pub fn new(ttl: Duration, available: AvailableFn) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            ttl: if ttl.is_zero() {
                Duration::from_secs(30 * 60)
            } else {
                ttl
            },
            available,
        }
    }

    /// 返回该会话键应绑定的账号 uid；`None` 表示当前无可用账号。
    ///
    /// 命中且账号仍可用 → 滚动续期并直接返回；否则重新分配。
    pub fn resolve(&self, key: &str) -> Option<String> {
        if key.is_empty() {
            return None;
        }
        let now = Instant::now();
        let available = (self.available)();
        let avail_set: std::collections::HashSet<&str> =
            available.iter().map(|s| s.as_str()).collect();

        // ── 快路径：命中且未过期且账号仍可用 ──────────────────
        {
            let guard = self.inner.lock().ok()?;
            if let Some(e) = guard.get(key) {
                if !expired(e, now, self.ttl) && avail_set.contains(e.uid.as_str()) {
                    let uid = e.uid.clone();
                    drop(guard);
                    self.touch(key, &uid, now);
                    return Some(uid);
                }
            }
            // 绑定号已冷却/占满/过期 → 落入慢路径重分配。
        }

        if available.is_empty() {
            return None;
        }

        // ── 慢路径：写锁 re-check 后分配 ──────────────────────
        let mut guard = self.inner.lock().ok()?;
        // re-check：并发同 key 可能已被其他线程分配好。
        if let Some(e) = guard.get(key) {
            if !expired(e, now, self.ttl) && avail_set.contains(e.uid.as_str()) {
                let uid = e.uid.clone();
                guard.insert(
                    key.to_string(),
                    Entry {
                        uid: uid.clone(),
                        last_active: now,
                    },
                );
                return Some(uid);
            }
            // 失效：清掉再分配
            guard.remove(key);
        }

        // 双段策略：优先「空闲账号」（未被任何会话绑定的可用号），其次全池。
        let mut bound: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for e in guard.values() {
            bound.insert(e.uid.as_str());
        }
        let idle: Vec<&String> = available
            .iter()
            .filter(|u| !bound.contains(u.as_str()))
            .collect();
        let pool: Vec<&String> = if idle.is_empty() {
            available.iter().collect()
        } else {
            idle
        };
        if pool.is_empty() {
            return None;
        }
        let uid = pool[hash_index(key, pool.len())].clone();
        guard.insert(
            key.to_string(),
            Entry {
                uid: uid.clone(),
                last_active: now,
            },
        );
        Some(uid)
    }

    /// 显式把会话键绑定到 uid（幂等覆盖旧值）。
    ///
    /// 供「粘性跟随最终成功号」用：请求成功返回前把会话重绑到实际成功的账号，
    /// 让多轮对话下一跳稳定收敛到对该会话持续成功的号。
    pub fn bind(&self, key: &str, uid: &str) {
        if key.is_empty() || uid.is_empty() {
            return;
        }
        if let Ok(mut guard) = self.inner.lock() {
            guard.insert(
                key.to_string(),
                Entry {
                    uid: uid.to_string(),
                    last_active: Instant::now(),
                },
            );
        }
    }

    /// 解除会话绑定（请求失败时调用，让该会话下次重新分配）。
    pub fn unbind(&self, key: &str) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.remove(key);
        }
    }

    /// 当前绑定数（供 `/status` 观测）。
    pub fn count(&self) -> usize {
        self.inner.lock().map(|g| g.len()).unwrap_or(0)
    }

    /// 清理 TTL 过期的绑定，返回清理条数。
    pub fn gc_once(&self) -> usize {
        let now = Instant::now();
        let Ok(mut guard) = self.inner.lock() else {
            return 0;
        };
        let before = guard.len();
        let ttl = self.ttl;
        guard.retain(|_, e| !expired(e, now, ttl));
        before - guard.len()
    }

    /// 滚动续期。
    fn touch(&self, key: &str, uid: &str, now: Instant) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.insert(
                key.to_string(),
                Entry {
                    uid: uid.to_string(),
                    last_active: now,
                },
            );
        }
    }
}

/// 是否已超过 TTL。
fn expired(e: &Entry, now: Instant, ttl: Duration) -> bool {
    now.duration_since(e.last_active) > ttl
}

/// 把会话键稳定散列到 `[0, n)`。
///
/// **issue #5 的根因**：直接 `h % n` 在 n 为偶数时会把账号可用数量砍半 ——
/// FNV-1a 的最低位只等于「初值最低位 XOR 所有输入字节最低位」，乘法因子与异或
/// 都不影响最低位。于是 h 的奇偶性完全由 key 各字节的奇偶性决定，n 为偶数时
/// `h % n` 的奇偶性 == h 的奇偶性，导致末字节为偶数的 key 只命中偶数下标账号 ——
/// 实测 14 个账号只有 7 个被用到。
///
/// 修复：先做一次 avalanche 混淆（murmur3 finalizer），让结果每一位都依赖输入的
/// 所有位，消除「输入低位 → 输出下标奇偶」的相关性。同一 key 仍恒定映射到同一
/// index（粘性语义不变）。
pub fn hash_index(key: &str, n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    let mut h: u32 = 2166136261;
    for b in key.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(16777619);
    }
    (mix32(h) % n as u32) as usize
}

/// 32 位 avalanche 混淆（murmur3 finalizer）。
fn mix32(mut h: u32) -> u32 {
    h ^= h >> 16;
    h = h.wrapping_mul(0x85ebca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2ae35);
    h ^= h >> 16;
    h
}

/// 从请求体提取会话键；按顺序依次尝试，找不到返回空串（**绝不失败**）。
///
/// 1. `metadata.conversation_id`
/// 2. `metadata.user_id`
/// 3. `conversation_id`
pub fn extract_key(body: &[u8]) -> String {
    if body.is_empty() {
        return String::new();
    }
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return String::new();
    };
    if let Some(meta) = v.get("metadata") {
        if let Some(s) = meta.get("conversation_id").and_then(|x| x.as_str()) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
        if let Some(s) = meta.get("user_id").and_then(|x| x.as_str()) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    v.get("conversation_id")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}

/// 把任意种子串折叠成一个稳定的会话键。
///
/// 用途：Anthropic Messages / Responses 等协议没有 `conversation_id`，
/// 适配层用「首条 user 文本 + system 摘要」当种子，这里做 FNV-1a 折叠，
/// 得到与 OpenAI 侧 `conversation_id` 等价的短键。
///
/// 相同种子必然得到相同键，这是粘性路由成立的前提。
pub fn session_key_from_seed(seed: &str) -> String {
    if seed.is_empty() {
        return String::new();
    }
    const OFFSET64: u64 = 14695981039346656037;
    const PRIME64: u64 = 1099511628211;
    let mut hash: u64 = OFFSET64;
    for b in seed.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(PRIME64);
    }
    format!("seed-{hash:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn router_with(uids: Vec<String>) -> Router {
        let list = Arc::new(uids);
        Router::new(
            Duration::from_secs(1800),
            Arc::new(move || (*list).clone()),
        )
    }

    #[test]
    fn extract_key_prefers_metadata_conversation_id() {
        let body = br#"{"metadata":{"conversation_id":"c1","user_id":"u1"},"conversation_id":"c2"}"#;
        assert_eq!(extract_key(body), "c1");
    }

    #[test]
    fn extract_key_falls_back_in_order() {
        let body = br#"{"metadata":{"user_id":"u1"},"conversation_id":"c2"}"#;
        assert_eq!(extract_key(body), "u1");
        let body = br#"{"conversation_id":"c2"}"#;
        assert_eq!(extract_key(body), "c2");
    }

    #[test]
    fn extract_key_never_fails() {
        assert_eq!(extract_key(b""), "");
        assert_eq!(extract_key(b"not json"), "");
        assert_eq!(extract_key(br#"{"metadata":{"conversation_id":123}}"#), "");
    }

    #[test]
    fn resolve_is_sticky_for_same_key() {
        let r = router_with(vec!["a".into(), "b".into(), "c".into()]);
        let first = r.resolve("conv-1").unwrap();
        for _ in 0..10 {
            assert_eq!(r.resolve("conv-1").unwrap(), first, "同 key 必须稳定");
        }
    }

    #[test]
    fn resolve_none_when_pool_empty() {
        let r = router_with(vec![]);
        assert!(r.resolve("conv-1").is_none());
    }

    /// 绑定号从可用集合消失后应重新分配，而不是继续返回它。
    #[test]
    fn resolve_reallocates_when_bound_account_unavailable() {
        let list = Arc::new(Mutex::new(vec!["a".to_string(), "b".to_string()]));
        let snapshot = Arc::clone(&list);
        let r = Router::new(
            Duration::from_secs(1800),
            Arc::new(move || snapshot.lock().unwrap().clone()),
        );
        let first = r.resolve("conv-1").unwrap();
        // 移除所有账号再恢复其中一个，确保不会返回已不可用的那个
        list.lock().unwrap().clear();
        list.lock().unwrap().push("b".to_string());
        let second = r.resolve("conv-1").unwrap();
        assert_eq!(second, "b", "绑定号不可用时应改绑可用号");
        let _ = first;
    }

    /// `resolve` 只认可用账号上的绑定：绑到一个不可用的 uid 时应改绑可用号，
    /// 因此这里绑定池里真实存在的 "a"。
    #[test]
    fn bind_and_unbind() {
        let r = router_with(vec!["a".into()]);
        r.bind("k", "a");
        assert_eq!(r.count(), 1);
        assert_eq!(r.resolve("k").unwrap(), "a");
        r.unbind("k");
        assert_eq!(r.count(), 0);
    }

    /// 绑定到不可用账号 → 快路径判定失效，重新分配可用号。
    #[test]
    fn bind_to_unavailable_uid_reallocates() {
        let r = router_with(vec!["a".into()]);
        r.bind("k", "gone");
        assert_eq!(r.resolve("k").unwrap(), "a", "应改绑可用号");
    }

    #[test]
    fn gc_removes_expired() {
        let r = Router::new(
            Duration::from_millis(1),
            Arc::new(|| vec!["a".to_string()]),
        );
        r.bind("k", "a");
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(r.gc_once(), 1);
        assert_eq!(r.count(), 0);
    }

    /// issue #5 回归：偶数个账号时，不同 key 必须能覆盖到全部下标。
    ///
    /// 修复前 FNV 低位相关性会让「末字节奇偶」决定下标奇偶，
    /// 14 个账号只有 7 个被用到。
    #[test]
    fn hash_index_covers_all_buckets_with_even_n() {
        let n = 14usize;
        let mut seen = std::collections::HashSet::new();
        // 用大量 key 覆盖，模拟真实会话键分布
        for i in 0..2000 {
            seen.insert(hash_index(&format!("conv-{i}"), n));
        }
        assert_eq!(seen.len(), n, "14 个桶应全部被覆盖，实际 {}", seen.len());
    }

    /// 同一 key 必须恒定映射到同一 index（粘性语义的前提）。
    #[test]
    fn hash_index_is_deterministic() {
        for key in ["a", "conv-1", "uuid-0", "c0", "c1"] {
            let first = hash_index(key, 14);
            for _ in 0..5 {
                assert_eq!(hash_index(key, 14), first, "key={key}");
            }
        }
    }

    /// 修复前会失败的具体形态：末字节偶数的 key 不应只落在偶数下标。
    #[test]
    fn hash_index_low_bit_not_correlated_with_key_parity() {
        let n = 14usize;
        let mut even_idx_from_even_key = 0;
        let mut total = 0;
        for i in 0..500 {
            let key = format!("uuid-{i}0"); // 末字节恒为偶数 '0'
            let idx = hash_index(&key, n);
            if idx % 2 == 0 {
                even_idx_from_even_key += 1;
            }
            total += 1;
        }
        // 若低位相关，这里会是 100%；修复后应在 50% 附近（宽松断言：不可能是全部）
        assert!(
            even_idx_from_even_key < total,
            "末字节偶数的 key 不应全部落在偶数下标（低位相关性回归）"
        );
    }

    #[test]
    fn session_key_from_seed_is_stable() {
        let a = session_key_from_seed("hello");
        let b = session_key_from_seed("hello");
        assert_eq!(a, b);
        assert!(a.starts_with("seed-"));
        assert_ne!(a, session_key_from_seed("hello2"));
        assert_eq!(session_key_from_seed(""), "");
    }
}
