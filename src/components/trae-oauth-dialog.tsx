import { useEffect, useRef, useState } from "react";
import { CheckCircle2, ClipboardPaste, ExternalLink, Loader2, ShieldCheck } from "lucide-react";
import { toast } from "sonner";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Separator } from "@/components/ui/separator";
import * as api from "@/lib/api";

/**
 * 打开外部浏览器。
 *
 * 优先走 Tauri 的 opener 插件（桌面宿主能正确唤起系统默认浏览器）；
 * 纯 webui 模式退回 `window.open`。不用 `window.location.href`：
 * 那会把整个应用导航走，用户回来时页面状态全丢。
 *
 * `preopened`：调用方在**点击的同步阶段**先开好的空白窗口（见
 * [`openBlankWindow`]）。给了就直接导航它，绕开弹窗拦截。
 */
async function openExternal(url: string, preopened?: Window | null) {
  if (api.isDesktop()) {
    try {
      const { openUrl } = await import("@tauri-apps/plugin-opener");
      await openUrl(url);
      return;
    } catch {
      // 插件不可用时退回 window.open，不让打开浏览器这一步卡住登录
    }
  }
  if (preopened && !preopened.closed) {
    preopened.location.href = url;
    return;
  }
  window.open(url, "_blank", "noopener,noreferrer");
}

/**
 * 在**用户点击的同步阶段**预开一个空白窗口，返回句柄。
 *
 * ★ 为什么必须同步开：浏览器的弹窗拦截按「当前是否处在用户手势的调用栈里」
 * 判断，而 `startLogin` 在真正打开授权页之前已经 `await` 过（签发 URL、
 * 启动回环）—— 手势上下文早已丢失，此时的 `window.open` 会被**静默拦截**
 * 并返回 `null`。症状就是「点了『在浏览器中授权登录』，什么都没发生」，
 * 用户自然认为登录坏了。先同步开窗拿到句柄，等 URL 到手再改 `location`，
 * 就不受这条限制。
 *
 * ⚠ 刻意**不传** `noopener`：带 `noopener` 时 `window.open` 按规范返回
 * `null`，就拿不到句柄了。改为开窗后立刻把 `opener` 置空 ——
 * 同样切断了授权页反向操作宿主页面的能力，但句柄还在。
 */
function openBlankWindow(): Window | null {
  try {
    const win = window.open("about:blank", "_blank");
    if (win) win.opener = null;
    return win;
  } catch {
    return null;
  }
}

type Phase = "idle" | "waiting" | "done";

/**
 * Trae OAuth 授权码登录弹窗。
 *
 * 流程：签发 URL → 启动回环监听 → 浏览器授权 → 监听器收到回调自动落库 →
 * 本弹窗收到 `oauth-login-done` 事件后自动收尾。
 *
 * 双通道设计：自动回环是主路径，**手动粘贴始终可用** —— 端口被占用、
 * 用户换了浏览器、自动收尾失败时，让用户把地址栏 URL 粘进来即可完成。
 * 没有这条兜底，回环一失败功能就整个不可用了。
 */
