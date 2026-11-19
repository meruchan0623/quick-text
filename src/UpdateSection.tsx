// 「更新」 in 设置: the same check as the background auto-update (both TSim update servers, the newer
// version wins), then download and verify it, and restart into it. The state lives in App
// (`useUpdates`), so a check made at startup marks the 设置 button and a download goes on while
// another page is shown. Talks to the backend through ./lib/update; nothing is opened from the
// webview.
import { useEffect, useRef, useState } from 'react';
import { errorText } from './ipc/errors';
import type { Unlisten } from './ipc/transport';
import {
  appVersion,
  checkUpdate,
  downloadUpdate,
  installUpdate,
  onUpdateProgress,
  type UpdateCheck,
  type UpdateProgress,
} from './lib/update';

export const AUTO_CHECK_KEY = 'quicktext.update.auto-check';
/** The startup check waits this long, so it does not compete with loading the configuration. */
export const AUTO_CHECK_DELAY_MS = 3000;

/** A stored yes/no, or null when none is stored or storage is not available. */
function readFlag(key: string): boolean | null {
  try {
    const value = localStorage.getItem(key);
    return value === null ? null : value === '1';
  } catch {
    return null;
  }
}

function writeFlag(key: string, value: boolean): void {
  try {
    localStorage.setItem(key, value ? '1' : '0');
  } catch {
    // Not stored: the choice holds for this session.
  }
}

const message = (e: unknown) => errorText(e) ?? '已取消';

export interface DownloadState {
  version: string;
  running: boolean;
  progress: UpdateProgress | null;
  done: boolean;
  error: string;
}

export interface UpdateController {
  /** The version running, once known. */
  current: string | null;
  result: UpdateCheck | null;
  checking: boolean;
  error: string;
  autoCheck: boolean;
  setAutoCheck: (value: boolean) => void;
  check: () => Promise<void>;
  download: DownloadState | null;
  startDownload: () => Promise<void>;
  installing: boolean;
  install: () => Promise<void>;
  installError: string;
}

/**
 * Update state for the app's lifetime. With 「自动检查更新」 on, checks once, silently,
 * `autoCheckDelayMs` after mount; null never checks by itself (the mock backend, tests).
 */
export function useUpdates(autoCheckDelayMs: number | null): UpdateController {
  const [autoCheck, setAutoCheckState] = useState(() => readFlag(AUTO_CHECK_KEY) ?? true);
  const [result, setResult] = useState<UpdateCheck | null>(null);
  const [checking, setChecking] = useState(false);
  const [error, setError] = useState('');
  const [download, setDownload] = useState<DownloadState | null>(null);
  const [installing, setInstalling] = useState(false);
  const [installError, setInstallError] = useState('');
  const [version, setVersion] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    appVersion().then(
      (v) => {
        if (live) setVersion(v);
      },
      () => {},
    );
    return () => {
      live = false;
    };
  }, []);
  const current = result?.current ?? version;
  // Checks overlap (the startup one and a click): only the last one's answer is shown.
  const checks = useRef(0);
  const unlisten = useRef<Unlisten | null>(null);
  useEffect(
    () => () => {
      unlisten.current?.();
      unlisten.current = null;
    },
    [],
  );

  const run = async (silent: boolean) => {
    const request = ++checks.current;
    if (!silent) {
      setChecking(true);
      setError('');
    }
    try {
      const answer = await checkUpdate();
      if (request !== checks.current) return;
      setResult(answer);
      setError('');
    } catch (e) {
      if (request === checks.current && !silent) setError(message(e));
    } finally {
      if (request === checks.current) setChecking(false);
    }
  };

  const runRef = useRef(run);
  useEffect(() => {
    runRef.current = run;
  });
  const autoDelay = autoCheck ? autoCheckDelayMs : null;
  const autoDone = useRef(false);
  useEffect(() => {
    if (autoDelay === null || autoDone.current) return;
    const timer = setTimeout(() => {
      autoDone.current = true;
      void runRef.current(true);
    }, autoDelay);
    return () => clearTimeout(timer);
  }, [autoDelay]);

  const setAutoCheck = (value: boolean) => {
    writeFlag(AUTO_CHECK_KEY, value);
    setAutoCheckState(value);
  };

  const startDownload = async () => {
    const latest = result?.latest;
    if (!latest || result.ready || download?.running) return;
    const { version: target } = latest;
    setInstallError('');
    setDownload({ version: target, running: true, progress: null, done: false, error: '' });
    try {
      unlisten.current?.();
      unlisten.current = await onUpdateProgress((p) => {
        if (p.version !== target) return;
        setDownload((d) => (d?.version === target && d.running ? { ...d, progress: p } : d));
      });
    } catch {
      // No progress events: the download still runs, without a bar.
    }
    try {
      await downloadUpdate(target);
      setDownload((d) => (d?.version === target ? { ...d, running: false, done: true } : d));
    } catch (e) {
      setDownload((d) => (d?.version === target ? { ...d, running: false, error: message(e) } : d));
    } finally {
      unlisten.current?.();
      unlisten.current = null;
    }
  };

  const install = async () => {
    if (installing) return;
    setInstallError('');
    setInstalling(true);
    try {
      // Restarts the app; it returns only when the install did not start.
      await installUpdate();
    } catch (e) {
      setInstallError(message(e));
    } finally {
      setInstalling(false);
    }
  };

  return {
    current,
    result,
    checking,
    error,
    autoCheck,
    setAutoCheck,
    check: () => run(false),
    download,
    startDownload,
    installing,
    install,
    installError,
  };
}

