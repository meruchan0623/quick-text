// Where errors go besides the error boundary's page. React 19 reports render errors on the root, and
// the webview reports any other uncaught error and unhandled rejection; by default both print the
// error itself on the console, message and component stack included, which can quote rule content.
// A production build replaces that: one console line with the kind of error, its class and the
// component names (see lib/errorReport.ts), and a `log_frontend_error` report of what the error
// boundary has not reported already. Development keeps the defaults, which show everything, for
// debugging.
import type { RootOptions } from 'react-dom/client';
import { invoke } from './ipc/transport';
import { componentFrames, errorClass, frontendErrorReport } from './lib/errorReport';

export type ErrorKind = 'caught' | 'uncaught' | 'recoverable' | 'global' | 'rejection';

const KIND: Record<ErrorKind, string> = {
  caught: 'a render error, caught by the error boundary',
  uncaught: 'a render error no error boundary caught',
  recoverable: 'a render error React recovered from',
  global: 'an uncaught error',
  rejection: 'an unhandled promise rejection',
};

/** Prints what may be printed of an error: its kind, its class and the component names. */
export function logFrontendError(kind: ErrorKind, error: unknown, componentStack?: string): void {
  const lines = [`Quicker Text: ${KIND[kind]} (${errorClass(error)}); the message is not shown.`];
  const components = componentFrames(componentStack);
  if (components) lines.push(components);
  console.error(lines.join('\n'));
}

/** Sends the report of an error to `log_frontend_error`. */
export function reportFrontendError(error: unknown): void {
  // A 0.3 backend has no such command, and a failed report must not fail the page.
  invoke<null>('log_frontend_error', frontendErrorReport(error)).catch(() => undefined);
}

export type RootErrorOptions = Pick<
  RootOptions,
  'onCaughtError' | 'onUncaughtError' | 'onRecoverableError'
>;

/** `createRoot` options: none in development (React's defaults), the sanitized ones in production. */
export function rootErrorOptions(dev: boolean = import.meta.env.DEV): RootErrorOptions {
  if (dev) return {};
  return {
    // The boundary's componentDidCatch has reported it already.
    onCaughtError: (error, info) => {
      logFrontendError('caught', error, info.componentStack);
    },
    onUncaughtError: (error, info) => {
      logFrontendError('uncaught', error, info.componentStack);
      reportFrontendError(error);
    },
    onRecoverableError: (error, info) => {
      logFrontendError('recoverable', error, info.componentStack);
      reportFrontendError(error);
    },
  };
}

/**
 * In production, handles the uncaught errors and unhandled rejections outside React's rendering (an
 * event handler, a timer, a promise nobody awaits) in the same way, instead of the webview printing
 * them; development keeps the webview's output. Returns the function that removes the handlers.
 */
export function installGlobalErrorHandlers(
  target: Window = window,
  dev: boolean = import.meta.env.DEV,
): () => void {
  if (dev) return () => undefined;
  // Cancelling the event is what keeps the webview from printing the error.
  const onError = (event: ErrorEvent) => {
    event.preventDefault();
    logFrontendError('global', event.error);
    reportFrontendError(event.error);
  };
  const onRejection = (event: PromiseRejectionEvent) => {
    event.preventDefault();
    logFrontendError('rejection', event.reason);
    reportFrontendError(event.reason);
  };
  target.addEventListener('error', onError);
  target.addEventListener('unhandledrejection', onRejection);
  return () => {
    target.removeEventListener('error', onError);
    target.removeEventListener('unhandledrejection', onRejection);
  };
}
