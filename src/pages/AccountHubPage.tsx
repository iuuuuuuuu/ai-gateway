/**
 * 统一账号页：把五个「XX 账号」页面收进一个页面，用 tab 切换。
 *
 * # 为什么合并
 * 五个账号页在侧栏里各占一行，而它们**互相之间的账号库、凭证、登录态都是独立的** ——
 * 用户得在五个入口之间来回跳，才能看清"我到底有哪些号、哪些在接流量"。
 * 业主原话（m00983）：把这几个账号管理合并到一个页面，通过 tabs 切换来查看。
 *
 * # ⚠ 平台开关的语义（极易误解，改之前先读完这段）
 * 关掉一个平台 = **只把它从兼容网关的选号 / 路由里排除**，它的账号照样跑签到、
 * 上报、成长任务与额度巡检。业主原话（m01323）：「关闭平台只是不参与兼容网关，
 * 并不是不执行任务请注意」。
 *
 * 所以这里**只调 setPlatformEnabled**，绝不写账号级的 disabled —— 后者的语义是
 * "网关判定该账号已死"，会让养号任务直接跳过它（两套字段在后端也是分开的）。
 *
 * # ⚠ 「键缺失 = 参加」
 * 后端下发的 platformEnabled 里**没有某个键**时，含义是"从未配过 ⇒ 参加"，
 * 不是"关掉了"。所以判断一律写成 !== false，**别用** !!platformEnabled[p]
 * —— 那会把"没配过"读成"关掉了"，一进页面就把所有平台显示成关闭。
 *
 * # 开关为什么长在标签里（业主 m02670 + 本次补充）
 * 第一版把开关放在标签行**右端**，作用对象是"当前激活的标签" —— 用户仍要先把
 * "右端那个开关"和"我现在看的平台"对上号。业主原话：「这个平台路由开关跟这个
 * tabs 这个合到一起吧，不要拆分了，操作不方便也不直观」，随后补充：「我的意思是
 * tabs 和这个开关合到一起去，**每个平台都有自己的开关**」⇒ 最终形态是**开关直接
 * 长在每个平台标签内部**：看到哪个标签，它上面那个开关就是它自己的，不再需要
 * "当前平台"这个中间概念，也不存在"看的是 Trae、改的是 Qoder"的错配。
 *
 * 由此产生的三个实现约束（改之前先读完）：
 * 1. Radix 的 TabsTrigger 把 onValueChange 挂在 **mousedown**（不是 click）上，
 *    所以标签内的开关必须在 pointerdown / mousedown / click 三处都
 *    stopPropagation，否则点开关会顺带把标签也切了。
 * 2. 开关的 checked 一律来自 offPlatforms（**不是** TabsTrigger 的 data-state）——
 *    "标签是否激活"和"平台是否参加路由"是两件互不相干的事。
 * 3. 开关可以单独操作**非激活标签**（想关掉某个平台不必先切过去），所以它的
 *    disabled 只取决于配置是否已加载 / 是否有另一个保存在途，与激活状态无关。
 *
 * # 页面状态为什么不会丢（业主 m02672）
 * Radix 的 TabsContent 默认**卸载**未选中的内容，切回来等于重新挂载：滚动位置、
 * 展开的分组、已加载的记录全部回到初始态。业主原话：「这些页面应该有缓存，
 * 而不是切换页面，再切回来就又恢复初始状态了」。
 *
 * 这里的做法是 **lazy + keep-alive**：只有"访问过"的 tab 才带上 forceMount 常驻
 * 挂载（Radix 会把它渲染在 hidden 容器里），没访问过的仍是零成本 —— 若一上来就
 * 让五个页面全部常驻，五个页面的轮询 hook 会同时开跑，反而拖慢首屏。
 * 代价：常驻页面即使不可见，其定时器仍在跑（见各页面内的 useVisibilityInterval /
 * 静默轮询设计），因此 keep-alive 的范围刻意只限"用户真的打开过"的 tab。
 */
import { useEffect, useState, type ReactNode } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { Ban, Loader2 } from "lucide-react";
import { toast } from "sonner";

