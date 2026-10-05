import { Component, type ErrorInfo, type ReactNode } from "react";

/**
 * 顶层错误边界：任何渲染期异常都不该让整页变成白屏。
 *
 * # 为什么必须有它（2026-10-01 真实事故）
 *
 * 所有者装完 1.0.14 后点「兼容网关」，**整页永久白屏** ——
 * 连左侧导航都消失，只能重启应用。实测抓到的异常是：
 *
 * ```text
 * TypeError: xxx is not iterable
 *   at GatewayPage 的 useMemo（accountModels 形状不符，详见 types.ts）
 * ```
 *
 * React 的行为是：**渲染期未捕获异常 ⇒ 卸载整棵树**。
 * 于是「一个页面的一处数据形状不符」被放大成「整个应用不可用」——
 * 用户连切到别的页面自救的机会都没有。
 *
 * # 它解决什么、不解决什么
 *
 * - **解决**：故障被**限制在一个页面内**，用户能看到「哪个页面出错了」、
 *   能看错误摘要、能一键重试、还能**切到别的页面继续用**。
 * - **不解决**：它不修任何具体缺陷。它只是把「白屏」变成「可读的错误页」，
 *   让下一次同类缺陷**可诊断**而不是让人对着白屏猜。
 *
 * ★ 本仓库的教训是：白屏最难的不是修，而是**不知道哪一行炸的**
 * （Electron/Tauri 的 WebView 控制台默认看不到）。这个边界把栈打出来，
 * 下次这类问题从「查半天」变成「看一眼」。
 */
interface Props {
  children: ReactNode;
  /**
   * ★★ 路由变化信号（本仓库里传的是 `location.pathname`）。
   *
   * # 为什么不用 `key`（2026-10-05 实测事故）
   *
   * 原先调用方写的是 `<ErrorBoundary key={location.pathname}>`。`key` 一变，
   * React 就**卸载整棵子树再重新挂载** —— 目的（重置边界状态）达到了，代价是
   * 边界内所有页面组件的实例与 DOM 全部销毁重建。
   *
   * 这在「账号管理」里是硬伤：五个平台标签各自是一个路由（`/accounts/trae`…），
   * 点一次标签就换一次 `key` ⇒ `AccountHubPage` 的 `visited` 状态被重置、
   * `forceMount` 保活的面板整个消失 ⇒ 业主要求的「页面切换保持状态缓存不重置」
   * 在主路径上完全不生效（CDP 实测：打了 expando 的面板在切标签后从 DOM 消失）。
   *
   * 改成普通 prop 后：路由变化仍会在**出错时**把边界重置回 children
   * （见 `componentDidUpdate`），而不出错时**不产生任何额外渲染**，
   * 子树照旧被 React 复用。
   */
  resetKey?: unknown;
}

interface State {
  error: Error | null;
  info: ErrorInfo | null;
}

export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null, info: null };

  static getDerivedStateFromError(error: Error): Partial<State> {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    this.setState({ info });
    // 同时打到控制台：开发时用 DevTools 能直接看到完整栈。
    console.error("[ErrorBoundary] 渲染期异常：", error, info.componentStack);
  }

  /**
   * ★ 仅在「已经出错」时因路由变化而复位。
   *
   * 不写成 `getDerivedStateFromProps`：那会让每次父组件渲染都多走一次派生状态计算；
   * 这里只在 `resetKey` 真的变了且当前处于错误态时才 `setState`，正常路径零开销。
   * 复用 `reset()` 也保证了与「重试」按钮完全相同的语义。
   */
  componentDidUpdate(prevProps: Props) {
    if (prevProps.resetKey !== this.props.resetKey && this.state.error) {
      this.reset();
    }
  }

  private reset = () => {
    this.setState({ error: null, info: null });
  };

  private reload = () => {
    window.location.reload();
  };

  render() {
    const { error, info } = this.state;
    if (!error) return this.props.children;

    return (
      <div className="flex min-h-screen items-center justify-center bg-background p-6">
        <div className="w-full max-w-2xl space-y-4 rounded-lg border border-destructive/40 bg-card p-6 shadow-sm">
          <div className="space-y-1">
            <h1 className="text-lg font-semibold text-destructive">页面出错了</h1>
            <p className="text-sm text-muted-foreground">
              这一页在渲染时抛了异常，已被拦截 —— 应用其余部分仍可使用。
              你可以重试，或先用左侧导航切到别的页面。
            </p>
          </div>

          <div className="space-y-2">
            <div className="text-xs font-medium text-muted-foreground">错误摘要</div>
            <pre className="max-h-40 overflow-auto rounded bg-muted p-3 text-xs whitespace-pre-wrap break-all">
              {error.name}: {error.message}
            </pre>
          </div>

          {info?.componentStack ? (
            <details className="space-y-2">
              <summary className="cursor-pointer text-xs font-medium text-muted-foreground">
                组件栈（排查用，可展开）
              </summary>
              <pre className="mt-2 max-h-56 overflow-auto rounded bg-muted p-3 text-xs whitespace-pre-wrap break-all">
                {info.componentStack}
              </pre>
            </details>
          ) : null}

          <div className="flex gap-2">
            <button
              type="button"
              onClick={this.reset}
              className="rounded bg-primary px-3 py-1.5 text-sm font-medium text-primary-foreground hover:opacity-90"
            >
              重试
            </button>
            <button
              type="button"
              onClick={this.reload}
              className="rounded border px-3 py-1.5 text-sm font-medium hover:bg-muted"
            >
              重新加载应用
            </button>
          </div>

          <p className="text-xs text-muted-foreground">
            ★ 「重试」只是重新渲染一次：若异常来自数据形状，点它会立刻再次出错。
            这时请把上面的错误摘要发给开发者 —— 它足以定位到具体代码行。
          </p>
        </div>
      </div>
    );
  }
}
