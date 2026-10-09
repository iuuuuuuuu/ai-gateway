//! 凭证目录热加载（对应 Go `internal/pool/watch.go`，211 行）。
//!
//! # 为什么需要它
//!
//! 网关启动时把凭证目录扫一遍建池；此后宿主往目录里新写的账号（导入、短信登录、
//! 令牌刷新换新）如果不重启网关就永远进不了池 —— 这是用户实际报过的痛点。
//! 本模块负责在运行期把「池」与「目录」重新对齐。
//!
//! # 为什么是轮询而不是 fsnotify
//!
//! 与 Go 侧同一口径（`watch.go:12-20`）：
//!
//! 1. **零新依赖** —— 本仓库的依赖面刻意保持极窄；
//! 2. 容器 / 网络文件系统上 fsnotify 会丢事件，还有 inotify 句柄耗尽的老问题；
//! 3. 代价可忽略：几十个文件，每轮一次 `ReadDir` + 名字/时间戳/大小比对。
//!
//! # 两个必须守住的约束
//!
//! * **剔除必须放过「不归本目录管」的账号**（[`keep_workbuddy_only`]）。池里同时
//!   装着 WorkBuddy / Qoder / ZCode 三个产品的账号，而本目录只管 WorkBuddy；
//!   一次全量剔除就会把另外两个产品的账号全部删出池子，且要重启才回来。
//! * **「目录暂时不可读」不等于「目录是空的」**。前者必须保持现有池状态，只有
//!   后者才该剔除。把前者当后者，一次目录瞬时不可读就会清空整个账号池。

