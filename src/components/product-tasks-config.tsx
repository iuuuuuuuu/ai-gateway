import { useCallback, useEffect, useState } from "react";
import { Loader2, RefreshCw, Save } from "lucide-react";

import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { CardContent } from "@/components/ui/card";
import { Switch } from "@/components/ui/switch";
import { SettingsFieldRow, SettingsGroup } from "@/components/settings-primitives";
import * as api from "@/lib/api";
import type { GatewayConfig } from "@/lib/types";

// product-tasks-config.tsx —— Qoder / ZCode 的「权益自动领取」配置。
//
// # ⚠ 2026-09-22：拆成**两个独立开关**（此前是一个共用开关）
//
// 所有者原话：
//
//	「权益自动领取 qoder zcode 拆分开,不要合成一个」
//
// 起因是两者诉求完全不同：
//
//	· Qoder  —— 每天 10:00 重置的**每日额度**，不领就过期作废
//	· ZCode  —— **限量套餐**（先到先得），节奏更接近"抢"
//
// 合成一个开关会让用户"想关 A 却把 B 也关了"。
//
// # ⚠ 三态语义（本组件最容易写错的地方）
//
// 每个开关的取值有三种，**不是两种**：
//
//	undefined（配置里没这个键）→ 回落到总闸 `product_tasks_enabled`
//	true                       → 显式开
//	false                      → 显式关
//
// 界面上显示时，`undefined` 要**按总闸的实际值**来决定显示开还是关
//（否则存量用户会看到两个"关闭"的开关，而实际还在自动领）。
// 但保存时**只有用户真的点了**才写具体值 —— 绝不主动写 undefined。

/** 从配置里读某个分产品开关的**实际生效值**（三态 → 布尔）。 */
function effectiveSwitch(cfg: GatewayConfig | null, key: "qoder" | "zcode"): boolean {
  const explicit = key === "qoder" ? cfg?.qoder_claim_enabled : cfg?.zcode_claim_enabled;
  if (explicit !== undefined) return explicit;
  // 没配 ⇒ 回落总闸；总闸缺省 true（与 Go 侧 `unwrap_or(true)` 一致）。
  // ⚠ 不能写成 `?? false` —— 那会让界面显示"已关闭"而实际还在自动领，
  // 比不显示更糟（用户以为自己已经关掉了）。
  return cfg?.product_tasks_enabled !== false;
}

/** 该分产品键是否**已被显式配置**（用于提示"当前跟随总闸"）。 */
function isExplicit(cfg: GatewayConfig | null, key: "qoder" | "zcode"): boolean {
  return (key === "qoder" ? cfg?.qoder_claim_enabled : cfg?.zcode_claim_enabled) !== undefined;
}

interface SwitchRowProps {
  label: string;
  description: React.ReactNode;
  checked: boolean;
  explicit: boolean;
  disabled: boolean;
  busy: boolean;
  onChange: (next: boolean) => void;
}

function ClaimSwitchRow({
  label,
  description,
  checked,
  explicit,
  disabled,
  busy,
  onChange,
}: SwitchRowProps) {
  return (
    <SettingsFieldRow label={label} description={description} operational>
      <div className="flex items-center gap-2">
        {busy && <Loader2 className="size-3.5 animate-spin text-muted-foreground" />}
        <Switch
          checked={checked}
          disabled={disabled || busy}
          onCheckedChange={onChange}
          aria-label={label}
        />
        {!explicit && (
          <span className="text-[11px] text-muted-foreground/70">跟随总开关</span>
        )}
      </div>
    </SettingsFieldRow>
  );
}