/** Whether a newer release than the running one was found (the 设置 button's mark). */
export const updateAvailable = (updates: UpdateController): boolean =>
  !!updates.result?.update_available && updates.result.latest !== null;

const withV = (version: string) => (/^v/i.test(version) ? version : `v${version}`);

/** 「2026-11-09」 in local time; null for no date or one that does not parse. */
export function releaseDate(iso: string | null): string | null {
  if (!iso) return null;
  const at = new Date(iso);
  if (Number.isNaN(at.getTime())) return null;
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${at.getFullYear()}-${pad(at.getMonth() + 1)}-${pad(at.getDate())}`;
}

/** 「12.3 MB」. */
export function sizeText(bytes: number): string {
  if (bytes <= 0) return '0 KB';
  if (bytes < 1024 * 1024) return `${Math.max(1, Math.round(bytes / 1024))} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

function Progress({ progress, size }: { progress: UpdateProgress | null; size: number | null }) {
  const total = progress?.total ?? size;
  const done = progress?.downloaded ?? 0;
  const known = total !== null && total > 0;
  return (
    <div className="update-progress">
      <progress
        aria-label="下载进度"
        {...(known ? { value: Math.min(done, total), max: total } : {})}
      />
      <small>
        {known
          ? `${Math.floor((Math.min(done, total) / total) * 100)}% · ${sizeText(done)} / ${sizeText(total)}`
          : done > 0
            ? sizeText(done)
            : '正在下载…'}
      </small>
    </div>
  );
}

export default function UpdateSection({ updates }: { updates: UpdateController }) {
  const { current, result, checking, error, download } = updates;
  const latest = result?.update_available ? result.latest : null;
  const date = releaseDate(latest?.pub_date ?? null);
  const mine = download !== null && latest !== null && download.version === latest.version;
  const ready = latest !== null && (result?.ready === true || (mine && download.done));
  const running = mine && download.running;
  return (
    <>
      <h3>更新</h3>
      <div className="update-card">
        <div className="update-head">
          <span>
            当前版本 <strong>{current === null ? '—' : withV(current)}</strong>
          </span>
          <button
            onClick={() => void updates.check()}
            disabled={checking || (download?.running ?? false) || updates.installing}
          >
            {checking ? '正在检查…' : '检查更新'}
          </button>
        </div>
        {error && (
          <p className="update-error" role="alert">
            检查更新失败：{error}
          </p>
        )}
        {result && !latest && (
          <p className="update-latest" role="status">
            已是最新版本
          </p>
        )}
        {latest && (
          <div className="update-found" role="status">
            <div className="update-title">
              <strong>发现新版本 {withV(latest.version)}</strong>
              {latest.source === 'test' && (
                <span className="capability-tag experimental">测试服</span>
              )}
              {date && <small className="muted">{date}</small>}
            </div>
            {latest.notes.trim() && (
              <details className="update-notes">
                <summary>更新内容</summary>
                <pre>{latest.notes}</pre>
              </details>
            )}
            <div className="update-download">
              {ready ? (
                <>
                  <span className="update-ok">已下载 · 签名校验通过</span>
                  <button
                    className="primary"
                    disabled={updates.installing}
                    onClick={() => void updates.install()}
                  >
                    {updates.installing ? '正在安装…' : '重启并更新'}
                  </button>
                </>
              ) : (
                <button
                  className="primary"
                  disabled={running}
                  onClick={() => void updates.startDownload()}
                >
                  {running ? '正在下载…' : '下载更新'}
                </button>
              )}
              {running && <Progress progress={download.progress} size={null} />}
            </div>
            {ready && (
              <small className="muted">不重启的话，下次从托盘「退出全部」时会自动安装。</small>
            )}
            {mine && download.error && (
              <p className="update-error" role="alert">
                下载失败：{download.error}
              </p>
            )}
            {updates.installError && (
              <p className="update-error" role="alert">
                {updates.installError}
              </p>
            )}
          </div>
        )}
        <div className="inline update-options">
          <label>
            <input
              type="checkbox"
              checked={updates.autoCheck}
              onChange={(e) => updates.setAutoCheck(e.target.checked)}
            />
            自动检查更新
          </label>
        </div>
      </div>
    </>
  );
}
