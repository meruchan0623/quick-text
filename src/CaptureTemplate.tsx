import { useEffect, useState } from 'react';
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
  useEffect(() => {
    setRows((current) => (value === serialize(current) ? current : parseRows(value)));
  }, [value]);
  const compatible = rows.every((r) => r.group > 0);
  const save = (xs: Row[]) => {
    setRows(xs);
    onChange(serialize(xs));
  };
  return (
    <div className="capture-template">
      <small>
        把匹配规则中括号里的文字取出来，作为附带内容。例如规则中的第一对括号对应“第 1
        组”。用于网址时，取出的文字会自动转换为网址可用的格式，且只转换一次。
      </small>
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
        <p className="muted">
          导入的内容含有自定义写法，已保留原文；需要修改时可展开下方高级编辑。
        </p>
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
