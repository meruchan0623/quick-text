import { useState } from 'react';
type Row = { id: string; name: string; group: number };
const serialize = (xs: Row[]) => xs.map((r) => `${r.name}={$${r.group}}`).join('&');
const parseRows = (value: string): Row[] =>
  value
    ? value.split('&').map((part) => {
        const p = part.indexOf('=');
        const capture = part.slice(p + 1).match(/^\{\$(\d+)\}$/);
        return {
          id: crypto.randomUUID(),
          name: p < 0 ? part : part.slice(0, p),
          group: capture ? Number(capture[1]) : 0,
        };
      })
    : [];
export default function CaptureTemplate({
  value,
  onChange,
}: {
  value: string;
  onChange: (s: string) => void;
}) {
  const [rows, setRows] = useState<Row[]>(() => parseRows(value));
  // Re-parse when the template changes from outside (not from our own edits).
  const [seen, setSeen] = useState(value);
  if (seen !== value) {
    setSeen(value);
    setRows((current) => (value === serialize(current) ? current : parseRows(value)));
  }
  const compatible = rows.every((r) => r.group > 0);
  const save = (xs: Row[]) => {
    setRows(xs);
    onChange(serialize(xs));
  };
  return (
    <div className="capture-template">
      {compatible ? (
        <>
          {rows.map((r, i) => (
            <div className="inline" key={r.id}>
              <input
                aria-label={`参数 ${i + 1} 名称`}
                placeholder="参数名称"
                value={r.name}
                onChange={(e) =>
                  save(rows.map((x, j) => (i === j ? { ...x, name: e.target.value } : x)))
                }
              />
              <span>=</span>
              <select
                aria-label={`参数 ${i + 1} 括号中的文字组`}
                value={r.group}
                onChange={(e) =>
                  save(rows.map((x, j) => (i === j ? { ...x, group: Number(e.target.value) } : x)))
                }
              >
                {Array.from({ length: 32 }, (_, n) => (
                  <option key={n + 1} value={n + 1}>
                    括号里的第 {n + 1} 组文字
                  </option>
                ))}
              </select>
              <button onClick={() => save(rows.filter((_, j) => i !== j))}>删除</button>
            </div>
          ))}
          <button
            onClick={() =>
              save([
                ...rows,
                {
                  id: crypto.randomUUID(),
                  name: `param${rows.length + 1}`,
                  group: rows.length + 1,
                },
              ])
            }
          >
            ＋ 添加参数
          </button>
        </>
      ) : (
        <p className="muted">含自定义写法，请在下方高级编辑中修改。</p>
      )}
      <details>
        <summary>查看或直接修改组合方式（高级）</summary>
        <input
          aria-label="括号文字的组合方式"
          placeholder="param1={$1}&param2={$2}"
          value={value}
          onChange={(e) => onChange(e.target.value)}
        />
      </details>
    </div>
  );
}