import * as api from "@/lib/api";
import { demoModeEnabled } from "@/lib/demo-mode";
import { Switch } from "@/components/ui/switch";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { DoubaoMark, QoderMark, TraeMark, WorkBuddyMark, ZcodeMark } from "@/components/product-marks";
import AccountsPage from "@/pages/AccountsPage";
// 积分统计 / Token 统计并入本页的 WorkBuddy tab（业主 m02672：这两页有独特性，
// 放在「账号管理」下更好找）。它们各自保留独立路由的用法，故仍按页面 import。
import CreditStatsPage from "@/pages/CreditStatsPage";
import TokenStatsPage from "@/pages/TokenStatsPage";
import QoderPage from "@/pages/QoderPage";
import ZcodePage from "@/pages/ZcodePage";
import TraePage from "@/pages/TraePage";
import DoubaoPage from "@/pages/DoubaoPage";

/**
 * tab 键 = URL 段 = 平台开关的键。
 *
 * 豆包也在这里（它有自己的账号页），但它**不是路由平台** —— 没有产品模块、
 * 账号不进网关池，故它没有开关，见 ROUTING_TABS。
 */
type AccountTab = "workbuddy" | "qoder" | "zcode" | "trae" | "doubao";

const TABS: { key: AccountTab; label: string; mark: ReactNode }[] = [
  { key: "workbuddy", label: "WorkBuddy", mark: <WorkBuddyMark size={16} /> },
  { key: "qoder", label: "Qoder", mark: <QoderMark size={16} /> },
  { key: "zcode", label: "ZCode", mark: <ZcodeMark size={16} /> },
  { key: "trae", label: "Trae", mark: <TraeMark size={16} /> },
  { key: "doubao", label: "豆包", mark: <DoubaoMark size={16} /> },
];

/** 有平台开关的 tab：只有参与兼容网关选号的平台才有"参加 / 不参加"这回事。 */
const ROUTING_TABS: AccountTab[] = ["workbuddy", "qoder", "zcode", "trae"];

function isAccountTab(value: string | undefined): value is AccountTab {
  return value !== undefined && TABS.some((t) => t.key === value);
}

function tabLabel(key: AccountTab): string {
  return TABS.find((t) => t.key === key)?.label ?? key;
}

/** 从后端回显里挑出**显式关闭**的平台（false）；键缺失或 true 都算参加。 */
function readOffPlatforms(map: Record<string, boolean> | undefined): Set<string> {
  return new Set(
    Object.entries(map ?? {})
      .filter(([, enabled]) => enabled === false)
      .map(([platform]) => platform),
  );
}

/**
 * 标签内那个开关的悬浮提示。
 *
 * 关掉开关最容易被误解成"停掉这个平台的任务"，所以提示里必须把**任务照常跑**
 * 这半句说全（业主 m01323 的语义约束：关闭平台只是不参与兼容网关）。
 */
function routingTitle(label: string, checked: boolean): string {
  return checked
    ? "已参加兼容网关路由：关掉后 " + label + " 账号不再被网关选号，签到等养号任务照常运行"
    : "已退出兼容网关路由：打开后 " + label + " 账号重新参与网关选号";
}

/**
 * 把某个平台的页面按「嵌入模式」渲染。
 *
 * ⚠ 刻意用 switch 逐个列举，而不是做一张 Record<AccountTab, ComponentType> 表：
 * 五个页面的 props 类型互不相通，映射表的联合类型会被 TS 摊平成 never；
 * 逐个列举还顺带把"哪个 tab 对应哪个页面"钉在一处。
 */
function PlatformPanel({ tab }: { tab: AccountTab }) {
  switch (tab) {
    case "workbuddy":
      return <WorkbuddyPanel />;
    case "qoder":
      return <QoderPage embedded />;
    case "zcode":
      return <ZcodePage embedded />;
    case "trae":
      return <TraePage embedded />;
    case "doubao":
      return <DoubaoPage embedded />;
  }
}

/** WorkBuddy tab 内部的二级视图：账号 / 积分统计 / Token 统计。 */
type WorkbuddyView = "accounts" | "credits" | "tokens";

/**
 * WorkBuddy tab 的内容：三级 tab 把「账号」「积分统计」「Token 统计」并到一处。
 *
 * 为什么合并：这两张统计页的数据源就是 WorkBuddy / CodeBuddy 账号，
 * 单独挂在侧栏反而要用户在「账号」与「统计」之间来回跳（业主 m02672）。
 *
 * keep-alive 与外面那层同法（见文件头）：只有访问过的视图才 forceMount 常驻，
 * 没访问过的按需挂载 —— 统计页各自带请求，一上来全部常驻会白打一轮接口。
 */
