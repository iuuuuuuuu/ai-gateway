import { useCallback, useEffect, useMemo, useState } from "react";
import {
  Ban,
  Check,
  Info,
  Loader2,
  Pencil,
  Plus,
  RefreshCw,
  Save,
  Trash2,
  X,
} from "lucide-react";

import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { SettingsGroup } from "@/components/settings-primitives";
import * as api from "@/lib/api";
import { PRODUCT_LABELS } from "@/lib/product-accent";
import type { PlatformModelMap } from "@/lib/types";
import { cn } from "@/lib/utils";

// platform-models-config.tsx —— 「平台 × 区域 → 模型」白名单的增删改查 + 禁用。
//
// 所有者 2026-09-28 的需求：
//
//	「给每个平台手动配置支持的模型,而且要区分国内外版本」
//	「配置页面你都没加,我怎么加配置?你在每个页面的配置里面加一个表单,
//	  用来手动配置,增删改查 禁用」
//
// # ⚠ 三态语义（本组件最容易写错的地方，务必读懂再改）
//
// 每个 (平台, 区域, 模型) 有三种状态，**不是两种**：
//
//	允许    —— 在 `platform_models` 里          → 一定放行
//	禁用    —— 在 `platform_models_disabled` 里 → 一定拦住（否决项）
//	未配置  —— 两个都不在                        → 跟随接口清单 / 兜底
//
// # ⚠ 为什么"禁用"不能靠"从白名单里删掉"来实现
//
// 网关的放行判据是**并集**：
//
//	手动配置 ∪ 接口返回(上游查询) ∪ 内置兜底   命中任一即放行
//
// 所以把一个模型从允许清单里删掉，**它可能仍然被放行** —— 因为
// 「接口返回」那个来源还在放它。实测 `qoder:deepseek-v4.1-flash`
// 就是这么被放行的。
//
// 因此"禁用"是一个**独立于并集的否决项**，在网关侧**先于**并集判定。
// 界面上表现为：把开关拨到"禁用"时，该模型写进 `platform_models_disabled`，
// 而**不是**简单地不出现在允许清单里。这两者效果完全不同。
//
// # ⚠ 保存时必须提交**全部平台**，不能只提交当前平台
//
// `set_platform_models` 是**整键替换**（`platform_models` 整个被覆盖，
// 而不是按平台深合并）。只提交当前页面的平台，会把其他平台配好的
// 白名单**静默清空**。故本组件始终持有全量 state，只编辑自己那一片。

/** 区域选择器的取值。Radix Select 不允许空串，故用哨兵值代替 `""`。 */
const REGION_ANY = "__any__";

interface RegionOption {
  value: string;
  label: string;
  hint: string;
}

const REGION_OPTIONS: RegionOption[] = [
  { value: "cn", label: "国服", hint: "只对该平台的国服账号生效" },
  { value: "intl", label: "国际版", hint: "只对该平台的国际版账号生效" },
  {
    value: REGION_ANY,
    label: "不分区域",
    hint: "国服与国际版都适用（两个区域各自还有自己的清单时，以更具体的为准）",
  },
];

/** 界面上的一个条目：模型名 + 是"允许"还是"禁用"。 */
interface Entry {
  model: string;
  allowed: boolean;
}

/** 全量草稿：平台 → 区域键 → 条目列表。区域键是 `cn` / `intl` / `""`。 */
type Draft = Record<string, Record<string, Entry[]>>;

/** 把两份清单合并成一份带状态的草稿（同一模型两边都有时按"禁用"算，与网关一致）。 */
function draftFrom(allow: PlatformModelMap, deny: PlatformModelMap): Draft {
  const out: Draft = {};
  const products = new Set([...Object.keys(allow ?? {}), ...Object.keys(deny ?? {})]);
  for (const product of products) {
    const regions = new Set([
      ...Object.keys(allow?.[product] ?? {}),
      ...Object.keys(deny?.[product] ?? {}),
    ]);
    const byRegion: Record<string, Entry[]> = {};
    for (const region of regions) {
      const allowed = new Set(allow?.[product]?.[region] ?? []);
      const denied = new Set(deny?.[product]?.[region] ?? []);
      const models = [...new Set([...allowed, ...denied])].sort((a, b) => a.localeCompare(b));
      byRegion[region] = models.map((model) => ({
        model,
        // 两边都出现 ⇒ 网关是否决优先。界面必须显示"禁用"，
        // 否则用户看到一个"允许"的开关却怎么都发不出去。
        allowed: allowed.has(model) && !denied.has(model),
      }));
    }
    out[product] = byRegion;
  }
  return out;
}

