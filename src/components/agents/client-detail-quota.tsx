import { CliQuotaPanel, CLIENT_TO_PROVIDER } from "@/components/cli-quota-panel";
import { Card } from "@/components/ui/card";

/**
 * 智能体客户端详情里的「登录额度」卡片。
 *
 * 只有 `CLIENT_TO_PROVIDER` 里有映射的客户端才渲染 —— 没有本机登录凭证语义的
 * 客户端（如 OpenCode、ZCode）显示一个空的额度卡片只会让人以为功能坏了。
 *
 * 不自动查询（`autoLoad` 缺省 false）：详情面板可能在列表里被逐个渲染，
 * 自动查询会让打开页面就打出多次网络请求。由用户点「查询」触发。
 */
export function ClientDetailQuota({ targetId }: { targetId: string }) {
  const provider = CLIENT_TO_PROVIDER[targetId];
  if (!provider) return null;

  return (
    <Card className="gap-3 rounded-xl border border-border/70 p-4 shadow-none">
      <CliQuotaPanel provider={provider} />
    </Card>
  );
}
