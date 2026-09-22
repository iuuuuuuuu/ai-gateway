import { useEffect, useRef, useState } from "react";
import { Loader2, MessageSquare, Phone, ShieldCheck } from "lucide-react";

import { Alert, AlertDescription } from "@/components/ui/alert";
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
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import * as api from "@/lib/api";

// sms-login-dialog.tsx —— 手机号 + 短信验证码添加账号（2026-09-22 新增）。
//
// # 为什么需要它
//
// 此前只有两条路加账号：**从本机导入**（要求装了桌面客户端且已登录）
// 与 **OAuth**（redirect_uri 是自定义协议，只有官方客户端能回调）。
// 短信登录是唯一**不依赖桌面端**的路子 —— 服务器/远程场景尤其需要。
//
// # 两步式交互（无法合并成一步）
//
// 验证码在用户手机上，天然要两次人机交互。合成一个接口就得把
// "等用户输入"做成服务端会话状态，而网关是无状态转发的。
//
// ⚠ 第二步失败（验证码错）时**不清空手机号**，只清验证码：
// 手机号是用户手打的、且是对的，让他重输一遍是纯粹的折磨。

interface SmsLoginDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** 添加成功后回调（父级刷新列表 + 关弹窗）。 */
  onSuccess?: (message: string) => void;
}

export function SmsLoginDialog({ open, onOpenChange, onSuccess }: SmsLoginDialogProps) {
  const [phone, setPhone] = useState("");
  const [code, setCode] = useState("");
  const [region, setRegion] = useState<"cn" | "intl">("cn");
  /** 是否已发出验证码（决定界面停在第一步还是第二步）。 */
  const [sent, setSent] = useState(false);
  const [busy, setBusy] = useState<"send" | "verify" | null>(null);
  const [err, setErr] = useState<string | null>(null);
  /** 验证码有效期倒计时（秒）；0 = 可重发。 */
  const [left, setLeft] = useState(0);
  const codeRef = useRef<HTMLInputElement>(null);

  // 倒计时：到 0 才能重发，避免用户狂点导致上游频控。
  useEffect(() => {
    if (left <= 0) return;
    const t = window.setInterval(() => setLeft((n) => (n <= 1 ? 0 : n - 1)), 1000);
    return () => window.clearInterval(t);
  }, [left]);

  // 关弹窗时**整体复位**：下次打开应是干净的第一步。
  // ⚠ 不复位会让上次的手机号/验证码留在输入框里，用户以为已经填过了。
  useEffect(() => {
    if (open) return;
    setPhone("");
    setCode("");
    setSent(false);
    setErr(null);
    setLeft(0);
    setBusy(null);
  }, [open]);

  // 进入第二步时自动聚焦验证码框 —— 用户下一步动作必然是输验证码。
  useEffect(() => {
    if (sent) codeRef.current?.focus();
  }, [sent]);

  async function send() {
    if (busy) return;
    setErr(null);
    setBusy("send");
    try {
      const res = await api.smsSend(phone, region);
      setSent(true);
      setLeft(res.expiresIn > 0 ? res.expiresIn : 300);
    } catch (e) {
      setErr(api.asError(e));
    } finally {
      setBusy(null);
    }
  }

  async function verify() {
    if (busy) return;
    setErr(null);
    setBusy("verify");
    try {
      const res = await api.smsVerify(phone, code, region);
      const name = res.account?.phone || phone;
      onSuccess?.(`已添加账号 ${name}`);
      onOpenChange(false);
    } catch (e) {
      setErr(api.asError(e));
      // ⚠ 只清验证码，保留手机号（见文件头）
      setCode("");
      codeRef.current?.focus();
    } finally {
      setBusy(null);
    }
  }

  const phoneOk = /^\d{6,15}$/.test(phone.trim());
  const canSend = phoneOk && !busy && left === 0;
  const canVerify = phoneOk && code.trim().length > 0 && !busy;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>手机号登录添加账号</DialogTitle>
          <DialogDescription>
            不依赖桌面客户端：向手机号发送验证码，验证通过后自动加入账号列表。
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-4 py-2">
          <div className="space-y-2">
            <Label htmlFor="sms-region">服务区域</Label>
            {/* 用既有的 shadcn Select（仓库没有 radio-group）。
                按 AGENTS.md 的 UI 规范：优先组合既有组件，
                缺组件时也要走 shadcn 体系而不是写自定义交互件。 */}
            <Select
              value={region}
              onValueChange={(v) => {
                // 换区域要重来：两个区域的验证码不通用，
                // 沿用前一个区域的验证码必然失败。
                setRegion(v as "cn" | "intl");
                setSent(false);
                setCode("");
                setLeft(0);
                setErr(null);
              }}
              disabled={sent || busy !== null}
            >
              <SelectTrigger id="sms-region">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="cn">国服（workbuddy.cn）</SelectItem>
                <SelectItem value="intl">国际版（workbuddy.ai）</SelectItem>
              </SelectContent>
            </Select>
          </div>

          <div className="space-y-2">
            <Label htmlFor="sms-phone">手机号</Label>
            <div className="flex gap-2">
              <div className="relative flex-1">
                <Phone className="absolute left-3 top-1/2 size-4 -translate-y-1/2 text-muted-foreground" />
                <Input
                  id="sms-phone"
                  className="pl-9"
                  placeholder="11 位手机号"
                  inputMode="numeric"
                  value={phone}
                  disabled={busy !== null || sent}
                  onChange={(e) => setPhone(e.target.value.replace(/\D/g, ""))}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" && canSend) void send();
                  }}
                />
              </div>
              <Button
                type="button"
                variant="outline"
                disabled={!canSend}
                onClick={() => void send()}
                className="shrink-0"
              >
                {busy === "send" ? (
                  <Loader2 className="animate-spin" />
                ) : (
                  <MessageSquare />
                )}
                {left > 0 ? `${left}s 后重发` : sent ? "重发验证码" : "发送验证码"}
              </Button>
            </div>
            {/* 手机号锁定后给一条说明：用户看到输入框变灰要知道为什么 */}
            {sent && (
              <p className="text-xs text-muted-foreground">
                手机号已锁定。若要更换，请关闭本窗口重新开始。
              </p>
            )}
          </div>

          {sent && (
            <div className="space-y-2">
              <Label htmlFor="sms-code">验证码</Label>
              <div className="relative">
                <ShieldCheck className="absolute left-3 top-1/2 size-4 -translate-y-1/2 text-muted-foreground" />
                <Input
                  id="sms-code"
                  ref={codeRef}
                  className="pl-9 tracking-[0.3em]"
                  placeholder="短信里的验证码"
                  inputMode="numeric"
                  value={code}
                  disabled={busy !== null}
                  onChange={(e) => setCode(e.target.value.replace(/\D/g, ""))}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" && canVerify) void verify();
                  }}
                />
              </div>
            </div>
          )}

          {err && (
            <Alert variant="destructive">
              <AlertDescription>{err}</AlertDescription>
            </Alert>
          )}
        </div>

        <DialogFooter>
          <Button type="button" variant="ghost" onClick={() => onOpenChange(false)}>
            取消
          </Button>
          <Button type="button" disabled={!canVerify} onClick={() => void verify()}>
            {busy === "verify" ? <Loader2 className="animate-spin" /> : null}
            登录并添加
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