export function ProductTasksConfigCard() {
  const [cfg, setCfg] = useState<GatewayConfig | null>(null);
  const [saving, setSaving] = useState<"qoder" | "zcode" | null>(null);
  const [msg, setMsg] = useState<{ type: "ok" | "err"; text: string } | null>(null);
  const [loading, setLoading] = useState(true);

  const load = useCallback(async () => {
    try {
      const res = await api.getGatewayConfig();
      setCfg(res.config);
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  async function toggle(which: "qoder" | "zcode", next: boolean) {
    if (saving) return;
    setSaving(which);
    setMsg(null);
    const prev = cfg;
    // 乐观更新：开关必须即时响应，否则点下去要等一个来回才动。
    setCfg((cur: GatewayConfig | null) =>
      cur
        ? {
            ...cur,
            ...(which === "qoder"
              ? { qoder_claim_enabled: next }
              : { zcode_claim_enabled: next }),
          }
        : cur,
    );
    try {
      const res = await api.saveGatewayConfig(
        which === "qoder"
          ? { qoder_claim_enabled: next }
          : { zcode_claim_enabled: next },
      );
      setCfg(res.config);
      setMsg({
        type: "ok",
        text: next
          ? `已开启 ${which === "qoder" ? "Qoder" : "ZCode"} 自动领取。网关重启后生效。`
          : `已关闭 ${which === "qoder" ? "Qoder" : "ZCode"} 自动领取。网关重启后生效 —— ` +
            `注意未领取的额度会随活动到期作废。`,
      });
    } catch (e) {
      setCfg(prev); // 失败回滚，不能让界面显示一个没保存成功的状态
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setSaving(null);
    }
  }

  const qoderOn = effectiveSwitch(cfg, "qoder");
  const zcodeOn = effectiveSwitch(cfg, "zcode");
  const hours = cfg?.qoder_claim_hours ?? [10, 21];
  const hoursText = hours.length > 0 ? hours.map((h) => `${h}:00`).join("、") : "仅按轮询";

  return (
    <SettingsGroup id="settings-product-tasks" title="权益自动领取">
      <CardContent className="space-y-0 p-0">
        <ClaimSwitchRow
          label="自动领取 Qoder 权益活动"
          description={
            <>
              每天 <span className="font-medium text-foreground">{hoursText}</span> 各触发一次
              （本地时间），两次互相兜底，防止某一轮错漏。
              <br />
              活动每天 <span className="font-medium text-foreground">10:00（UTC+8）重置</span>，
              单条时限约 22 小时 —— <span className="font-medium text-foreground">不领就会过期作废</span>。
            </>
          }
          checked={qoderOn}
          explicit={isExplicit(cfg, "qoder")}
          disabled={loading}
          busy={saving === "qoder"}
          onChange={(v) => void toggle("qoder", v)}
        />

        <ClaimSwitchRow
          label="自动领取 ZCode 套餐权益"
          description={
            <>
              网关启动即跑一轮，之后每 <span className="font-medium text-foreground">5 分钟</span> 检查
              （失败进入冷却，冷却结束自动重试）。
              <br />
              <span className="font-medium text-foreground">与 Qoder 独立</span> —— 关掉其中一个不影响另一个。
            </>
          }
          checked={zcodeOn}
          explicit={isExplicit(cfg, "zcode")}
          disabled={loading}
          busy={saving === "zcode"}
          onChange={(v) => void toggle("zcode", v)}
        />

        {msg && (
          <div className="px-4 pb-3 sm:px-5">
            <Alert variant={msg.type === "err" ? "destructive" : "default"}>
              <AlertDescription>{msg.text}</AlertDescription>
            </Alert>
          </div>
        )}

        <div className="flex items-center justify-between gap-3 px-4 py-3 sm:px-5">
          <p className="text-xs leading-4 text-muted-foreground/75">
            配置改动写入网关配置文件，<span className="font-medium">网关重启后生效</span>。
          </p>
          <Button
            type="button"
            size="sm"
            variant="ghost"
            disabled={loading || saving !== null}
            onClick={() => void load()}
          >
            {loading ? <Loader2 className="animate-spin" /> : <RefreshCw />}
            重新读取
          </Button>
        </div>
      </CardContent>
    </SettingsGroup>
  );
}

/**
 * 「立即领取」按钮 —— 手动触发一轮，用于**验证开关是否真的生效**。
 *
 * 与自动领取走同一实现（`claim-all-campaigns`），故手动能领到
 * 就意味着自动那条路也是通的。
 */
export function ProductClaimNowCard() {
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<{ type: "ok" | "err"; text: string } | null>(null);

  async function run() {
    if (busy) return;
    setBusy(true);
    setMsg(null);
    try {
      const res = await api.qoderClaimAllCampaigns();
      const claimed = res.claimedCount ?? 0;
      const nothing = res.nothingCount ?? 0;
      const failed = res.failedCount ?? 0;
      setMsg({
        type: failed > 0 ? "err" : "ok",
        text: `本轮：成功领取 ${claimed} 项 / 无可领 ${nothing} 项 / 失败 ${failed} 项`,
      });
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setBusy(false);
    }
  }

  return (
    <SettingsGroup id="settings-product-claim-now" title="立即领取">
      <CardContent className="space-y-0 p-0">
        <SettingsFieldRow
          label="立刻跑一轮 Qoder 领取"
          description="不等定时任务，现在就把可领的权益领掉。与自动领取走同一实现，故也可用来验证开关是否生效。"
          operational
        >
          <Button type="button" size="sm" disabled={busy} onClick={() => void run()}>
            {busy ? <Loader2 className="animate-spin" /> : <Save />}
            立即领取
          </Button>
        </SettingsFieldRow>
        {msg && (
          <div className="px-4 pb-3 sm:px-5">
            <Alert variant={msg.type === "err" ? "destructive" : "default"}>
              <AlertDescription>{msg.text}</AlertDescription>
            </Alert>
          </div>
        )}
      </CardContent>
    </SettingsGroup>
  );
}