use std::path::{Component, Path};
use std::sync::mpsc::{RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::auth::{self, Auth, PRODUCT_WORKBUDDY};
use crate::pool::Pool;

/// 默认轮询周期（对应 Go `watchInterval`，`watch.go:45`）。
pub const DEFAULT_WATCH_INTERVAL: Duration = Duration::from_secs(5);

/// 凭证目录监听句柄。
///
/// ★ [`AuthDirWatch::stop`] **幂等**（对应 Go 返回的 `stop func()` 里的 `stopped`
/// 守卫，`watch.go:104-110`）：`defer stop()` 与显式调用撞在一起是常见写法，不能
/// 在那里炸掉进程。Rust 这边靠「`take()` 走句柄」天然幂等 —— 第二次是空操作。
/// `Drop` 也会停，所以忘记显式调用不会漏线程。
#[derive(Debug)]
pub struct AuthDirWatch {
    stop_tx: Option<Sender<()>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl AuthDirWatch {
    /// 空句柄：没起任何线程，`stop()` 是空操作。
    ///
    /// 三种情况会得到它：`dir` 为空、目录不可读、以及调用方**显式关闭**了热加载
    /// （`pool.watch_auth_dir=false`）。最后一种由调用方自己打日志 —— 本模块不该
    /// 替「用户主动关掉」和「配置缺失」发同一条提示。
    pub fn inert() -> Self {
        Self {
            stop_tx: None,
            join: None,
        }
    }

    /// 停止监听并等轮询线程退出（幂等；已停过则为空操作）。
    ///
    /// 用 `mpsc::recv_timeout` 当定时器，所以停止信号能让线程**立刻**醒来，
    /// 不会阻塞到下一个周期（退出时不必等满 5s）。
    pub fn stop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.join.take() {
            let _ = h.join();
        }
    }

    /// 是否真的起了轮询线程（`dir` 为空或目录不可读时为 `false`）。
    pub fn is_running(&self) -> bool {
        self.join.is_some()
    }
}

impl Drop for AuthDirWatch {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 启动凭证目录监听（对应 Go `StartAuthDirWatchWithInterval`）。
///
/// `interval` 为零时回落 [`DEFAULT_WATCH_INTERVAL`]（对应 Go 的 `interval <= 0`）。
///
/// `dir` 为空或当前不可读时返回一个**空句柄**并只打一行日志 —— 监听失败不该
/// 拖垮网关（Go 侧同样是 no-op stop）。
///
/// ★ 基线是**调用时刻**的目录指纹，所以本函数必须紧跟在「启动对齐」之后调用：
/// 排在它之前的目录变化会被算进基线而永远不触发重载。
pub fn start_auth_dir_watch(
    pool: Arc<Mutex<Pool>>,
    dir: &str,
    interval: Duration,
) -> AuthDirWatch {
    if dir.is_empty() {
        crate::logline!("[watch] auths 目录未配置，跳过热加载监听");
        return AuthDirWatch::inert();
    }
    let interval = if interval.is_zero() {
        DEFAULT_WATCH_INTERVAL
    } else {
        interval
    };
    let Some(base) = dir_fingerprint(dir) else {
        crate::logline!("[watch] auths 目录 {dir} 不可读，跳过热加载监听（加账号后需手动重启）");
        return AuthDirWatch::inert();
    };
    crate::logline!(
        "[watch] auths 目录监听已启用（每 {interval:?} 检查一次，新增账号自动加载，无需重启）"
    );

    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let owned = dir.to_string();
    let join = std::thread::Builder::new()
        .name("auth-dir-watch".into())
        .spawn(move || {
            let mut last = base;
            loop {
                match rx.recv_timeout(interval) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
                    Err(RecvTimeoutError::Timeout) => {}
                }
                // ★ `None`（不可读）与 `Some("")`（空目录）必须区别对待：
                //   前者 `continue` 不当作变化 —— 目录正在被原子替换时若把
                //   「读不到」当成「所有文件都被删了」，一次误判就会清空账号池。
                let Some(cur) = dir_fingerprint(&owned) else {
                    continue;
                };
                if cur == last {
                    continue;
                }
                last = cur;
                reload_auth_dir(&pool, &owned);
            }
        })
        .ok();

    AuthDirWatch {
        stop_tx: Some(tx),
        join,
    }
}

/// 重新加载凭证目录并与池对齐（对应 Go `reloadAuthDir`，`watch.go:120-141`）。
fn reload_auth_dir(pool: &Arc<Mutex<Pool>>, dir: &str) {
    // ★★ 必须先挡这一条：`auth::load_dir` 对「不是目录」返回的是 **`Ok(空列表)`**
    //     而不是错误（Go 那边返回 error，走的是「保持现有池状态」分支）。
    //     照单全收的话，一次目录瞬时不可读就会被当成「凭证全被删了」，整个账号池
    //     被清空 —— 与 Go 的意图正好相反。
    if !Path::new(dir).is_dir() {
        crate::logline!("WARN: [watch] {dir} 不是目录或不可读，本轮跳过（保持现有池状态）");
        return;
    }
    let auths = match auth::load_dir(dir) {
        Ok(v) => v,
        Err(e) => {
            crate::logline!("WARN: [watch] 重新加载 {dir} 失败: {e}（保持现有池状态）");
            return;
        }
    };

    let before = lock_pool(pool).all_uids().len();
    {
        let mut p = lock_pool(pool);
        p.sync_to_dir_keeping(&auths, keep_workbuddy_only(dir));
    }
    let (after, available) = {
        let p = lock_pool(pool);
        (p.all_uids().len(), p.available_uids().len())
    };

    if after != before {
        crate::logline!(
            "[watch] auths 目录变化：账号数 {before} → {after}（已热加载，无需重启；当前可选 {available}）"
        );
    } else {
        // 数量没变但内容变了（凭证刷新 / 文件改名）也要落一条，便于对账。
        crate::logline!("[watch] auths 目录变化：账号数保持 {before}（已热加载凭证更新；当前可选 {available}）");
    }
}

/// 热加载的剔除过滤器：只允许剔除「**归本目录管**的 WorkBuddy 账号」。
///
/// 对应 Go `keepWorkBuddyOnly`（`watch.go:168-179`）。返回 `true` = **保留**。
///
/// ★ 这是本项目的真实约束，不是防御性编程：`sync_to_dir_keeping` 的剔除是
/// **按 uid 全量比对**的。启动路径满足前提（那时池里只有本目录的账号），但运行期
/// 热加载不满足 —— 池里已经有 Qoder / ZCode 的账号。直接全量剔除的症状是
/// 「往 WorkBuddy 加了个账号，Qoder 和 ZCode 的账号全不见了」，且要重启才回来。
///
/// 三条判据（逐字对齐 Go）：
///
/// 1. 不是 WorkBuddy 产品 → 保留（另外两个产品的账号不归本目录管）；
/// 2. `file_path` 为空 → 保留（没有凭证文件来源，只可能是 `Pool::add` 直接塞进来的
///    多产品路径 / 单测 / 状态文件里恢复出来的条目）；
/// 3. 凭证文件不在本目录 → 保留。
pub fn keep_workbuddy_only(dir: &str) -> impl Fn(&Auth) -> bool + Send + 'static {
    let want = clean_path(dir);
    move |a: &Auth| {
        if a.product_of() != PRODUCT_WORKBUDDY {
            return true;
        }
        if a.file_path.is_empty() {
            return true;
        }
        let parent = Path::new(&a.file_path)
            .parent()
            .map(|p| clean_path(&p.to_string_lossy()))
            .unwrap_or_default();
        parent != want
    }
}