/** 草稿 → 两份提交用的清单（空区域整个丢掉 = 未配置）。 */
function splitDraft(draft: Draft): {
  allow: PlatformModelMap;
  deny: PlatformModelMap;
} {
  const allow: PlatformModelMap = {};
  const deny: PlatformModelMap = {};
  for (const [product, byRegion] of Object.entries(draft)) {
    for (const [region, entries] of Object.entries(byRegion)) {
      const yes = entries.filter((e) => e.allowed).map((e) => e.model);
      const no = entries.filter((e) => !e.allowed).map((e) => e.model);
      if (yes.length > 0) (allow[product] ??= {})[region] = yes;
      if (no.length > 0) (deny[product] ??= {})[region] = no;
    }
  }
  return { allow, deny };
}

/** 区域键 → 界面显示名（含"不分区域"）。 */
function regionLabel(region: string): string {
  if (region === "") return "不分区域";
  return REGION_OPTIONS.find((o) => o.value === region)?.label ?? region;
}

/** 归一化模型名 —— 必须与宿主/网关侧的规则一致（去空白、小写）。 */
function normalizeModel(raw: string): string {
  return raw.trim().toLowerCase();
}

interface RowProps {
  entry: Entry;
  busy: boolean;
  onToggle: (allowed: boolean) => void;
  onRename: (next: string) => void;
  onRemove: () => void;
}

function ModelRow({ entry, busy, onToggle, onRename, onRemove }: RowProps) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(entry.model);

  useEffect(() => {
    if (!editing) setDraft(entry.model);
  }, [entry.model, editing]);

  function commit() {
    const next = normalizeModel(draft);
    if (next === "") {
      setDraft(entry.model);
      setEditing(false);
      return;
    }
    setEditing(false);
    if (next !== entry.model) onRename(next);
  }

  return (
    <div
      className={cn(
        "flex min-w-0 flex-wrap items-center justify-between gap-2 border-b border-border/50 py-2 last:border-b-0",
        !entry.allowed && "opacity-70",
      )}
    >
      <div className="flex min-w-0 flex-1 items-center gap-2">
        {editing ? (
          <>
            <Input
              autoFocus
              value={draft}
              className="h-7 max-w-xs font-mono text-xs"
              onChange={(e) => setDraft(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") commit();
                if (e.key === "Escape") {
                  setDraft(entry.model);
                  setEditing(false);
                }
              }}
            />
            <Button size="sm" variant="ghost" className="h-7 px-2" onClick={commit}>
              <Check />
            </Button>
            <Button
              size="sm"
              variant="ghost"
              className="h-7 px-2"
              onClick={() => {
                setDraft(entry.model);
                setEditing(false);
              }}
            >
              <X />
            </Button>
          </>
        ) : (
          <>
            <span
              className={cn(
                "truncate font-mono text-xs",
                !entry.allowed && "line-through decoration-muted-foreground/50",
              )}
              title={entry.model}
            >
              {entry.model}
            </span>
            <Badge
              variant={entry.allowed ? "success" : "destructive"}
              className="shrink-0 gap-1 text-[10px]"
            >
              {entry.allowed ? <Check /> : <Ban />}
              {entry.allowed ? "允许" : "禁用"}
            </Badge>
          </>
        )}
      </div>

      <div className="flex shrink-0 items-center gap-1">
        <Switch
          checked={entry.allowed}
          disabled={busy || editing}
          onCheckedChange={onToggle}
          aria-label={`${entry.model} ${entry.allowed ? "允许" : "禁用"}`}
        />
        <Button
          size="sm"
          variant="ghost"
          className="h-7 px-2"
          disabled={busy || editing}
          onClick={() => setEditing(true)}
          title="改模型名"
        >
          <Pencil />
        </Button>
        <Button
          size="sm"
          variant="ghost"
          className="h-7 px-2 text-muted-foreground hover:text-destructive"
          disabled={busy}
          onClick={onRemove}
          title="从清单里删掉（回到「未配置」，跟随接口清单）"
        >
          <Trash2 />
        </Button>
      </div>
    </div>
  );
}

export interface PlatformModelsConfigCardProps {
  /**
   * 本页面对应的平台稳定标识：`workbuddy` / `qoder` / `zcode`。
   *
   * 只编辑这个平台那一片，但**提交时带上其他平台的全量数据**（见文件头
   * 的"保存时必须提交全部平台"）。
   */
  product: string;
}

