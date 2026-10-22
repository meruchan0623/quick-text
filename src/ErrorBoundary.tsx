// Outermost error boundary: a render error shows a recoverable page instead of a blank window, and
// is logged through `log_frontend_error` without its message or any rule content (see
// lib/errorReport.ts).
import { Component, Fragment, type ErrorInfo, type ReactNode } from 'react';
import { invoke } from './ipc/transport';
import { frontendErrorReport } from './lib/errorReport';

interface Props {
  children: ReactNode;
}

interface State {
  failed: boolean;
  /** Bumped by 重新加载界面 so that the children mount afresh. */
  generation: number;
}

export class ErrorBoundary extends Component<Props, State> {
  override state: State = { failed: false, generation: 0 };

  static getDerivedStateFromError(): Partial<State> {
    return { failed: true };
  }

  override componentDidCatch(error: unknown, info: ErrorInfo): void {
    // A 0.3 backend has no such command, and a failed report must not fail the page.
    invoke<null>('log_frontend_error', frontendErrorReport(error, info.componentStack)).catch(
      () => undefined,
    );
  }

  #retry = () => {
    this.setState((s) => ({ failed: false, generation: s.generation + 1 }));
  };

  override render(): ReactNode {
    if (!this.state.failed)
      return <Fragment key={this.state.generation}>{this.props.children}</Fragment>;
    return (
      <main className="crash-page" role="alert">
        <h1>界面出错了</h1>
        <p>
          界面遇到问题，已停止显示。后台的文本指令不受影响；正在编辑但还没保存的内容可能会丢失。
        </p>
        <p className="muted">错误类型已写入日志，不含指令内容。</p>
        <button className="primary" onClick={this.#retry}>
          重新加载界面
        </button>
      </main>
    );
  }
}