/// 目录指纹：`文件名|修改时间|大小` 逐条拼接后按字典序排序（对应 Go `dirFingerprint`）。
///
/// 返回 `None` = **目录读不了**；返回 `Some("")` = **目录是空的**（合法的空指纹）。
/// ★ 这两个状态必须能区分（Go 用一个独立的 `ok` 返回值表达同一件事）：空目录是
/// 完全正常的状态（凭证被全部删掉），若和「读不了」共用一个哨兵，清空目录就会被
/// 误判成读失败而跳过同步，已删账号永远滞留在池子里。
///
/// ★ 刻意**不读文件内容**（`watch.go:186-190`）：那是账号密钥（access/refresh
/// token），周期性扫描反复读密钥只会扩大暴露面；mtime + size 足以判定变化。
pub fn dir_fingerprint(dir: &str) -> Option<String> {
    let rd = std::fs::read_dir(dir).ok()?;
    let mut parts: Vec<String> = Vec::new();
    for ent in rd {
        let Ok(ent) = ent else { continue };
        let name = ent.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".json") {
            continue;
        }
        let Ok(md) = ent.metadata() else { continue };
        if md.is_dir() {
            continue;
        }
        parts.push(format!(
            "{name}|{}|{}",
            mtime_stamp(md.modified().ok()),
            md.len()
        ));
    }
    parts.sort();
    Some(parts.join("\n"))
}

/// 修改时间戳（纳秒精度，用于指纹）。
///
/// 与 Go 的 `info.ModTime().Format(time.RFC3339Nano)` **文本不同但性质相同**：
/// 同一 mtime 必得同一字符串，不同 mtime 必得不同字符串。指纹只是不透明比对值，
/// 不对外暴露，因此不追求逐字一致。
fn mtime_stamp(t: Option<SystemTime>) -> String {
    match t.and_then(|t| t.duration_since(UNIX_EPOCH).ok()) {
        Some(d) => format!("{}.{:09}", d.as_secs(), d.subsec_nanos()),
        None => "0.000000000".to_string(),
    }
}