export function TraeOAuthDialog({
  open,
  onOpenChange,
  onSuccess,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onSuccess: () => void;
}) {
  const [accountName, setAccountName] = useState("");
  const [phase, setPhase] = useState<Phase>("idle");
  const [loginUrl, setLoginUrl] = useState("");
  const [listenError, setListenError] = useState<string | null>(null);
  const [manualUrl, setManualUrl] = useState("");
  const [busy, setBusy] = useState(false);
  const [doneMessage, setDoneMessage] = useState("");
  // 回环是否已成功启动：决定界面上是「等待浏览器授权」还是「请手动粘贴」
  const [looping, setLooping] = useState(false);
  /**
   * 网络诊断结果（桌面专属）。
   *
   * ★ 为什么在**打开浏览器之前**就诊断（2026-10-02 所有者现场）：
   *
   * 所有者的系统代理（Reqable `:9000`）当时**完全不通** ——
   * `trae.cn` 经它超时 20s、直连 0.39s。浏览器默认「使用系统代理设置」，
   * 于是授权页打不开，用户只看到「访问超时」，无从判断原因。
   *
   * 提前测一次（约 8 秒超时）就能在界面上直接说明白，
   * 而不是让用户对着一个转圈的页面猜。
   *
   * ⚠ 诊断**失败不阻断登录**：它只是提示，网络正常时用户看不到任何多余的东西。
   */
  const [netDiag, setNetDiag] = useState<api.OAuthNetworkDiagnosis | null>(null);
  // 防重入：监听回调与手动提交可能同时完成
  const settledRef = useRef(false);
  // ★ 发起登录**之前**账号库里各 uid 的最新更新时间快照（uid → updatedAt）。
  // 轮询靠它判断「有没有新账号落库」—— 详见下面轮询 effect 的说明。
  const knownAccountsRef = useRef<Map<string, string | null>>(new Map());
  // 基线快照是否真的取到了。取不到时**必须关掉轮询**，否则空快照会把库里
  // 已有的账号误判成「本次新增」，一点登录就立刻谎报成功。
  const baselineReadyRef = useRef(false);
  /**
   * `onSuccess` 的 ref 化副本。
   *
   * ★ 为什么必须这样：调用方（`TraePage`）传的是内联箭头 `() => void load()`，
   * **每次父组件渲染都是一个新函数**。若把它写进 effect 依赖，监听与轮询
   * 就会被反复拆掉重建 —— 而重建之间有一个「既没订阅、也没轮询」的空窗，
   * 回调恰好落在那个窗口里就永久丢失（表现为登录明明成功、界面却一直等待）。
   * 用 ref 存住最新值，effect 依赖里只留 `open` / `phase`，订阅不再抖动。
   */
  const onSuccessRef = useRef(onSuccess);
  useEffect(() => {
    onSuccessRef.current = onSuccess;
  }, [onSuccess]);

  // 每次打开都重置：上一次的 URL 与报错留在这里只会造成困惑
  useEffect(() => {
    if (!open) return;
    setPhase("idle");
    setLoginUrl("");
    setListenError(null);
    setManualUrl("");
    setDoneMessage("");
    setLooping(false);
    setNetDiag(null);
    settledRef.current = false;
    // 基线快照与本次会话绑定：不重置的话，上一次登录留下的快照会让
    // 这一次的轮询在启动前就被误判为"已就绪"。
    knownAccountsRef.current = new Map();
    baselineReadyRef.current = false;

    // 后端可能还留着一个未完成的会话（上次弹窗被直接关掉、或应用异常退出）。
    // 这种残留会让用户这次粘贴的回调撞上旧 state 而被判 CSRF —— 提示并清掉。
    if (api.isDesktop()) {
      void api
        .oauthPending()
        .then((res) => {
          if (res.pending) {
            setListenError("上次的登录尚未完成，已重置会话。请重新点「在浏览器中授权登录」。");
          }
        })
        .catch(() => {});
      void api.oauthCancel().catch(() => {});
    }
  }, [open]);

  // 桌面宿主监听 `oauth-login-done`：回环收到回调后由后端推送
  useEffect(() => {
    if (!open || !api.isDesktop()) return;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void (async () => {
      try {
        const { listen } = await import("@tauri-apps/api/event");
        unlisten = await listen<api.TraeOAuthDoneEvent>("oauth-login-done", (event) => {
          if (disposed) return;
          const { ok, message, userId } = event.payload;
          if (settledRef.current) return;
          if (ok) {
            settledRef.current = true;
            setPhase("done");
            setDoneMessage(message);
            setLooping(false);
            toast.success(message);
            onSuccessRef.current();
            // userId 仅用于日志可读性；界面不展示（列表里已有）
            void userId;
          } else {
            // 失败**不关弹窗**：用户还要用下面的手动粘贴兜底
            setPhase("idle");
            setLooping(false);
            setListenError(message);
            toast.error(message);
          }
        });
      } catch {
        // 事件订阅失败不影响手动粘贴路径
      }
    })();
    return () => {
      disposed = true;
      unlisten?.();
    };
    // ⚠ 依赖里**刻意不放** `onSuccess`：它每次父渲染都换新引用，放进来会让
    // 订阅反复重建（详见 onSuccessRef 的说明）。
  }, [open]);

  // 关闭弹窗时停掉回环（桌面）：留着会占住 17388 端口直到 5 分钟超时
  useEffect(() => {
    if (open || !api.isDesktop()) return;
    void api.oauthStopLoopback().catch(() => {});
  }, [open]);

  /**
   * ★ 轮询兜底：不依赖任何事件总线，桌面与 webui 两种形态都能收尾。
   *
   * 为什么需要它 —— 两条完成路径都有各自的盲区：
   *
   * 1. **webui（浏览器）形态根本没有事件总线**。`oauth-login-done` 由
   *    Tauri 宿主的 `AppHandle::emit` 推送，浏览器里没有 `__TAURI_INTERNALS__`，
   *    订阅无从谈起（上面的监听 effect 已用 `isDesktop()` 挡掉）。更关键的是：
   *    这个形态下**本机 17388 回环也没人监听**（回环是 Tauri 专属命令，
   *    `oauth_start_loopback` 刻意没有 HTTP 路由），所以浏览器授权后
   *    回调**必然**无处落地 —— 用户看到的「一直停在等待状态」就是这么来的。
   * 2. **桌面形态的事件可能丢**：用户先点了「取消」又重开、窗口重载、
   *    或订阅建立晚于回调到达（`listen` 是异步的，而回环在
   *    `oauth_start_loopback` 返回时就已经在收包了）—— 这些都是竞态窗口。
   *
   * 轮询的做法：比对账号库里 uid → `updatedAt` 的**前后快照**，
   * 出现「新 uid」或「同 uid 但 `updatedAt` 变了」即判定登录成功。
   *
   * ⚠ 为什么拿 `updatedAt` 当判据是安全的：`trae_refresh_all` 会改写
   * `updated_at`（`trae_refresh::apply_refresh_success` → `update_jwt`），
   * 但它**只由用户点按钮触发**，没有任何定时器会自动跑它
   * （已确认：`crates` 里除 HTTP 路由外无其它调用点，`src-tauri` 里也没有
   * 周期任务）。所以轮询窗口内 `updatedAt` 的变化只可能来自本次登录。
   * 反过来说，**若将来给 Trae 账号加了自动刷新定时任务，这条判据就会误报**
   * —— 那时必须改成比对更具体的信号（如新增的 uid 数）。
   */
  useEffect(() => {
    if (!open || phase !== "waiting") return;
    // 基线没取到就不轮询：宁可退回「事件 + 手动粘贴」，也不能谎报成功
    if (!baselineReadyRef.current) return;
    let disposed = false;

    const check = async () => {
      if (disposed || settledRef.current) return;
      try {
        const res = await api.traeListAccounts();
        const known = knownAccountsRef.current;
        const hit = (res.accounts || []).find((a) => {
          const seen = known.get(a.userId);
          // 新账号：之前完全没见过这个 uid
          if (seen === undefined) return true;
          // 已存在的账号：updatedAt 变了说明本次登录把它重新落库了
          return seen !== (a.updatedAt ?? null);
        });
        if (!hit || disposed || settledRef.current) return;
        settledRef.current = true;
        const message = `账号 [${hit.name || hit.userId}] 登录成功`;
        setPhase("done");
        setDoneMessage(message);
        setLooping(false);
        toast.success(message);
        onSuccessRef.current();
      } catch {
        // 读列表失败（宿主没起、网络抖动）不该打断登录：下一轮再试
      }
    };

    const timer = window.setInterval(() => void check(), 2000);
    // 立刻先跑一次：账号可能在本 effect 建立之前就已经落库了
    void check();
    return () => {
      disposed = true;
      window.clearInterval(timer);
    };
    // 同监听 effect：依赖里不放 `onSuccess`。
  }, [open, phase]);

  const startLogin = async () => {
    setListenError(null);
    // ★ 同步预开窗口：必须在本函数**第一个 await 之前**调用，否则会丢失
    // 用户手势上下文而被弹窗拦截（详见 openBlankWindow 的说明）。
    //
    // ⚠ 只在 webui 形态预开：桌面形态走 Tauri 的 opener 插件唤起系统浏览器，
    // **不需要**用户手势，预开只会多留一个空白标签页没人关。
    const preopened = api.isDesktop() ? null : openBlankWindow();
    setBusy(true);
    try {
      // ★ 先记下当前账号库快照，供轮询比对「本次登录有没有落下新账号」。
      // 放在签发 URL 之前：宁可把快照取早（最多多认出一个并发新增的账号），
      // 也不能取晚 —— 取晚会让本次登录刚落的库被当成"本来就存在"而漏判。
      try {
        const before = await api.traeListAccounts();
        knownAccountsRef.current = new Map(
          (before.accounts || []).map((a) => [a.userId, a.updatedAt ?? null]),
        );
        baselineReadyRef.current = true;
      } catch {
        // 取不到基线就**关掉轮询**（而不是退化成空快照）：
        // 空快照会让库里**任何**已存在账号都被判成"新增" ⇒ 一点登录就立刻
        // 报「登录成功」，那是比卡住更糟的谎报。此时退回事件 + 手动粘贴两条路。
        knownAccountsRef.current = new Map();
        baselineReadyRef.current = false;
      }

      const info = await api.oauthLoginUrl(accountName.trim() || undefined);
      setLoginUrl(info.url);

      if (api.isDesktop()) {
        try {
          await api.oauthStartLoopback(accountName.trim() || undefined);
          setLooping(true);
        } catch (e) {
          // 端口被占用：不阻断登录，明确告知走手动粘贴
          setLooping(false);
          setListenError(api.asError(e));
        }
      } else {
        setListenError(
          "当前是 Web 模式，无法在本机监听回调端口；请在浏览器完成授权后，把地址栏的完整 URL 粘贴到下方。",
        );
      }

      await openExternal(info.url, preopened);
      setPhase("waiting");

      /*
       * ★ 打开浏览器之后**异步**诊断网络（不 await，不挡登录流程）。
       *
       * 为什么不等它：诊断最坏要 8 秒（代理不通时就是最坏），
       * 让用户先看到浏览器弹出来更重要。诊断结果到了再更新界面。
       *
       * 只在桌面形态做：webui 形态本来就不走回环，代理提示会误导。
       */
      if (api.isDesktop()) {
        void api
          .oauthDiagnoseNetwork()
          .then((d) => {
            // 只在**确实有问题**时才记下来 —— 网络正常时不打扰用户
            if (!d.direct.ok || (d.proxyEnabled && d.proxyServer && !d.viaProxy.ok) || d.loopbackAlsoProxied) {
              setNetDiag(d);
            }
          })
          .catch(() => {
            // 诊断本身失败不该影响登录：静默忽略
          });
      }
    } catch (e) {
      // 签发失败就别把预开的空白窗口留在那里
      if (preopened && !preopened.closed) preopened.close();
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const submitManual = async () => {
    const url = manualUrl.trim();
    if (!url) {
      toast.error("请粘贴浏览器地址栏中的完整回调 URL");
      return;
    }
    setBusy(true);
    try {
      const res = await api.oauthSubmitCallback(url, accountName.trim() || undefined);
      settledRef.current = true;
      setPhase("done");
      setDoneMessage(res.message);
      toast.success(res.message);
      onSuccess();
    } catch (e) {
      setListenError(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const cancel = async () => {
    // 放弃会话：否则旧 state 残留在后端，下次登录的回调可能撞上它
    await api.oauthCancel().catch(() => {});
    onOpenChange(false);
  };

  return (
    <Dialog
      open={open}
      onOpenChange={(o) => {
        if (!o) void cancel();
      }}
    >
      <DialogContent className="max-w-xl">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <ShieldCheck className="size-4" />
            OAuth 登录 Trae 账号
          </DialogTitle>
          <DialogDescription>
            用 Trae 官方授权流程安全登录，凭证由浏览器直接回调到本机，
            比手动粘贴 JWT 更可靠（能一并拿到 refresh token 自动续期）。
          </DialogDescription>
        </DialogHeader>

        {phase === "done" ? (
          <div className="space-y-3">
            <Alert>
              <CheckCircle2 className="size-4" />
              <AlertTitle>登录成功</AlertTitle>
              <AlertDescription>{doneMessage}</AlertDescription>
            </Alert>
            <DialogFooter>
              <Button onClick={() => onOpenChange(false)}>完成</Button>
            </DialogFooter>
          </div>
        ) : (
          <div className="space-y-4">
            <div className="space-y-1.5">
              <Label htmlFor="oauth-name">备注名（可选）</Label>
              <Input
                id="oauth-name"
                value={accountName}
                placeholder="例如：主号"
                maxLength={64}
                onChange={(e) => setAccountName(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter" && phase === "idle") void startLogin();
                }}
              />
            </div>

            {phase === "idle" ? (
              <Button className="w-full" disabled={busy} onClick={() => void startLogin()}>
                {busy ? <Loader2 className="size-4 animate-spin" /> : <ExternalLink className="size-4" />}
                在浏览器中授权登录
              </Button>
            ) : (
              <div className="space-y-3">
                <div className="flex items-center gap-2 rounded-lg border border-border/60 bg-muted/30 px-3 py-2.5 text-sm">
                  {looping ? (
                    <>
                      <Loader2 className="size-4 shrink-0 animate-spin" />
                      <span>
                        正在等待浏览器完成授权…（本机 17388 端口监听中，
                        5 分钟无操作自动关闭）
                      </span>
                    </>
                  ) : (
                    <>
                      <ClipboardPaste className="size-4 shrink-0" />
                      <span>请在浏览器完成授权，然后复制地址栏的完整 URL 粘贴到下方</span>
                    </>
                  )}
                </div>

                {/*
                 * ★ 网络诊断（2026-10-02 所有者现场：「浏览器会打开，但是访问超时」）。
                 *
                 * 只在**实测有问题**时才渲染（`startLogin` 里已经过滤过）：
                 * 网络正常时这里什么都不显示，不给用户添噪音。
                 *
                 * 为什么必须由产品说出来：用户看到的只是浏览器「超时」，
                 * 而真正的原因（系统代理不通）在浏览器之外 ——
                 * 不说清楚，用户只会以为是这个软件坏了。
                 */}
                {netDiag && (
                  <Alert variant="destructive">
                    <AlertTitle>
                      {netDiag.proxyEnabled && netDiag.proxyServer && !netDiag.viaProxy.ok
                        ? "系统代理当前不通，授权页可能打不开"
                        : "网络可能有问题"}
                    </AlertTitle>
                    <AlertDescription className="space-y-2">
                      <p>{netDiag.advice}</p>
                      {/* 把实测数据摆出来 —— 用户（或来帮忙的人）据此就能判断，
                          不必再问「你怎么知道」。 */}
                      <ul className="space-y-0.5 font-mono text-[11px] opacity-80">
                        {netDiag.proxyEnabled && netDiag.proxyServer && (
                          <li>系统代理：{netDiag.proxyServer}</li>
                        )}
                        <li>
                          直连 trae.cn：
                          {netDiag.direct.ok
                            ? `通（${netDiag.direct.ms}ms）`
                            : `不通（${netDiag.direct.error ?? "未知"}）`}
                        </li>
                        {netDiag.proxyEnabled && netDiag.proxyServer && !netDiag.viaProxy.skipped && (
                          <li>
                            经该代理：
                            {netDiag.viaProxy.ok
                              ? `通（${netDiag.viaProxy.ms}ms）`
                              : `不通（${netDiag.viaProxy.ms}ms 后超时）`}
                          </li>
                        )}
                        {netDiag.loopbackAlsoProxied && (
                          <li>代理例外含 &lt;-loopback&gt;：127.0.0.1 的回调也会走代理</li>
                        )}
                      </ul>
                      {/* 打不开授权页时的**兜底**：手动粘贴那条路不经过浏览器自动回调。 */}
                      <p className="text-[11px]">
                        若授权页确实打不开：可以在浏览器里手动访问上面的链接，
                        或改用「粘贴 JWT 添加」—— 它不依赖浏览器回调。
                      </p>
                    </AlertDescription>
                  </Alert>
                )}

                {/* ★ Web 模式的正确预期：这一形态下没有本机回环监听器
                    （`oauth_start_loopback` 是 Tauri 专属命令，刻意没有 HTTP 路由），
                    所以浏览器授权后会跳到一个**打不开**的本地地址。这**不是**故障 ——
                    那个地址栏里的 URL 本身就是完成登录所需的凭证载体。
                    不提前讲清楚，用户看到「无法访问此网站」就会以为登录坏了
                    （这正是「无法登陆」的观感来源）。 */}
                {!api.isDesktop() && (
                  <Alert>
                    <AlertTitle>浏览器会跳到打不开的本地地址，这是正常的</AlertTitle>
                    <AlertDescription>
                      授权成功后，浏览器会跳转到{" "}
                      <span className="font-mono">127.0.0.1:17388</span>{" "}
                      并提示无法访问 —— Web 模式下没有进程在监听该端口。
                      请把<strong>地址栏里的完整 URL</strong>复制回来，粘贴到下面的输入框提交，
                      登录即完成。
                    </AlertDescription>
                  </Alert>
                )}

                {loginUrl && (
                  <div className="flex items-center gap-2 text-xs">
                    <span className="text-muted-foreground">授权页没自动打开？</span>
                    <Button
                      size="sm"
                      variant="link"
                      className="h-auto p-0 text-xs"
                      onClick={() => void openExternal(loginUrl)}
                    >
                      再次打开
                    </Button>
                    {/* 端口提示只在桌面形态有意义：Web 模式下那个端口并没有被监听，
                        标出来反而会让人以为「有服务在收，只是没收到」。 */}
                    {api.isDesktop() && (
                      <Badge variant="outline" className="ml-auto font-mono text-[10px]">
                        回调端口 {17388}
                      </Badge>
                    )}
                  </div>
                )}

                <Separator />

                {/* 手动兜底：始终可用，不因回环成功而隐藏 */}
                <div className="space-y-1.5">
                  <Label htmlFor="oauth-callback">手动完成（推荐兜底路径）</Label>
                  <div className="flex gap-2">
                    <Input
                      id="oauth-callback"
                      value={manualUrl}
                      placeholder="http://127.0.0.1:17388/authorize?code=..."
                      className="font-mono text-xs"
                      onChange={(e) => setManualUrl(e.target.value)}
                      onKeyDown={(e) => {
                        if (e.key === "Enter") void submitManual();
                      }}
                    />
                    <Button disabled={busy || !manualUrl.trim()} onClick={() => void submitManual()}>
                      {busy ? <Loader2 className="size-4 animate-spin" /> : "提交"}
                    </Button>
                  </div>
                  <p className="text-xs text-muted-foreground">
                    粘贴浏览器地址栏中的完整 URL（含 <span className="font-mono">code</span> 与{" "}
                    <span className="font-mono">state</span> 参数）。
                  </p>
                </div>
              </div>
            )}

            {listenError && (
              <Alert variant="destructive">
                <AlertTitle>
                  {looping ? "登录未完成" : "自动接收回调不可用"}
                </AlertTitle>
                <AlertDescription className="break-all">{listenError}</AlertDescription>
              </Alert>
            )}
          </div>
        )}

        {phase !== "done" && (
          <DialogFooter>
            <Button variant="outline" onClick={() => void cancel()}>
              取消
            </Button>
          </DialogFooter>
        )}
      </DialogContent>
    </Dialog>
  );
}