export function PlatformModelsConfigCard({ product }: PlatformModelsConfigCardProps) {
  const [draft, setDraft] = useState<Draft | null>(null);
  const [region, setRegion] = useState<string>("cn");
  const [newModel, setNewModel] = useState("");
  const [busy, setBusy] = useState(false);
  const [loading, setLoading] = useState(true);
  const [msg, setMsg] = useState<{ type: "ok" | "err"; text: string } | null>(null);
  const [suggestions, setSuggestions] = useState<string[]>([]);

  const productLabel = PRODUCT_LABELS[product] ?? product;

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const state = await api.getPlatformModels();
      setDraft(draftFrom(state.platform_models, state.platform_models_disabled));
      setMsg(null);
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // 模型名建议：网关已知的模型列表。拿不到就不给建议，不影响手填。
  useEffect(() => {
    let alive = true;
    void (async () => {
      try {
        const models = await api.getGatewayModels();
        if (!alive) return;
        setSuggestions(
          [...new Set(models.map((m) => normalizeModel(m.id)).filter(Boolean))].sort((a, b) =>
            a.localeCompare(b),
          ),
        );
      } catch {
        // 静默：建议列表是锦上添花，拿不到不该在配置卡片里报错。
      }
    })();
    return () => {
      alive = false;
    };
  }, []);

  const entries: Entry[] = useMemo(
    () => draft?.[product]?.[region] ?? [],
    [draft, product, region],
  );

  /** 已知区域 = 配置里出现过的 + 三个固定项（去重）。 */
  const regionChoices = useMemo(() => {
    const known = Object.keys(draft?.[product] ?? {});
    const out: RegionOption[] = [...REGION_OPTIONS];
    for (const k of known) {
      if (!out.some((o) => o.value === (k === "" ? REGION_ANY : k))) {
        out.push({ value: k === "" ? REGION_ANY : k, label: regionLabel(k), hint: "配置里已有的区域" });
      }
    }
    return out;
  }, [draft, product]);

  /** 只改自己那一片，其他平台原样保留（整键替换，不能丢）。 */
  function mutate(fn: (list: Entry[]) => Entry[]) {
    setDraft((cur) => {
      const base: Draft = cur ? { ...cur } : {};
      const byRegion = { ...(base[product] ?? {}) };
      byRegion[region] = fn(byRegion[region] ?? []);
      base[product] = byRegion;
      return base;
    });
  }

  function addModel() {
    const model = normalizeModel(newModel);
    if (model === "") return;
    const exists = entries.some((e) => e.model === model);
    if (exists) {
      setMsg({ type: "err", text: `「${model}」已经在${regionLabel(region)}的清单里了。` });
      return;
    }
    // 新条目默认"允许"—— 用户点"添加"的意图是想让它可用；
    // 想禁用的话拨一下开关即可。
    mutate((list) => [...list, { model, allowed: true }].sort((a, b) => a.model.localeCompare(b.model)));
    setNewModel("");
    setMsg(null);
  }

  async function save() {
    if (busy || !draft) return;
    setBusy(true);
    setMsg(null);
    const { allow, deny } = splitDraft(draft);
    try {
      await api.setPlatformModels(allow, deny);
      setMsg({
        type: "ok",
        text: `已保存。网关重启后生效 —— 允许 ${countOf(allow, product)} 个、禁用 ${countOf(deny, product)} 个模型。`,
      });
      // 回读一次：以磁盘上的归一化结果为准，而不是只信本地 state。
      await load();
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setBusy(false);
    }
  }

  const allowCount = entries.filter((e) => e.allowed).length;
  const denyCount = entries.length - allowCount;

  return (
    <SettingsGroup id={`settings-platform-models-${product}`} title={`${productLabel} 可用的模型`}>
      <CardContent className="space-y-0 p-0">
        <div className="border-b border-border/50 px-4 py-3 sm:px-5">
          <p className="text-xs leading-5 text-muted-foreground/85">
            这里配置 <span className="font-medium text-foreground">{productLabel}</span> 平台
            <span className="font-medium text-foreground">允许</span>或
            <span className="font-medium text-foreground">禁用</span>哪些模型。
            发请求时用 <code className="rounded bg-muted px-1 font-mono">{product}:模型名</code> 指定平台
            （带区域：<code className="rounded bg-muted px-1 font-mono">{product}:国际版:模型名</code>）。
          </p>
          <p className="mt-1.5 flex items-start gap-1.5 text-xs leading-5 text-muted-foreground/75">
            <Info className="mt-0.5 size-3.5 shrink-0" />
            <span>
              <span className="font-medium text-foreground">禁用</span>是一个独立的否决项：
              即使该模型出现在上游返回的清单里，也会被拦住。
              而<span className="font-medium text-foreground">删掉</span>只是回到"未配置"
              —— 此时是否可用由上游清单决定，可能仍然放行。
            </span>
          </p>
        </div>

        <div className="flex flex-wrap items-center justify-between gap-3 border-b border-border/50 px-4 py-3 sm:px-5">
          <div className="flex items-center gap-2">
            <span className="text-[13px] leading-4">区域</span>
            <Select
              value={region === "" ? REGION_ANY : region}
              onValueChange={(v) => setRegion(v === REGION_ANY ? "" : v)}
              disabled={busy || loading}
            >
              <SelectTrigger size="sm" className="w-40" aria-label="区域">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {regionChoices.map((o) => (
                  <SelectItem key={o.value} value={o.value}>
                    {o.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
          <p className="text-xs text-muted-foreground/75">
            {loading ? "读取中…" : `本区域：允许 ${allowCount} 个 / 禁用 ${denyCount} 个`}
          </p>
        </div>

        <div className="px-4 sm:px-5">
          {loading ? (
            <p className="flex items-center gap-2 py-6 text-xs text-muted-foreground">
              <Loader2 className="size-3.5 animate-spin" />
              正在读取配置…
            </p>
          ) : entries.length === 0 ? (
            <p className="py-6 text-xs leading-5 text-muted-foreground/75">
              还没有为「{regionLabel(region)}」配置任何模型。
              <br />
              此时不限制该区域 —— 只要上游清单里有，就能用。想明确拦住某个模型，
              在下面加上它，再把开关拨到"禁用"。
            </p>
          ) : (
            entries.map((entry) => (
              <ModelRow
                key={entry.model}
                entry={entry}
                busy={busy}
                onToggle={(allowed) =>
                  mutate((list) => list.map((e) => (e.model === entry.model ? { ...e, allowed } : e)))
                }
                onRename={(next) =>
                  mutate((list) => {
                    // 改名后可能与已有条目重名 —— 那就合并（保留已有的开关状态，
                    // 否则"改个名"会静默把另一条的配置冲掉）。
                    const without = list.filter((e) => e.model !== entry.model);
                    const hit = without.find((e) => e.model === next);
                    if (hit) return without;
                    return [...without, { model: next, allowed: entry.allowed }].sort((a, b) =>
                      a.model.localeCompare(b.model),
                    );
                  })
                }
                onRemove={() => mutate((list) => list.filter((e) => e.model !== entry.model))}
              />
            ))
          )}
        </div>

        <div className="flex flex-wrap items-center gap-2 border-t border-border/50 px-4 py-3 sm:px-5">
          <Input
            value={newModel}
            list={`platform-models-suggest-${product}`}
            placeholder="模型名，例如 deepseek-v4.1-flash"
            className="h-8 max-w-xs font-mono text-xs"
            disabled={busy || loading}
            onChange={(e) => setNewModel(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") addModel();
            }}
          />
          <datalist id={`platform-models-suggest-${product}`}>
            {suggestions.map((m) => (
              <option key={m} value={m} />
            ))}
          </datalist>
          <Button
            type="button"
            size="sm"
            variant="outline"
            disabled={busy || loading || newModel.trim() === ""}
            onClick={addModel}
          >
            <Plus />
            添加到「{regionLabel(region)}」
          </Button>
        </div>

        {msg && (
          <div className="px-4 pb-3 sm:px-5">
            <Alert variant={msg.type === "err" ? "destructive" : "default"}>
              <AlertDescription>{msg.text}</AlertDescription>
            </Alert>
          </div>
        )}

        <div className="flex flex-wrap items-center justify-between gap-3 border-t border-border/50 px-4 py-3 sm:px-5">
          <p className="text-xs leading-4 text-muted-foreground/75">
            保存写入网关配置，<span className="font-medium">网关重启后生效</span>。
          </p>
          <div className="flex items-center gap-2">
            <Button
              type="button"
              size="sm"
              variant="ghost"
              disabled={loading || busy}
              onClick={() => void load()}
            >
              {loading ? <Loader2 className="animate-spin" /> : <RefreshCw />}
              重新读取
            </Button>
            <Button type="button" size="sm" disabled={loading || busy} onClick={() => void save()}>
              {busy ? <Loader2 className="animate-spin" /> : <Save />}
              保存
            </Button>
          </div>
        </div>
      </CardContent>
    </SettingsGroup>
  );
}

/** 数一个平台在两份清单里的条目数（只用于保存后的提示文案）。 */
function countOf(map: PlatformModelMap, product: string): number {
  const byRegion = map[product];
  if (!byRegion) return 0;
  return Object.values(byRegion).reduce((n, list) => n + list.length, 0);
}
