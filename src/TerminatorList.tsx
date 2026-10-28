import { effectiveTerminators } from './lib/terminators';
import { keyNames } from './model';

/** 触发字符 column: supported keys, then the unsupported ones marked as such. */
export function TerminatorList({
  keys: list,
  platform,
}: {
  keys: string[];
  platform?: string | undefined;
}) {
  const usable = effectiveTerminators(list, platform);
  const unusable = list.filter((k) => !usable.includes(k));
  return (
    <>
      {usable.map((k) => keyNames[k] || k).join(' / ') || '—'}
      {unusable.length > 0 && (
        <small className="muted">
          {' '}
          {unusable.map((k) => keyNames[k] || k).join('、')} 暂不支持
        </small>
      )}
    </>
  );
}