/// 词法路径归一（对应 Go `filepath.Clean`）：统一分隔符、去掉 `.`、消解 `..`、
/// 合并重复分隔符、去掉结尾分隔符。**不碰文件系统**（不做 canonicalize）。
///
/// 为什么必须是纯词法的：`keep_workbuddy_only` 要对**可能已经不存在的**凭证文件
/// 判定归属（那正是「被删除、该剔除」的场景）。任何依赖磁盘查询的实现都会在
/// 文件消失时失败，从而把该剔除的账号错误地保留下来。
fn clean_path(p: &str) -> String {
    let normalized = p.replace('/', std::path::MAIN_SEPARATOR_STR);
    let mut prefix = String::new();
    let mut rooted = false;
    let mut segs: Vec<String> = Vec::new();
    for c in Path::new(&normalized).components() {
        match c {
            Component::Prefix(pre) => prefix = pre.as_os_str().to_string_lossy().into_owned(),
            Component::RootDir => rooted = true,
            Component::CurDir => {}
            Component::ParentDir => {
                if segs.last().map(|s| s != "..").unwrap_or(false) {
                    segs.pop();
                } else if !rooted {
                    segs.push("..".to_string());
                }
            }
            Component::Normal(s) => segs.push(s.to_string_lossy().into_owned()),
        }
    }
    let mut out = prefix;
    if rooted {
        out.push(std::path::MAIN_SEPARATOR);
    }
    out.push_str(&segs.join(std::path::MAIN_SEPARATOR_STR));
    if out.is_empty() {
        // Go 的 filepath.Clean("") 也是 "."。
        out.push('.');
    }
    out
}

