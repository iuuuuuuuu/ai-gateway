import claudeIcon from "@/assets/agent-icons/claude.svg";
import codexIcon from "@/assets/agent-icons/codex.svg";
import deepseekIcon from "@/assets/agent-icons/deepseek.svg";
import grokIcon from "@/assets/agent-icons/grok.svg";
import hermesIcon from "@/assets/agent-icons/hermes.png";
import kimiIcon from "@/assets/agent-icons/kimi-light.svg";
import minimaxIcon from "@/assets/agent-icons/minimax-code.svg";
import openclawIcon from "@/assets/agent-icons/openclaw.svg";
import opencodeIcon from "@/assets/agent-icons/opencode.svg";
import piIcon from "@/assets/agent-icons/pi-logo-on-light.svg";
import zcodeIcon from "@/assets/agent-icons/zcode.png";

/** 客户端图标映射。 */
export const CLIENT_ICONS: Record<string, string> = {
  "claude-code": claudeIcon,
  "claude-desktop": claudeIcon,
  codex: codexIcon,
  dsh: deepseekIcon,
  opencode: opencodeIcon,
  pi: piIcon,
  "grok-build": grokIcon,
  zcode: zcodeIcon,
  "kimi-code": kimiIcon,
  openclaw: openclawIcon,
  hermes: hermesIcon,
  "minimax-code": minimaxIcon,
};

/** Claude 客户端的模型槽位上限：Sonnet / Opus / Haiku / Fable。 */
export const CLAUDE_SLOT_LIMIT = 4;

/** 使用 Anthropic Messages 协议、只支持固定槽位的客户端。 */
export const CLAUDE_PROTOCOL_TARGETS = new Set(["claude-code", "claude-desktop"]);

export interface ClientMeta {
  protocol: string;
  protocolBadge: string;
  description: string;
}

const CLIENT_METAS: Record<string, ClientMeta> = {
  "claude-code": {
    protocol: "Anthropic Messages",
    protocolBadge: "POST /v1/messages",
    description:
      "Anthropic 官方终端助手。四模型槽位（Sonnet/Opus/Haiku/Fable）直写真实模型名，网关零转译转发。",
  },
  "claude-desktop": {
    protocol: "Anthropic Messages (3P)",
    protocolBadge: "3P Gateway",
    description:
      "Claude 桌面客户端第三方网关模式。自动配置 Sonnet/Opus/Haiku/Fable 四槽位与鉴权凭据。",
  },
  codex: {
    protocol: "OpenAI Responses",
    protocolBadge: "POST /v1/responses",
    description: "Codex CLI 终端助手。完美兼容 0.146+ 版本强制要求的 Responses API 协议。",
  },
  dsh: {
    protocol: "OpenAI Chat",
    protocolBadge: "POST /v1/chat/completions",
    description: "DeepSeek Harness 架构工具。通过标准 OpenAI Chat 协议直连网关。",
  },
  opencode: {
    protocol: "OpenAI Chat",
    protocolBadge: "POST /v1/chat/completions",
    description:
      "开源 AI 编程终端与桌面环境。自动写入 provider.workbuddy 兼容配置并支持多模型。",
  },
  pi: {
    protocol: "OpenAI Chat",
    protocolBadge: "POST /v1/chat/completions",
    description: "极简终端编程助手。通过 providers.workbuddy 协议提供方无缝挂载多模型接入。",
  },
  "grok-build": {
    protocol: "OpenAI Responses",
    protocolBadge: "POST /v1/responses",
    description:
      "Grok 命令行研发工具。自动设置 default 模型及每个选定模型的 [model.\"...\"] 配置表。",
  },
  zcode: {
    protocol: "OpenAI Chat",
    protocolBadge: "POST /v1/chat/completions",
    description: "ZCode 编程助理。自动写入 v2 规范 provider 架构配置与全套多选模型参数。",
  },
  "kimi-code": {
    protocol: "OpenAI Chat",
    protocolBadge: "POST /v1/chat/completions",
    description:
      "Kimi Code 终端助手。写入 default_model 与每个选中模型的 [models.\"...\"] 协议表。",
  },
  openclaw: {
    protocol: "OpenAI Chat",
    protocolBadge: "POST /v1/chat/completions",
    description: "OpenClaw 多智能体框架。自动写入 models.providers 并激活默认主模型。",
  },
  hermes: {
    protocol: "OpenAI Chat",
    protocolBadge: "POST /v1/chat/completions",
    description: "Hermes Agent 终端助手。自动在 custom_providers 列表挂载所有选定模型。",
  },
  "minimax-code": {
    protocol: "Anthropic Messages",
    protocolBadge: "POST /v1/messages",
    description:
      "MiniMax Code 桌面客户端。在 provider 块注册本网关（Anthropic 协议适配器），并把 defaultModel 指向它；官方 MiniMax 账号配置原样保留。",
  },
};

const FALLBACK_META: ClientMeta = {
  protocol: "通用兼容",
  protocolBadge: "API",
  description: "AI 客户端集成支持",
};

export function metaOf(id: string): ClientMeta {
  return CLIENT_METAS[id] ?? FALLBACK_META;
}

export function iconOf(id: string): string {
  return CLIENT_ICONS[id] || claudeIcon;
}

/** 一级菜单的条目形态（与 AgentSwitcher 的入参保持一致）。 */
export interface ClientItem {
  id: string;
  label: string;
  icon: string;
  badge?: "terminal" | "monitor";
  hint?: string;
}

export function toClientItems(
  targets: Array<{ id: string; label: string }>,
): ClientItem[] {
  return targets.map((target) => ({
    id: target.id,
    label: target.label,
    icon: iconOf(target.id),
    hint: metaOf(target.id).protocol,
    badge:
      target.id === "claude-code"
        ? "terminal"
        : target.id === "claude-desktop"
          ? "monitor"
          : undefined,
  }));
}