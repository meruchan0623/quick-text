// 「更新」 in 设置: check the releases CI publishes, download the installer of this platform and show
// it in its folder. The state lives in App (`useUpdates`), so a check made at startup marks the
// 设置 button and a download goes on while another page is shown. Talks to the backend through
// ./lib/update; nothing is opened from the webview.
import { useEffect, useRef, useState } from 'react';
import { errorText } from './ipc/errors';
import type { Unlisten } from './ipc/transport';
import {
  appVersion,
  checkUpdate,
  downloadUpdate,
  isPrerelease,
  onUpdateProgress,
  revealUpdate,
  type UpdateCheck,
  type UpdateDownload,
  type UpdateProgress,
} from './lib/update';

export const PRERELEASE_KEY = 'quicktext.update.include-prerelease';
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
  tag: string;
  running: boolean;
  progress: UpdateProgress | null;
  result: UpdateDownload | null;
  error: string;
}

export interface UpdateController {
  /** The version running, once a check has said it. */
  current: string | null;
  result: UpdateCheck | null;
  checking: boolean;
  error: string;
  includePrerelease: boolean;
  setIncludePrerelease: (value: boolean) => void;
  autoCheck: boolean;
  setAutoCheck: (value: boolean) => void;
  check: () => Promise<void>;
  download: DownloadState | null;
  startDownload: () => Promise<void>;
  reveal: () => Promise<void>;
  revealError: string;
}

/**
 * Update state for the app's lifetime. With 「自动检查更新」 on, checks once, silently,
 * `autoCheckDelayMs` after mount; null never checks by itself (the mock backend, tests).
 */
export function useUpdates(autoCheckDelayMs: number | null): UpdateController {
  // The stored choice, also as a ref: a check started in the same event reads it.
  const [storedPrerelease, setStoredPrerelease] = useState(() => readFlag(PRERELEASE_KEY));
  const prerelease = useRef(storedPrerelease);
  const [autoCheck, setAutoCheckState] = useState(() => readFlag(AUTO_CHECK_KEY) ?? true);
  const [result, setResult] = useState<UpdateCheck | null>(null);
  const [checking, setChecking] = useState(false);
  const [error, setError] = useState('');
  const [download, setDownload] = useState<DownloadState | null>(null);
  const [revealError, setRevealError] = useState('');
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
  // Without a choice of its own, a test build looks for test builds too.
  const includePrerelease = storedPrerelease ?? (current !== null && isPrerelease(current));
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
      const stored = prerelease.current;
      const include = stored ?? (current !== null && isPrerelease(current));
      let answer = await checkUpdate(include);
      // The first check tells the version: a test build without a choice looks again with them.
      if (stored === null && !include && isPrerelease(answer.current))
        answer = await checkUpdate(true);
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

  const setIncludePrerelease = (value: boolean) => {
    writeFlag(PRERELEASE_KEY, value);
    prerelease.current = value;
    setStoredPrerelease(value);
    // What was shown answered the other choice: ask again.
    if (result !== null || error !== '' || checking) void run(false);
  };

  const setAutoCheck = (value: boolean) => {
    writeFlag(AUTO_CHECK_KEY, value);
    setAutoCheckState(value);
  };

  const startDownload = async () => {
    const latest = result?.latest;
    if (!latest || download?.running) return;
    const { tag } = latest;
    setRevealError('');
    setDownload({ tag, running: true, progress: null, result: null, error: '' });
    try {
      unlisten.current?.();
      unlisten.current = await onUpdateProgress((p) => {
        if (p.tag !== tag) return;
        setDownload((d) => (d?.tag === tag && d.running ? { ...d, progress: p } : d));
      });
    } catch {
      // No progress events: the download still runs, without a bar.
    }
    try {
      const done = await downloadUpdate(tag);
      setDownload((d) => (d?.tag === tag ? { ...d, running: false, result: done } : d));
    } catch (e) {
      setDownload((d) => (d?.tag === tag ? { ...d, running: false, error: message(e) } : d));
    } finally {
      unlisten.current?.();
      unlisten.current = null;
    }
  };

  const reveal = async () => {
    const path = download?.result?.path;
    if (!path) return;
    setRevealError('');
    try {
      await revealUpdate(path);
    } catch (e) {
      setRevealError(message(e));
    }
  };

  return {
    current,
    result,
    checking,
    error,
    includePrerelease,
    setIncludePrerelease,
    autoCheck,
    setAutoCheck,
    check: () => run(false),
    download,
    startDownload,
    reveal,
    revealError,
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
  const date = releaseDate(latest?.published_at ?? null);
  const fetched = download !== null && latest !== null && download.tag === latest.tag;
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
            disabled={checking || (download?.running ?? false)}
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
              {latest.prerelease && <span className="capability-tag experimental">测试版</span>}
              {date && <small className="muted">{date}</small>}
            </div>
            {latest.notes.trim() && (
              <details className="update-notes">
                <summary>更新内容</summary>
                <pre>{latest.notes}</pre>
              </details>
            )}
            {latest.asset === null ? (
              <p className="update-error">这个版本没有适用于本机的安装包。</p>
            ) : (
              <div className="update-download">
                {!(fetched && download.result) && (
                  <button
                    className="primary"
                    disabled={fetched && download.running}
                    onClick={() => void updates.startDownload()}
                  >
                    {fetched && download.running
                      ? '正在下载…'
                      : `下载（${sizeText(latest.asset.size)}）`}
                  </button>
                )}
                {fetched && download.running && (
                  <Progress progress={download.progress} size={latest.asset.size} />
                )}
                {fetched && download.result && (
                  <>
                    {download.result.verified ? (
                      <span className="update-ok">已下载 · 校验通过</span>
                    ) : (
                      <span className="update-warning">已下载 · 未能校验</span>
                    )}
                    <button onClick={() => void updates.reveal()}>在文件夹中显示</button>
                  </>
                )}
              </div>
            )}
            {fetched && download.error && (
              <p className="update-error" role="alert">
                下载失败：{download.error}
              </p>
            )}
            {updates.revealError && (
              <p className="update-error" role="alert">
                {updates.revealError}
              </p>
            )}
          </div>
        )}
        <div className="inline update-options">
          <label>
            <input
              type="checkbox"
              checked={updates.includePrerelease}
              onChange={(e) => updates.setIncludePrerelease(e.target.checked)}
            />
            包含测试版
          </label>
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