function WorkbuddyPanel() {
  const [view, setView] = useState<WorkbuddyView>("accounts");
  const [visited, setVisited] = useState<Set<WorkbuddyView>>(() => new Set<WorkbuddyView>(["accounts"]));

  return (
    <Tabs
      value={view}
      onValueChange={(v) => {
        const next = v as WorkbuddyView;
        setVisited((p) => (p.has(next) ? p : new Set(p).add(next)));
        setView(next);
      }}
    >
      <TabsList className="h-auto flex-wrap border border-border bg-transparent">
        <TabsTrigger value="accounts" className="h-8 gap-2 px-3">
          账号
        </TabsTrigger>
        <TabsTrigger value="credits" className="h-8 gap-2 px-3">
          积分统计
        </TabsTrigger>
        <TabsTrigger value="tokens" className="h-8 gap-2 px-3">
          Token 统计
        </TabsTrigger>
      </TabsList>

      <TabsContent
        key="accounts"
        value="accounts"
        className="min-w-0"
        forceMount={visited.has("accounts") ? true : undefined}
      >
        <AccountsPage embedded />
      </TabsContent>
      <TabsContent
        key="credits"
        value="credits"
        className="min-w-0"
        forceMount={visited.has("credits") ? true : undefined}
      >
        <CreditStatsPage embedded />
      </TabsContent>
      <TabsContent
        key="tokens"
        value="tokens"
        className="min-w-0"
        forceMount={visited.has("tokens") ? true : undefined}
      >
        <TokenStatsPage embedded />
      </TabsContent>
    </Tabs>
  );
}