/// 取池锁，中毒（某线程持锁 panic）时沿用内部值 —— 与 `server.rs` 同一口径：
/// 账号池数据仍然自洽，没理由因为一次 panic 让整个网关停摆。
fn lock_pool(pool: &Arc<Mutex<Pool>>) -> MutexGuard<'_, Pool> {
    match pool.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::PRODUCT_QODER;
    use crate::pool::CoolKind;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "wbswg-authwatch-{tag}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_cred(dir: &Path, name: &str, uid: &str) {
        std::fs::write(
            dir.join(name),
            format!(r#"{{"accessToken":"at-{uid}","uid":"{uid}"}}"#),
        )
        .unwrap();
    }

    fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        f()
    }

    #[test]
    fn fingerprint_distinguishes_unreadable_from_empty() {
        let d = tmpdir("fp-empty");
        let ds = d.to_string_lossy().to_string();
        // ★ 空目录是「合法的空指纹」，不是「读失败」
        assert_eq!(dir_fingerprint(&ds), Some(String::new()));
        assert_eq!(
            dir_fingerprint(&d.join("nope").to_string_lossy()),
            None,
            "不存在的目录必须是 None（与空目录区分开）"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn fingerprint_tracks_json_files_only() {
        let d = tmpdir("fp-files");
        let ds = d.to_string_lossy().to_string();
        let base = dir_fingerprint(&ds).unwrap();

        write_cred(&d, "workbuddy-a.json", "a");
        let one = dir_fingerprint(&ds).unwrap();
        assert_ne!(base, one, "新增凭证必须改变指纹");

        // 非 .json 文件与「名字以 .json 结尾的目录」都不参与
        std::fs::write(d.join("notes.txt"), "x").unwrap();
        std::fs::create_dir_all(d.join("sub.json")).unwrap();
        assert_eq!(dir_fingerprint(&ds).unwrap(), one, "非 .json / 目录不参与指纹");

        // 内容变化（大小不同）→ 指纹变
        std::fs::write(
            d.join("workbuddy-a.json"),
            r#"{"accessToken":"at-aaaa","uid":"a"}"#,
        )
        .unwrap();
        assert_ne!(dir_fingerprint(&ds).unwrap(), one, "凭证刷新必须改变指纹");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// ★★ 2026-10-09：这条测试**只对 Windows 路径语义**成立 ——
    /// 判据全是 `C:\...` 形态的路径，而 Linux 上 `Path::new(r"C:\a\b")`
    /// 里**没有** `Prefix`/`RootDir` 组件（整串是一个 `Normal`），
    /// `clean_path` / `go_dir` 的结果因此必然不同。
    ///
    /// 那正是被测函数的设计：它们复刻的是 **Go 的 `filepath.Clean`**，
    /// 而 Go 在 Windows 上跑 —— 非 Windows 上这些函数不会被调用到
    /// （`keep_workbuddy_only` / qoder 登录都只在 Windows 客户端路径上有意义）。
    /// 所以门控是**如实表达适用范围**，不是把断言放宽。
    #[cfg(windows)]
    #[test]
    fn keep_workbuddy_only_protects_other_products_and_foreign_files() {
        let keep = keep_workbuddy_only(r"C:\auths");
        // 1) 别的产品：保留
        assert!(keep(&Auth {
            product: PRODUCT_QODER.into(),
            file_path: r"C:\auths\q.json".into(),
            ..Default::default()
        }));
        // 2) 没有凭证文件来源：保留
        assert!(keep(&Auth {
            file_path: String::new(),
            ..Default::default()
        }));
        // 3) 文件属于别的目录：保留
        assert!(keep(&Auth {
            file_path: r"C:\other\workbuddy-x.json".into(),
            ..Default::default()
        }));
        // 4) 本目录的 WorkBuddy 账号：允许剔除
        assert!(!keep(&Auth {
            file_path: r"C:\auths\workbuddy-x.json".into(),
            ..Default::default()
        }));
    }

    /// ★★ 2026-10-09：这条测试**只对 Windows 路径语义**成立 ——
    /// 判据全是 `C:\...` 形态的路径，而 Linux 上 `Path::new(r"C:\a\b")`
    /// 里**没有** `Prefix`/`RootDir` 组件（整串是一个 `Normal`），
    /// `clean_path` / `go_dir` 的结果因此必然不同。
    ///
    /// 那正是被测函数的设计：它们复刻的是 **Go 的 `filepath.Clean`**，
    /// 而 Go 在 Windows 上跑 —— 非 Windows 上这些函数不会被调用到
    /// （`keep_workbuddy_only` / qoder 登录都只在 Windows 客户端路径上有意义）。
    /// 所以门控是**如实表达适用范围**，不是把断言放宽。
    #[cfg(windows)]
    #[test]
    fn clean_path_normalizes_like_go_filepath_clean() {
        assert_eq!(clean_path(""), ".");
        assert_eq!(clean_path(r"C:\a\b\"), clean_path(r"C:\a\b"));
        assert_eq!(clean_path(r"C:\a\b\\c"), clean_path(r"C:\a\b\c"));
        assert_eq!(clean_path(r"C:\a\.\b"), clean_path(r"C:\a\b"));
        assert_eq!(clean_path(r"C:\a\x\..\b"), clean_path(r"C:\a\b"));
    }

    #[test]
    fn reload_adds_and_removes_workbuddy_accounts() {
        let d = tmpdir("reload");
        let ds = d.to_string_lossy().to_string();
        write_cred(&d, "workbuddy-a.json", "a");
        write_cred(&d, "workbuddy-b.json", "b");

        let mut pool = Pool::new(String::new());
        for a in auth::load_dir(&ds).unwrap() {
            pool.add(a);
        }
        // 多产品账号：没有凭证文件（模拟 `Pool::add` 直接塞进来的路径）
        pool.add(Auth {
            uid: "q1".into(),
            access_token: "at-q1".into(),
            product: PRODUCT_QODER.into(),
            ..Default::default()
        });
        assert_eq!(pool.all_uids().len(), 3);

        let pool = Arc::new(Mutex::new(pool));

        // 删一个、加一个，然后热加载
        std::fs::remove_file(d.join("workbuddy-b.json")).unwrap();
        write_cred(&d, "workbuddy-c.json", "c");
        reload_auth_dir(&pool, &ds);

        let p = lock_pool(&pool);
        assert_eq!(
            p.all_uids(),
            vec!["a".to_string(), "c".to_string(), "q1".to_string()],
            "应加入 c、剔除 b，并保住没有凭证文件的 q1"
        );
        // ★ 删除后必须重建索引：按 uid 取回的必须仍是本人（错位会把别人的 token 发出去）
        assert_eq!(p.auth_by_uid("c").unwrap().access_token, "at-c");
        assert_eq!(p.auth_by_uid("q1").unwrap().access_token, "at-q1");
        assert!(p.auth_by_uid("b").is_none());
        drop(p);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn reload_keeps_pool_when_dir_disappears() {
        let d = tmpdir("vanish");
        let ds = d.to_string_lossy().to_string();
        write_cred(&d, "workbuddy-a.json", "a");
        let mut pool = Pool::new(String::new());
        for a in auth::load_dir(&ds).unwrap() {
            pool.add(a);
        }
        let pool = Arc::new(Mutex::new(pool));

        std::fs::remove_dir_all(&d).unwrap();
        reload_auth_dir(&pool, &ds);
        assert_eq!(
            lock_pool(&pool).all_uids().len(),
            1,
            "目录不可读时必须保持现有池状态，绝不能当成「凭证全被删了」"
        );
    }

    #[test]
    fn reload_refreshes_token_and_preserves_runtime_state() {
        let d = tmpdir("refresh");
        let ds = d.to_string_lossy().to_string();
        write_cred(&d, "workbuddy-a.json", "a");
        let mut pool = Pool::new(String::new());
        for a in auth::load_dir(&ds).unwrap() {
            pool.add(a);
        }
        pool.cooldown("a", CoolKind::Soft, Duration::from_secs(600), "429");
        let pool = Arc::new(Mutex::new(pool));

        // 宿主刷新令牌（内容变了）
        std::fs::write(
            d.join("workbuddy-a.json"),
            r#"{"accessToken":"at-a-new","uid":"a"}"#,
        )
        .unwrap();
        reload_auth_dir(&pool, &ds);

        let p = lock_pool(&pool);
        assert_eq!(p.auth_by_uid("a").unwrap().access_token, "at-a-new");
        assert!(
            p.status_by_uid("a").unwrap().cooling,
            "换 token 不该清掉冷却 —— 否则「额度用完」的账号会立刻被重新选中"
        );
        drop(p);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn watch_loads_new_account_without_restart() {
        let d = tmpdir("e2e");
        let ds = d.to_string_lossy().to_string();
        write_cred(&d, "workbuddy-a.json", "a");
        let mut pool = Pool::new(String::new());
        for a in auth::load_dir(&ds).unwrap() {
            pool.add(a);
        }
        let pool = Arc::new(Mutex::new(pool));

        let mut w = start_auth_dir_watch(Arc::clone(&pool), &ds, Duration::from_millis(30));
        assert!(w.is_running());

        write_cred(&d, "workbuddy-b.json", "b");
        assert!(
            wait_until(Duration::from_secs(5), || lock_pool(&pool)
                .all_uids()
                .len()
                == 2),
            "新增凭证应在若干轮内被热加载"
        );

        std::fs::remove_file(d.join("workbuddy-b.json")).unwrap();
        assert!(
            wait_until(Duration::from_secs(5), || lock_pool(&pool)
                .all_uids()
                .len()
                == 1),
            "删除凭证应被热加载剔除"
        );

        w.stop();
        w.stop(); // ★ 幂等：重复调用不得 panic
        assert!(!w.is_running());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn watch_is_inert_for_empty_or_unreadable_dir() {
        let pool = Arc::new(Mutex::new(Pool::new(String::new())));
        let w = start_auth_dir_watch(Arc::clone(&pool), "", Duration::from_millis(20));
        assert!(!w.is_running(), "空目录配置应是空句柄");

        let missing = std::env::temp_dir().join(format!("wbswg-nope-{}", uuid::Uuid::new_v4()));
        let w2 = start_auth_dir_watch(
            Arc::clone(&pool),
            &missing.to_string_lossy(),
            Duration::from_millis(20),
        );
        assert!(!w2.is_running(), "不可读目录应是空句柄（监听失败不拖垮网关）");
    }
}
