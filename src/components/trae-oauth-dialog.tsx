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
 */
async function openExternal(url: string) {
  if (api.isDesktop()) {
    try {
      const { openUrl } = await import("@tauri-apps/plugin-opener");
      await openUrl(url);
      return;
    } catch {
      // 插件不可用时退回 window.open，不让打开浏览器这一步卡住登录
    }
  }
  window.open(url, "_blank", "noopener,noreferrer");
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
  // 防重入：监听回调与手动提交可能同时完成
  const settledRef = useRef(false);

  // 每次打开都重置：上一次的 URL 与报错留在这里只会造成困惑
  useEffect(() => {
    if (!open) return;
    setPhase("idle");
    setLoginUrl("");
    setListenError(null);
    setManualUrl("");
    setDoneMessage("");
    setLooping(false);
    settledRef.current = false;

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
            onSuccess();
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
  }, [open, onSuccess]);

  // 关闭弹窗时停掉回环（桌面）：留着会占住 17388 端口直到 5 分钟超时
  useEffect(() => {
    if (open || !api.isDesktop()) return;
    void api.oauthStopLoopback().catch(() => {});
  }, [open]);

  const startLogin = async () => {
    setBusy(true);
    setListenError(null);
    try {
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

      await openExternal(info.url);
      setPhase("waiting");
    } catch (e) {
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
                        正在等待浏览器完成授权…（本机 {api.isDesktop() ? "17388 端口" : ""}
                        监听中，5 分钟无操作自动关闭）
                      </span>
                    </>
                  ) : (
                    <>
                      <ClipboardPaste className="size-4 shrink-0" />
                      <span>请在浏览器完成授权，然后复制地址栏的完整 URL 粘贴到下方</span>
                    </>
                  )}
                </div>

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
                    <Badge variant="outline" className="ml-auto font-mono text-[10px]">
                      回调端口 {17388}
                    </Badge>
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