export default function AccountHubPage() {
  const navigate = useNavigate();
  const { tab } = useParams<{ tab: string }>();
  // 非法 URL 段（手打 /accounts/xxx）一律回落到 workbuddy，并把 URL 归一化 ——
  // 用 replace 而不是 push：否则用户按"后退"会退回同一个脏地址，原地打转。
  const active: AccountTab = isAccountTab(tab) ? tab : "workbuddy";

  useEffect(() => {
    if (tab !== undefined && !isAccountTab(tab)) navigate("/accounts/workbuddy", { replace: true });
  }, [tab, navigate]);

  // 平台开关：存"显式关闭的平台集合"，而不是一张布尔表 —— 理由见文件头的「键缺失 = 参加」。
  const [offPlatforms, setOffPlatforms] = useState<Set<string>>(new Set());
  const [enabledLoaded, setEnabledLoaded] = useState(false);
  const [savingPlatform, setSavingPlatform] = useState<AccountTab | null>(null);

  // keep-alive 的准入集合：只有打开过的 tab 才常驻（见文件头「页面状态为什么不会丢」）。
  const [visited, setVisited] = useState<Set<AccountTab>>(() => new Set<AccountTab>([active]));

  // 地址栏 / 后退前进都能改 active（onValueChange 只在点击时触发），
  // 所以"进入过的 tab"记在 active 的副作用里，而不是只记在点击回调里。
  useEffect(() => {
    setVisited((prev) => (prev.has(active) ? prev : new Set(prev).add(active)));
  }, [active]);

  useEffect(() => {
    let alive = true;
    void (async () => {
      try {
        const res = await api.getPlatformEnabled();
        if (alive) setOffPlatforms(readOffPlatforms(res.platformEnabled));
      } catch (e) {
        // 读不到不该让整页报错（账号管理本身仍然可用），但必须说一声 ——
        // 静默失败会让用户看到"开关是开的"，进而以为后端也是开的。
        if (alive) toast.error("读取平台开关失败：" + api.asError(e));
      } finally {
        if (alive) setEnabledLoaded(true);
      }
    })();
    return () => {
      alive = false;
    };
  }, []);

  const togglePlatform = async (platform: AccountTab, next: boolean) => {
    // ★ 乐观更新 + 失败回滚：开关是即时反馈控件，等一个往返才动会显得卡；
    //   而配置写失败必须退回原样，否则界面与落盘配置分叉。
    const before = offPlatforms;
    const after = new Set(before);
    if (next) after.delete(platform);
    else after.add(platform);
    setOffPlatforms(after);
    setSavingPlatform(platform);
    try {
      // 只送这一个键：后端是"整键替换后合并进配置"，不需要回传其它平台。
      const res = await api.setPlatformEnabled({ [platform]: next });
      // 以后端回显为准：它才是真正落盘的配置（并发改动 / 后端补默认值都在这里体现）。
      setOffPlatforms(readOffPlatforms(res.platformEnabled));
      toast.success(
        next
          ? "已让 " + tabLabel(platform) + " 参加兼容网关路由"
          : "已让 " + tabLabel(platform) + " 退出兼容网关路由（签到等养号任务照常运行）",
      );
    } catch (e) {
      setOffPlatforms(before);
      toast.error(api.asError(e));
    } finally {
      setSavingPlatform(null);
    }
  };

  const handleTabChange = (value: string) => {
    if (isAccountTab(value)) {
      setVisited((prev) => (prev.has(value) ? prev : new Set(prev).add(value)));
    }
    navigate("/accounts/" + value);
  };

  return (
    <div className="w-full space-y-6 px-5 py-6 sm:px-8 sm:py-8">
      {/* 共享 header：五个平台各自的标题 / 副标题都留在自己的 tab 里（嵌入模式下已隐藏），
          这里只保留一份 28px 大标题，避免同屏出现两个页面标题。 */}
      <header className="mb-6">
        <h1 className="text-[28px] font-semibold tracking-tight">账号管理</h1>
        <p className="mt-2 text-sm leading-6 text-muted-foreground">
          用标签切换平台；每个平台标签自带它的路由开关。
          关掉开关只让它退出兼容网关选号，签到、上报与额度巡检等养号任务照常运行。
        </p>
      </header>

      <Tabs value={active} onValueChange={handleTabChange}>
        {/* 每个平台标签自带自己的开关（业主 m02670 + 本次补充）：
            开关不再挂在行右端"作用于当前标签"，而是长在标签里面，一一对应。 */}
        <div className="flex flex-wrap items-center gap-3">
          <TabsList className="h-auto flex-wrap">
            {TABS.map((t) => {
              const tabRouting = ROUTING_TABS.includes(t.key);
              const checked = !offPlatforms.has(t.key);
              const busy = savingPlatform === t.key;
              return (
                <TabsTrigger
                  key={t.key}
                  value={t.key}
                  className="group/tab h-auto gap-2 py-1.5 pr-1.5 pl-3"
                  title={tabRouting ? routingTitle(t.label, checked) : t.label + " 不是兼容网关的路由平台：账号不进网关账号池"}
                >
                  {t.mark}
                  {t.label}
                  {tabRouting ? (
                    busy ? (
                      <Loader2 className="size-3.5 animate-spin text-muted-foreground" />
                    ) : (
                      <Switch
                        checked={checked}
                        // 只跟"配置是否已加载 / 是否有另一个保存在途"有关，
                        // 与标签是否激活无关：非激活标签也能直接开关。
                        disabled={!enabledLoaded || savingPlatform !== null || demoModeEnabled}
                        onCheckedChange={(next) => void togglePlatform(t.key, next)}
                        // ★ Radix 的切标签动作挂在 mousedown 上，这三处必须止住，
                        //   否则点开关会顺带切标签（见文件头第 1 条约束）。
                        onClick={(e) => e.stopPropagation()}
                        onPointerDown={(e) => e.stopPropagation()}
                        onMouseDown={(e) => e.stopPropagation()}
                        className="ml-1 opacity-70 transition-opacity group-hover/tab:opacity-100 data-[state=checked]:opacity-100"
                        aria-label={t.label + " 参加兼容网关路由"}
                      />
                    )
                  ) : (
                    // 豆包没有开关：它不是路由平台，账号不进网关账号池。
                    <Ban className="size-3.5 text-muted-foreground/60" />
                  )}
                </TabsTrigger>
              );
            })}
          </TabsList>

          <p className="max-w-md min-w-0 text-xs leading-5 text-muted-foreground">
            标签上的开关就是该平台自己的路由开关。关闭只让它退出网关选号，签到、上报与额度巡检等养号任务照常运行。
          </p>
        </div>

        {TABS.map((t) => (
          <TabsContent
            key={t.key}
            value={t.key}
            className="min-w-0"
            // 访问过的 tab 常驻挂载（切走不卸载 ⇒ 状态不丢）；没访问过的照旧按需挂载。
            forceMount={visited.has(t.key) ? true : undefined}
          >
            <PlatformPanel tab={t.key} />
          </TabsContent>
        ))}
      </Tabs>
    </div>
  );
}
