import { Fragment, useId, useRef, useState } from 'react';
import { call } from './ipc/transport';
import { diagnosticText, parseCommandError } from './ipc/errors';
import { choiceCapability, choiceOffer, findCapability } from './lib/capabilities';
import {
  actionChoice,
  actionLabel,
  actionOptions,
  clone,
  Data,
  offeredChoices,
  Rule,
} from './model';
import { moveItem, withoutKey } from './lib/list';
import {
  actionDiagnostics,
  isInvalidNumber,
  numberText,
  rangeError,
  readNumber,
  type InvalidNumber,
} from './lib/numberFields';
import { inRange, ranges } from './lib/ruleShape';
import { ActionTypePicker } from './ActionTypePicker';
import { LinesInput } from './LinesInput';
import { NumberField } from './NumberField';
import { describeExecution } from './lib/describe';
import { parseRecording } from './lib/sendKeys';
import { KeyRecorder } from './KeyRecorder';
type Props = {
  platform?: string | undefined;
  rules: Rule[];
  action: Data;
  diagnostics?: Data[];
  /** `get_capabilities` entries, as the backend sent them. */
  capabilities?: unknown;
  onChange: (a: Data) => void;
  run: (f: () => Promise<any>, s?: string) => Promise<any>;
  show: (d: { title: string; data: any }) => void;
};
/** A value of the reusable-action form; invalid number text is kept as a marker. */
type ParameterValue = string | number | boolean | null | InvalidNumber;
// 「#13」 is a Windows key code. Its text is checked against the code's range (1..=255) before it
// is converted. Text that is not such a code is kept in the rule as an InvalidNumber marker, which
// the editor refuses to save or send (src/lib/numberFields.ts), so it never writes a code past u16
// that the backend cannot read, and the text typed survives switching steps.
const key = (name: string) =>
  name === '$primary'
    ? { type: 'platform_primary' }
    : name.startsWith('#')
      ? { type: 'windows_code', code: readNumber(name.slice(1), ranges.windowsKeyCode) }
      : { type: 'named', name };
const keyValue = (k: Data): string =>
  k.type === 'platform_primary'
    ? '$primary'
    : k.type === 'windows_code'
      ? `#${numberText(k.code)}`
      : k.name;
const invalidKey = (k: Data) =>
  k.type === 'windows_code' && !inRange(ranges.windowsKeyCode, k.code);
const namedKeys = [
  'Ctrl',
  'Shift',
  'Alt',
  'Win',
  '$primary',
  'Enter',
  'Tab',
  'Escape',
  'Space',
  'Backspace',
  'Delete',
  'Left',
  'Right',
  'Up',
  'Down',
  'Home',
  'End',
  ...Array.from({ length: 24 }, (_, i) => `F${i + 1}`),
  ...'ABCDEFGHIJKLMNOPQRSTUVWXYZ'.split(''),
  ...Array.from({ length: 10 }, (_, i) => `D${i}`),
];
const commands = [
  'input',
  'input2',
  'sendkeys',
  'delay',
  'paste',
  'keydown',
  'keyup',
  'keypress',
  'hotkey',
  'moveto',
  'move',
  'click',
  'dbclick',
  'down',
  'up',
  'wheel',
  'wheeldelta',
  'hwheel',
  'hwheeldelta',
  'pastefile',
  'pasteimage',
];
const cmdNames: Record<string, string> = {
  input: '键入文本',
  input2: '键入含换行或制表符的文字',
  sendkeys: '按键组合',
  delay: '等待',
  paste: '粘贴文本',
  keydown: '按住键',
  keyup: '松开键',
  keypress: '按一下键',
  hotkey: '组合快捷键',
  moveto: '移动到坐标',
  move: '从当前位置移动',
  click: '单击',
  dbclick: '双击',
  down: '按住鼠标键',
  up: '松开鼠标键',
  wheel: '上下滚动',
  wheeldelta: '少量上下滚动',
  hwheel: '左右滚动',
  hwheeldelta: '少量左右滚动',
  pastefile: '粘贴文件',
  pasteimage: '粘贴图片',
};
const stepLabel = (o: Data): string =>
  (
    ({
      text: o.method === 'paste' ? '粘贴文字' : '逐字键入',
      delay: '等待',
      key_down: '按住键',
      key_up: '松开键',
      key_press: '按一下键',
      hotkey: '按快捷键',
      mouse_move: '移动鼠标',
      mouse_button: '鼠标按键',
      wheel: '滚动',
      paste_files: '粘贴文件',
      paste_image: '粘贴图片',
    }) as Record<string, string>
  )[o.type] || '操作';
const replayCodes = ['MV', 'MD', 'MC', 'MU', 'MH', 'MW', 'DL', 'KD', 'KU', 'KP', 'MVD'];
const replayMap: Record<string, string> = {
  MV: 'moveto',
  MD: 'down',
  MC: 'click',
  MU: 'up',
  MH: 'hwheel',
  MW: 'wheel',
  DL: 'delay',
  KD: 'keydown',
  KU: 'keyup',
  KP: 'keypress',
  MVD: 'move',
};
function operation(c: string): Data {
  if (['input', 'input2', 'paste'].includes(c))
    return { type: 'text', text: '', method: c === 'paste' ? 'paste' : 'keys' };
  if (c === 'delay') return { type: 'delay', ms: 100 };
  if (['keydown', 'keyup', 'keypress'].includes(c))
    return {
      type: c === 'keydown' ? 'key_down' : c === 'keyup' ? 'key_up' : 'key_press',
      key: key('Enter'),
    };
  if (c === 'hotkey') return { type: 'hotkey', keys: [key('Ctrl'), key('C')] };
  if (['moveto', 'move'].includes(c))
    return { type: 'mouse_move', x: 0, y: 0, relative: c === 'move', percent: false };
  if (['click', 'dbclick', 'down', 'up'].includes(c))
    return {
      type: 'mouse_button',
      button: 'left',
      state: c === 'dbclick' ? 'double_click' : c === 'click' ? 'click' : c,
    };
  if (['wheel', 'wheeldelta', 'hwheel', 'hwheeldelta'].includes(c))
    return {
      type: 'wheel',
      axis: c.startsWith('h') ? 'horizontal' : 'vertical',
      amount: 1,
      unit: c.endsWith('delta') ? 'delta' : 'clicks',
    };
  if (c === 'pastefile') return { type: 'paste_files', paths: [] };
  return { type: 'paste_image', path: '' };
}
function defaultAction(t: string): Data {
  switch (t) {
    case 'text_paste':
    case 'text_keys':
      return {
        type: 'text',
        method: t === 'text_paste' ? 'paste' : 'keys',
        source: { type: 'inline', value: '' },
      };
    case 'send_keys':
      return { type: t, source: { type: 'inline', value: '' }, syntax_variables: [] };
    case 'html':
      return { type: t, html: '', plain_text: '' };
    case 'hotkey':
      return { type: t, keys: [key('Ctrl'), key('C')] };
    case 'steps':
    case 'replay':
      return {
        type: t,
        plan: { version: 1, dialect: 'structured', source: null, steps: [] },
        ...(t === 'replay' ? { speed: 1 } : {}),
      };
    case 'open':
      return { type: t, target: '', args: [], working_directory: null };
    case 'common':
      return { type: t, function: 'copy' };
    case 'reusable':
      return { type: t, action_id: '', parameters: {} };
    case 'quicker':
      return { type: t, action_id: '', parameter: '' };
    default:
      return { type: 'none' };
  }
}
function KeySelect({ value, onChange }: { value: Data; onChange: (k: Data) => void }) {
  // As in NumberField: the text typed stays while it still stands for `value` (「#013」); invalid
  // code text comes back from the rule itself.
  const errorId = useId();
  const [typed, setTyped] = useState<string | null>(null);
  const invalid = invalidKey(value);
  return (
    <>
      <input
        aria-label="按键名称"
        aria-invalid={invalid || undefined}
        aria-describedby={invalid ? errorId : undefined}
        list="keynames"
        value={typed !== null && keyValue(key(typed)) === keyValue(value) ? typed : keyValue(value)}
        onChange={(e) => {
          setTyped(e.target.value);
          onChange(key(e.target.value));
        }}
      />
      {invalid && (
        <small id={errorId} className="number-error">
          以 # 开头表示 Windows 键码，{rangeError(ranges.windowsKeyCode)}
        </small>
      )}
      <datalist id="keynames">
        {namedKeys.map((k) => (
          <option key={k} value={k}>
            {k === '$primary' ? '常用快捷键：Windows 用 Ctrl，Mac 用 Cmd' : k}
          </option>
        ))}
      </datalist>
    </>
  );
}
function Shortcut({
  value,
  onChange,
  platform,
}: {
  platform?: string | undefined;
  value: Data[];
  onChange: (ks: Data[]) => void;
}) {
  const [capturing, setCapturing] = useState(false);
  return (
    <div className="shortcut">
      {value.map((k, i) => (
        <div key={i}>
          <KeySelect value={k} onChange={(n) => onChange(value.map((x, j) => (i === j ? n : x)))} />
          <button aria-label="删除按键" onClick={() => onChange(value.filter((_, j) => i !== j))}>
            ×
          </button>
        </div>
      ))}
      <button onClick={() => onChange([...value, key('Enter')])}>＋ 按键</button>
      <button onClick={() => setCapturing(!capturing)}>
        {capturing ? '取消录制' : '录制快捷键'}
      </button>
      {capturing && (
        <input
          autoFocus
          aria-label="快捷键录制"
          placeholder="按下组合键…"
          readOnly
          onKeyDown={(e) => {
            e.preventDefault();
            e.stopPropagation();
            if (['Control', 'Shift', 'Alt', 'Meta'].includes(e.key)) return;
            if (e.key === 'Escape') {
              setCapturing(false);
              return;
            }
            const result: Data[] = [];
            if (e.ctrlKey) result.push(key('Ctrl'));
            if (e.shiftKey) result.push(key('Shift'));
            if (e.altKey) result.push(key('Alt'));
            if (e.metaKey) {
              if (platform !== 'macos' && platform !== 'windows') {
                setCapturing(false);
                return;
              }
              result.push(key(platform === 'macos' ? 'Cmd' : 'Win'));
            }
            let name = e.key.length === 1 ? e.key.toUpperCase() : e.key;
            name =
              (
                {
                  ArrowLeft: 'Left',
                  ArrowRight: 'Right',
                  ArrowUp: 'Up',
                  ArrowDown: 'Down',
                  ' ': 'Space',
                } as Data
              )[name] || name;
            if (/^\d$/.test(name)) name = 'D' + name;
            result.push(key(name));
            onChange(result);
            setCapturing(false);
          }}
        />
      )}
      <small>
        点“录制快捷键”后，按一次想要的组合键即可；Esc 可取消。手动选择 $primary 时，Windows 使用
        Ctrl，Mac 使用 Cmd。
      </small>
    </div>
  );
}
export default function ActionEditor({
  platform,
  rules,
  action: a,
  diagnostics: inheritedDiagnostics = [],
  capabilities = [],
  onChange,
  run,
  show,
}: Props) {
  const [diagnostics, setDiagnostics] = useState<Data[]>([]);
  // What the action had when the editor opened. Choices and fields kept only for rules that use them
  // stay on screen for the whole edit, so clearing one or switching away can be taken back.
  const [opened] = useState(() => ({
    choice: actionChoice(a),
    fileSource: a.source?.type === 'file',
    syntaxVariables: (a.syntax_variables ?? []).length > 0,
    args: (a.args ?? []).length > 0,
    workingDirectory: Boolean(a.working_directory),
    // A source the recorder cannot show opens with the advanced source box expanded.
    keysAdvanced:
      a.type === 'send_keys' &&
      a.source?.type === 'inline' &&
      parseRecording(String(a.source.value)) === null,
  }));
  const sourceEditor = useRef<HTMLTextAreaElement>(null),
    stepForm = useRef<HTMLDivElement>(null);
  const perform = async (command: string, args: Data) => {
    try {
      return await call(command, args);
    } catch (e) {
      // Validation failures point at the offending step; the caller reports the error itself.
      const error = parseCommandError(e);
      if (error.code === 'validation') setDiagnostics(error.detail.diagnostics);
      throw e;
    }
  };
  // A test sends nothing the editor would refuse to save: the same checks run first, and their
  // diagnostics go where the backend's would.
  const execute = (action: Data, found: Data[]) => {
    if (found.length > 0) {
      setDiagnostics(found);
      throw new Error('有数值需要修改，请按提示改正后再测试；已填写的内容会保留。');
    }
    return perform('execute_action', { action });
  };
  const focusDiagnostic = (d: Data) => {
    const index = String(d.path || '').match(/steps\[(\d+)\]/);
    if (index) {
      setStepIndex(Number(index[1]));
      setExpert(false);
      requestAnimationFrame(() => {
        stepForm.current?.scrollIntoView({ block: 'nearest' });
        stepForm.current?.querySelector<HTMLElement>('input,textarea,select')?.focus();
      });
    } else if (d.span?.line > 0) {
      setExpert(true);
      requestAnimationFrame(() => {
        const t = sourceEditor.current;
        if (!t) return;
        t.focus();
        const lines = source.split('\n');
        const start =
          lines.slice(0, d.span.line - 1).reduce((n, l) => n + l.length + 1, 0) +
          Math.max(0, (d.span.column || 1) - 1);
        t.setSelectionRange(start, start + Math.max(1, d.span.length || 1));
      });
    }
  };
  const [imagePreview, setImagePreview] = useState<{ path: string; url: string } | null>(null),
    [expert, setExpert] = useState(false),
    [source, setSource] = useState(''),
    [command, setCommand] = useState('input'),
    [stepIndex, setStepIndex] = useState(0),
    [history, setHistory] = useState<Data[]>([]),
    [bText, setBText] = useState(''),
    [bKey, setBKey] = useState('Enter'),
    [repeat, setRepeat] = useState<number | InvalidNumber>(1),
    [mods, setMods] = useState<string[]>([]),
    [recording, setRecording] = useState(false),
    [eventDelay, setEventDelay] = useState<number | InvalidNumber>(0);
  const dragged = useRef<number | null>(null);
  const edit = (patch: Data) => onChange({ ...a, ...patch });
  const steps: Data[] = a.plan?.steps ?? [];
  const updateSteps = (s: Data[]) => {
    setHistory((h) => [...h.slice(-19), clone(a)]);
    edit({ plan: { ...a.plan, dialect: 'structured', source: null, steps: s } });
  };
  const setOp = (patch: Data) =>
    updateSteps(
      steps.map((s, i) =>
        i === stepIndex ? { ...s, operation: { ...s.operation, ...patch } } : s,
      ),
    );
  const current: Data | undefined = steps[stepIndex];
  const o: Data = current?.operation ?? {};
  const parse = async (dialect: string, s: string) => {
    const r = await call('parse_source', { dialect, source: s });
    if (r.plan) {
      setDiagnostics([]);
      return r.plan;
    }
    if (r.steps) {
      setDiagnostics([]);
      return r;
    }
    setDiagnostics(r.diagnostics || []);
    throw new Error('内容格式有误，请点击下方提示找到需要修改的位置；已输入的内容会保留');
  };
  const addStep = () => {
    if (command === 'sendkeys') {
      // The button is disabled then too; this keeps a marker out of the text sent to parse.
      if (!inRange(ranges.repeat, repeat)) return;
      void run(async () => {
        const sequence =
          mods.map((m) => (m === 'Ctrl' ? '^' : m === 'Shift' ? '+' : '%')).join('') +
          `{${bKey} ${repeat}}`;
        const p = await parse('send_keys', sequence);
        updateSteps([...steps, ...p.steps]);
        setStepIndex(steps.length);
      });
      return;
    }
    if (command === 'input2') {
      void run(async () => {
        const p = await parse('inputscript', 'input2:' + bText);
        updateSteps([...steps, ...p.steps]);
        setStepIndex(steps.length);
      });
      return;
    }
    const cmd = a.type === 'replay' ? replayMap[command] || 'moveto' : command;
    // As above: the button is disabled while the delay is invalid, and no marker gets past here.
    if (a.type === 'replay' && !inRange(ranges.delayMs, eventDelay)) return;
    const delay = inRange(ranges.delayMs, eventDelay) ? eventDelay : 0;
    updateSteps([
      ...steps,
      ...(a.type === 'replay' && delay > 0
        ? [
            {
              operation: { type: 'delay', ms: delay },
              span: { line: steps.length + 1, column: 1, length: 0 },
              target: { type: 'locked' },
            },
          ]
        : []),
      {
        operation: operation(cmd),
        span: { line: steps.length + 1, column: 1, length: 0 },
        target: { type: 'locked' },
      },
    ]);
    setStepIndex(steps.length);
  };
  const choosePath = (kind: string, fn: (p: string[]) => void) =>
    run(async () => {
      const p = await call<string[] | null>('choose_file', { kind });
      if (p?.length) fn(p);
    });
  // Principle 5 (S-16): the capability the chosen action needs, and how every action type is
  // offered by the capability it needs (the type the action has keeps a selectable entry).
  const needed = findCapability(capabilities, choiceCapability(actionChoice(a)), platform);
  const offer = (choice: string) =>
    choiceOffer(
      findCapability(capabilities, choiceCapability(choice), platform),
      choice === actionChoice(a),
    );
  return (
    <>
      {[...inheritedDiagnostics, ...diagnostics].length > 0 && (
        <div className="action-diagnostics" role="alert">
          <strong>需要修改的设置 · 点击查看位置</strong>
          {[...inheritedDiagnostics, ...diagnostics].map((d, i) => {
            const step = /steps\[(\d+)\]/.exec(String(d.path || ''));
            return (
              <button key={i} onClick={() => focusDiagnostic(d)}>
                {step ? `步骤 ${Number(step[1]) + 1}` : '操作设置'}
                {d.span?.line > 0
                  ? ` · 第 ${d.span.line} 行 / ${d.span.column || 1} 列`
                  : ''} · {diagnosticText(d)}
              </button>
            );
          })}
        </div>
      )}
      {needed && needed.presentation !== 'available' && (
        <div className={`capability-warning ${needed.presentation}`}>
          <strong>
            {needed.usable ? (
              <>
                <span className={`capability-tag ${needed.presentation}`}>{needed.tag}</span>{' '}
                {needed.name}
              </>
            ) : (
              '当前电脑暂不支持此操作'
            )}
          </strong>
          <p>
            {needed.id === 'quicker_private_action'
              ? '当前版本不能执行原 Quicker 动作；请改成这里支持的操作。'
              : (needed.reason ?? '暂无额外说明')}
          </p>
        </div>
      )}
      <label className="field">
        <span>操作类型</span>
        <ActionTypePicker
          value={actionChoice(a)}
          platform={platform}
          options={actionOptions.flatMap(([k, v]) => {
            // Types not offered for new rules keep an entry on a rule that has one.
            if (!offeredChoices.has(k) && k !== actionChoice(a) && k !== opened.choice) return [];
            const { hidden, disabled, note } = offer(k);
            return hidden ? [] : [{ value: k, label: `${v}${note}`, disabled }];
          })}
          onChange={(choice) => {
            onChange(
              a.type === 'text' && choice.startsWith('text_')
                ? { ...a, method: choice === 'text_paste' ? 'paste' : 'keys' }
                : defaultAction(choice),
            );
            setDiagnostics([]);
            setExpert(false);
            setSource('');
            setStepIndex(0);
            setHistory([]);
          }}
        />
      </label>
      {(a.type === 'text' || a.type === 'send_keys') && (
        <>
          {/* Reading a text file is kept for rules that already do; new rules type the text here. */}
          {(opened.fileSource || a.source.type === 'file') && (
            <div className="inline">
              <label>
                要输出的文字来自哪里{' '}
                <select
                  value={a.source.type}
                  onChange={(e) =>
                    edit({
                      source:
                        e.target.value === 'file'
                          ? { type: 'file', path: '', encoding: 'utf8' }
                          : { type: 'inline', value: '' },
                    })
                  }
                >
                  <option value="inline">在这里填写文字</option>
                  <option value="file">读取文字文件</option>
                </select>
              </label>
            </div>
          )}
          {a.source.type === 'file' ? (
            <>
              <label className="field">
                <span>文字文件的位置</span>
                <input
                  value={a.source.path}
                  onChange={(e) => edit({ source: { ...a.source, path: e.target.value } })}
                />
              </label>
              <div className="inline">
                <button
                  onClick={() =>
                    choosePath('text', (p) => edit({ source: { ...a.source, path: p[0] } }))
                  }
                >
                  选择文件
                </button>
                <select
                  aria-label="文件编码"
                  value={a.source.encoding}
                  onChange={(e) => edit({ source: { ...a.source, encoding: e.target.value } })}
                >
                  {Object.entries({
                    utf8: 'UTF-8（常用）',
                    utf16_le: 'UTF-16 小端',
                    utf16_be: 'UTF-16 大端',
                    gbk: 'GBK（旧版中文文件）',
                  }).map(([value, label]) => (
                    <option key={value} value={value}>
                      {label}
                    </option>
                  ))}
                </select>
                <small>文字显示乱码时，可尝试更换文件编码；不确定时先选 UTF-8。</small>
                <button
                  onClick={() =>
                    run(async () =>
                      show({
                        title: '文件内容预览',
                        data: await call('preview_file', { source: a.source }),
                      }),
                    )
                  }
                >
                  预览文件内容
                </button>
              </div>
            </>
          ) : a.type === 'send_keys' ? (
            <>
              <KeyRecorder
                value={String(a.source.value)}
                onChange={(value) => edit({ source: { ...a.source, value } })}
              />
              <details className="advanced-settings sendkeys-advanced" open={opened.keysAdvanced}>
                <summary>高级：直接编辑按键代码</summary>
                <textarea
                  aria-label="操作正文"
                  className="content code"
                  value={a.source.value}
                  onChange={(e) => edit({ source: { ...a.source, value: e.target.value } })}
                />
                <button
                  onClick={() =>
                    run(async () =>
                      show({
                        title: '按键内容检查结果',
                        data: await parse('send_keys', a.source.value),
                      }),
                    )
                  }
                >
                  检查并查看将按哪些键
                </button>
                <details className="sendkeys-cheatsheet">
                  <summary>按键代码写法速查</summary>
                  <table>
                    <tbody>
                      {[
                        ['你好{Enter}', '输入“你好”，再按一次 Enter'],
                        ['{Enter 3}', '连按 3 次 Enter（花括号内空格后写次数）'],
                        ['{Tab}、{Esc}、{Backspace}', '按 Tab、Esc、退格；方向键写 {Left} {Up} 等'],
                        ['^a', 'Ctrl+A（^ 代表 Ctrl）'],
                        ['+{Tab}', 'Shift+Tab（+ 代表 Shift）'],
                        ['%{F4}', 'Alt+F4（% 代表 Alt）'],
                        ['^(ac)', '按住 Ctrl 依次按 A、C'],
                        ['{+}、{%}、{{}', '原样输出 + % { 等特殊符号'],
                      ].map(([code, meaning]) => (
                        <tr key={code}>
                          <td>
                            <code>{code}</code>
                          </td>
                          <td>{meaning}</td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </details>
                <small>
                  上面的录制框和这里是同一份内容，改哪边都可以。需要 Windows 键、播放等媒体键或
                  F17–F24 时，请选择“按顺序执行多个操作”。
                </small>
              </details>
            </>
          ) : (
            <>
              <textarea
                aria-label="操作正文"
                className="content"
                value={a.source.value}
                onChange={(e) => edit({ source: { ...a.source, value: e.target.value } })}
              />
              <div className="inline">
                {['{cliptext}', '{selection}', '{0:yyyy-MM-dd}', '{0:HH:mm:ss}'].map((v) => (
                  <button
                    key={v}
                    onClick={() =>
                      edit({ source: { ...a.source, value: String(a.source.value) + v } })
                    }
                  >
                    {
                      (
                        {
                          '{cliptext}': '插入已复制的文字',
                          '{selection}': '插入选中的文字',
                          '{0:yyyy-MM-dd}': '插入今天的日期',
                          '{0:HH:mm:ss}': '插入当前时间',
                        } as Data
                      )[v]
                    }
                  </button>
                ))}
              </div>
              <small>这些按钮会加入一个标记，执行指令时再换成当时的文字、日期或时间。</small>
            </>
          )}
          {/* Only for a rule already running copied text as key commands. */}
          {a.type === 'send_keys' &&
            (opened.syntaxVariables || (a.syntax_variables ?? []).length > 0) && (
              <div className="dynamic-source">
                <h4>来自剪贴板或选中文字的处理方式</h4>
                {[
                  ...new Set([
                    'cliptext',
                    'selection',
                    ...(a.source.type === 'inline'
                      ? [...String(a.source.value).matchAll(/\{(param:[^}]+)\}/g)].map((m) => m[1])
                      : []),
                    ...(a.syntax_variables ?? []),
                  ]),
                ].map((token) => (
                  <label className="field" key={token}>
                    <span>
                      {token === 'cliptext'
                        ? '已复制的文字'
                        : token === 'selection'
                          ? '选中的文字'
                          : `传入的内容：${token.replace('param:', '')}`}
                    </span>
                    <select
                      value={(a.syntax_variables ?? []).includes(token) ? 'syntax' : 'literal'}
                      onChange={(e) =>
                        edit({
                          syntax_variables:
                            e.target.value === 'syntax'
                              ? [...new Set([...(a.syntax_variables ?? []), token])]
                              : (a.syntax_variables ?? []).filter((x: string) => x !== token),
                        })
                      }
                    >
                      <option value="literal">直接输出文字（推荐）</option>
                      <option value="syntax">作为按键命令执行（先检查格式）</option>
                    </select>
                  </label>
                ))}
                <small>
                  默认把复制或选中的内容当作普通文字。只有选择“作为按键命令执行”的项目才会触发按键操作。此操作类型读取文件时，也会把文件内容当作按键命令检查和执行。
                </small>
              </div>
            )}
        </>
      )}
      {a.type === 'html' && (
        <>
          <label className="field">
            <span>带格式文字的 HTML 代码</span>
            <textarea
              className="content code"
              value={a.html}
              onChange={(e) => edit({ html: e.target.value })}
            />
          </label>
          <label className="field">
            <span>无法显示格式时使用的普通文字</span>
            <textarea value={a.plain_text} onChange={(e) => edit({ plain_text: e.target.value })} />
          </label>
          <details>
            <summary>查看文字效果（不会运行网页脚本）</summary>
            <iframe title="HTML 预览" sandbox="" srcDoc={a.html} />
          </details>
        </>
      )}
      {a.type === 'hotkey' && (
        <Shortcut platform={platform} value={a.keys} onChange={(v) => edit({ keys: v })} />
      )}
      {a.type === 'open' && (
        <>
          <label className="field">
            <span>网址、文件位置或程序位置</span>
            <input value={a.target} onChange={(e) => edit({ target: e.target.value })} />
          </label>
          <button onClick={() => choosePath('file', (p) => edit({ target: p[0] }))}>
            选择文件或应用
          </button>
          {/* Shown only on a rule that had them when opened, so they can be changed or cleared. */}
          {(opened.args || a.args.length > 0) && (
            <label className="field">
              <span>启动程序时附带的内容（每行一项，通常留空）</span>
              <textarea
                rows={3}
                value={a.args.join('\n')}
                onChange={(e) => edit({ args: e.target.value ? e.target.value.split('\n') : [] })}
              />
            </label>
          )}
          {(opened.workingDirectory || a.working_directory) && (
            <label className="field">
              <span>程序启动时使用的文件夹（通常留空）</span>
              <input
                value={a.working_directory ?? ''}
                onChange={(e) => edit({ working_directory: e.target.value || null })}
              />
            </label>
          )}
        </>
      )}
      {a.type === 'common' && (
        <label className="field">
          <span>常用功能</span>
          <select value={a.function} onChange={(e) => edit({ function: e.target.value })}>
            {[
              'copy',
              'paste',
              'cut',
              'select_all',
              'undo',
              'redo',
              'volume_up',
              'volume_down',
              'volume_mute',
              'media_play_pause',
              'media_next',
              'media_previous',
              'browser_back',
              'browser_forward',
            ].map((f) => (
              <option key={f} value={f}>
                {
                  (
                    {
                      copy: '复制',
                      paste: '粘贴',
                      cut: '剪切',
                      select_all: '全选',
                      undo: '撤销',
                      redo: '重做',
                      volume_up: '增大音量',
                      volume_down: '减小音量',
                      volume_mute: '静音',
                      media_play_pause: '播放 / 暂停',
                      media_next: '下一曲',
                      media_previous: '上一曲',
                      browser_back: '后退',
                      browser_forward: '前进',
                      lock_screen: '锁定屏幕',
                    } as Data
                  )[f]
                }
              </option>
            ))}
          </select>
        </label>
      )}
      {(a.type === 'reusable' || a.type === 'quicker') && (
        <>
          <label className="field">
            <span>{a.type === 'quicker' ? 'Quicker 动作编号' : '选择要执行的文本指令'}</span>
            {a.type === 'reusable' ? (
              <select
                aria-label="复用已保存指令"
                value={a.action_id}
                onChange={(e) => edit({ action_id: e.target.value })}
              >
                <option value="">选择已保存的指令…</option>
                {rules.map((r) => (
                  <option key={r.id} value={r.id}>
                    {r.match.value} · {r.description || r.group} · {actionLabel(r.action)}
                  </option>
                ))}
              </select>
            ) : (
              <input value={a.action_id} onChange={(e) => edit({ action_id: e.target.value })} />
            )}
          </label>
          {a.type === 'quicker' ? (
            <>
              <label className="field">
                <span>传给该动作的内容（可选）</span>
                <input value={a.parameter} onChange={(e) => edit({ parameter: e.target.value })} />
              </label>
              <p className="muted">
                这些信息来自原来的 Quicker
                动作，当前版本只保留信息，不能执行。请改选这里支持的操作类型，再填写相应内容。
              </p>
            </>
          ) : (
            <div className="typed-parameters">
              <h4>传给另一条指令的内容（可选）</h4>
              <small>
                名称要与另一条指令需要的名称一致；内容可以是文字、数字、“是或否”，也可以留空。
              </small>
              {Object.entries<ParameterValue>(a.parameters ?? {}).map(([name, value], i) => (
                <div className="inline" key={i}>
                  <input
                    aria-label="参数名称"
                    value={name}
                    onChange={(e) => {
                      const p = withoutKey(a.parameters ?? {}, name);
                      p[e.target.value] = value;
                      edit({ parameters: p });
                    }}
                  />
                  <select
                    aria-label="参数类型"
                    value={
                      value === null ? 'null' : isInvalidNumber(value) ? 'number' : typeof value
                    }
                    onChange={(e) =>
                      edit({
                        parameters: {
                          ...a.parameters,
                          [name]:
                            e.target.value === 'boolean'
                              ? true
                              : e.target.value === 'number'
                                ? 0
                                : e.target.value === 'null'
                                  ? null
                                  : '',
                        },
                      })
                    }
                  >
                    <option value="string">文字</option>
                    <option value="number">数字</option>
                    <option value="boolean">是或否</option>
                    <option value="null">空值</option>
                  </select>
                  {typeof value === 'boolean' ? (
                    <select
                      aria-label="选择是或否"
                      value={String(value)}
                      onChange={(e) =>
                        edit({
                          parameters: { ...a.parameters, [name]: e.target.value === 'true' },
                        })
                      }
                    >
                      <option value="true">是</option>
                      <option value="false">否</option>
                    </select>
                  ) : typeof value === 'number' || isInvalidNumber(value) ? (
                    // Any JSON number within the IPC number contract (Rust takes a JSON value).
                    // Invalid text is kept as a marker, which saving and testing refuse.
                    <NumberField
                      aria-label="参数值"
                      range={ranges.json}
                      value={value}
                      onChange={(n) => edit({ parameters: { ...a.parameters, [name]: n } })}
                    />
                  ) : (
                    <input
                      aria-label="参数值"
                      disabled={value === null}
                      value={value === null ? '' : value}
                      onChange={(e) =>
                        edit({ parameters: { ...a.parameters, [name]: e.target.value } })
                      }
                    />
                  )}
                  <button
                    onClick={() => edit({ parameters: withoutKey(a.parameters ?? {}, name) })}
                  >
                    删除
                  </button>
                </div>
              ))}
              <button
                onClick={() =>
                  edit({
                    parameters: {
                      ...a.parameters,
                      [`param${Object.keys(a.parameters ?? {}).length + 1}`]: '',
                    },
                  })
                }
              >
                ＋ 添加参数
              </button>
              <small>
                执行时会使用当前已保存的那条指令。如果两条指令互相调用，或最终调用回自己，软件会停止执行。
              </small>
            </div>
          )}
        </>
      )}
      {(a.type === 'steps' || a.type === 'replay') && (
        <>
          <div className="inline">
            <label>
              <input
                type="checkbox"
                checked={expert}
                onChange={(e) => setExpert(e.target.checked)}
              />
              直接编辑操作命令（高级）
            </label>
            {a.type === 'replay' && (
              <label>
                倍速{' '}
                <NumberField
                  className="number"
                  range={ranges.replaySpeed}
                  value={a.speed}
                  onChange={(n) => edit({ speed: n })}
                />
              </label>
            )}
            <button
              disabled={!history.length}
              onClick={() => {
                const prev = history.at(-1);
                if (!prev) return;
                onChange(prev);
                setHistory(history.slice(0, -1));
                setStepIndex(0);
              }}
            >
              撤销
            </button>
          </div>
          {expert ? (
            <>
              <textarea
                ref={sourceEditor}
                aria-label="操作命令"
                className="content code"
                placeholder={
                  a.type === 'replay'
                    ? '0;KD;Ctrl\n0;KP;C\n0;KU;Ctrl'
                    : 'input:您好\ndelay:100\nhotkey:Ctrl+C'
                }
                value={source}
                onChange={(e) => setSource(e.target.value)}
              />
              <button
                onClick={() =>
                  run(async () => {
                    const p = await parse(a.type === 'replay' ? 'replay' : 'inputscript', source);
                    edit({ plan: p });
                    setStepIndex(0);
                  }, '命令已转换为操作步骤，请保存')
                }
              >
                检查命令并转换为步骤
              </button>
              <small>
                适合熟悉操作命令的用户。检查通过后才会转换为步骤；有错误时保留原文并提示位置。
              </small>
            </>
          ) : (
            <>
              <div className="step-toolbar">
                <select
                  aria-label="新步骤类型"
                  value={command}
                  onChange={(e) => setCommand(e.target.value)}
                >
                  {(a.type === 'replay' ? replayCodes : commands).map((c) => (
                    <option key={c} value={c}>
                      {cmdNames[replayMap[c] || c]}
                    </option>
                  ))}
                </select>
                <button
                  disabled={
                    (a.type === 'steps' &&
                      command === 'sendkeys' &&
                      !inRange(ranges.repeat, repeat)) ||
                    (a.type === 'replay' && !inRange(ranges.delayMs, eventDelay))
                  }
                  onClick={addStep}
                >
                  ＋ 添加步骤
                </button>
                <button
                  onClick={() => {
                    const sample = [
                      {
                        operation: { type: 'text', text: '您好！', method: 'keys' },
                        span: { line: 1, column: 1, length: 0 },
                        target: { type: 'locked' },
                      },
                      {
                        operation: { type: 'delay', ms: 100 },
                        span: { line: 2, column: 1, length: 0 },
                        target: { type: 'locked' },
                      },
                    ];
                    updateSteps([...steps, ...sample]);
                  }}
                >
                  问候预设
                </button>
              </div>
              {command === 'input2' && a.type === 'steps' && (
                <label className="field">
                  <span>要输入的文字（\t 表示制表符，\r\n 表示换行）</span>
                  <textarea rows={2} value={bText} onChange={(e) => setBText(e.target.value)} />
                  <small>添加步骤时，会把这些标记转换成真正的制表符或换行。</small>
                </label>
              )}
              {command === 'sendkeys' && a.type === 'steps' && (
                <div className="inline">
                  {['Ctrl', 'Shift', 'Alt'].map((m) => (
                    <label key={m}>
                      <input
                        type="checkbox"
                        checked={mods.includes(m)}
                        onChange={(e) =>
                          setMods(e.target.checked ? [...mods, m] : mods.filter((x) => x !== m))
                        }
                      />
                      {m}
                    </label>
                  ))}
                  <input
                    list="keynames"
                    aria-label="此步骤要按的键"
                    value={bKey}
                    onChange={(e) => setBKey(e.target.value)}
                  />
                  <NumberField
                    className="number"
                    aria-label="此步骤重复次数"
                    range={ranges.repeat}
                    value={repeat}
                    onChange={setRepeat}
                  />
                  <small>选好按键和重复次数后，点击“添加步骤”。</small>
                </div>
              )}
              {a.type === 'replay' && (
                <label className="field">
                  <span>执行新增操作前等待多久（毫秒，1000 毫秒是 1 秒）</span>
                  <NumberField range={ranges.delayMs} value={eventDelay} onChange={setEventDelay} />
                  <small>
                    会添加一个等待步骤。调整倍速只改变等待时间，鼠标位置和滚动距离保持不变。
                  </small>
                </label>
              )}
              <div className="step-layout">
                <ol className="steps">
                  {steps.map((s, i) => (
                    <li
                      key={i}
                      draggable
                      onDragStart={() => (dragged.current = i)}
                      onDragOver={(e) => e.preventDefault()}
                      onDrop={() => {
                        if (dragged.current !== null) {
                          updateSteps(moveItem(steps, dragged.current, i));
                          setStepIndex(i);
                          dragged.current = null;
                        }
                      }}
                      className={i === stepIndex ? 'active' : ''}
                    >
                      <button onClick={() => setStepIndex(i)}>
                        <span>{i + 1}</span>
                        {stepLabel(s.operation)}
                        <small>
                          {s.operation.text ||
                            (isInvalidNumber(s.operation.ms)
                              ? s.operation.ms.invalidNumber
                              : s.operation.ms) ||
                            s.operation.key?.name ||
                            (
                              {
                                left: '左键',
                                right: '右键',
                                middle: '中键',
                                x1: '侧键 1',
                                x2: '侧键 2',
                              } as Data
                            )[s.operation.button] ||
                            ''}
                        </small>
                      </button>
                    </li>
                  ))}
                </ol>
                <div ref={stepForm} className="step-form">
                  {!current ? (
                    <p className="muted">
                      先选择操作并点击“添加步骤”，再在这里填写文字、按键或鼠标位置。按从上到下的顺序执行，可拖动步骤调整顺序。
                    </p>
                  ) : (
                    // Keyed by step: text typed into one step's fields is not shown for another.
                    <Fragment key={stepIndex}>
                      <h4>
                        步骤 {stepIndex + 1} · {stepLabel(o)}
                      </h4>
                      {o.type === 'text' && (
                        <>
                          <textarea
                            aria-label="步骤文本"
                            rows={4}
                            value={o.text}
                            onChange={(e) => setOp({ text: e.target.value })}
                          />
                          <select
                            aria-label="步骤输出方式"
                            value={o.method}
                            onChange={(e) => setOp({ method: e.target.value })}
                          >
                            <option value="keys">一个字一个字输入</option>
                            <option value="paste">一次粘贴整段文字</option>
                          </select>
                        </>
                      )}
                      {o.type === 'delay' && (
                        <label>
                          等待多久（毫秒）{' '}
                          <NumberField
                            aria-label="等待时间（毫秒）"
                            range={ranges.delayMs}
                            value={o.ms}
                            onChange={(n) => setOp({ ms: n })}
                          />
                        </label>
                      )}
                      {['key_down', 'key_up', 'key_press'].includes(o.type) && (
                        <KeySelect value={o.key} onChange={(k) => setOp({ key: k })} />
                      )}{' '}
                      {o.type === 'hotkey' && (
                        <Shortcut
                          platform={platform}
                          value={o.keys}
                          onChange={(ks) => setOp({ keys: ks })}
                        />
                      )}
                      {o.type === 'mouse_move' && (
                        <>
                          <div className="inline">
                            <label>
                              左右位置 X{' '}
                              <NumberField
                                aria-label="左右位置 X"
                                range={o.percent ? ranges.percent : ranges.coordinate}
                                value={o.x}
                                onChange={(n) => setOp({ x: n })}
                              />
                            </label>
                            <label>
                              上下位置 Y{' '}
                              <NumberField
                                aria-label="上下位置 Y"
                                range={o.percent ? ranges.percent : ranges.coordinate}
                                value={o.y}
                                onChange={(n) => setOp({ y: n })}
                              />
                            </label>
                          </div>
                          <label>
                            <input
                              type="checkbox"
                              checked={o.relative}
                              onChange={(e) => setOp({ relative: e.target.checked })}
                            />
                            从鼠标当前位置移动
                          </label>
                          <label>
                            <input
                              type="checkbox"
                              checked={o.percent}
                              onChange={(e) => setOp({ percent: e.target.checked })}
                            />
                            用主屏幕宽、高的百分比填写位置
                          </label>
                          <button
                            onClick={() =>
                              run(async () => {
                                const p = await call('get_cursor');
                                setOp({ x: p.x, y: p.y });
                                show({ title: '坐标与显示器', data: p });
                              })
                            }
                          >
                            使用鼠标现在的位置
                          </button>
                        </>
                      )}
                      {o.type === 'mouse_button' && (
                        <div className="inline">
                          <select
                            aria-label="鼠标按钮"
                            value={o.button}
                            onChange={(e) => setOp({ button: e.target.value })}
                          >
                            {Object.entries({
                              left: '左键',
                              right: '右键',
                              middle: '中键（滚轮）',
                              x1: '侧键 1',
                              x2: '侧键 2',
                            }).map(([value, label]) => (
                              <option key={value} value={value}>
                                {label}
                              </option>
                            ))}
                          </select>
                          <select
                            aria-label="鼠标动作"
                            value={o.state}
                            onChange={(e) => setOp({ state: e.target.value })}
                          >
                            {Object.entries({
                              click: '单击',
                              double_click: '双击',
                              down: '按住',
                              up: '松开',
                            }).map(([value, label]) => (
                              <option key={value} value={value}>
                                {label}
                              </option>
                            ))}
                          </select>
                        </div>
                      )}
                      {o.type === 'wheel' && (
                        <>
                          <small>正数向上或向右滚动，负数向下或向左滚动。</small>
                          <select
                            aria-label="滚动方向"
                            value={o.axis}
                            onChange={(e) => setOp({ axis: e.target.value })}
                          >
                            <option value="vertical">上下滚动</option>
                            <option value="horizontal">左右滚动</option>
                          </select>
                          <NumberField
                            aria-label="滚动量"
                            range={ranges.wheelAmount}
                            value={o.amount}
                            onChange={(n) => setOp({ amount: n })}
                          />
                          <select
                            aria-label="滚动单位"
                            value={o.unit}
                            onChange={(e) => setOp({ unit: e.target.value })}
                          >
                            <option value="clicks">滚轮格数</option>
                            <option value="delta">细小单位（120 单位等于 1 格）</option>
                          </select>
                        </>
                      )}
                      {o.type === 'paste_files' && (
                        <>
                          <LinesInput
                            aria-label="文件路径列表"
                            value={o.paths}
                            onChange={(paths) => setOp({ paths })}
                          />
                          <button onClick={() => choosePath('files', (p) => setOp({ paths: p }))}>
                            选择多个文件
                          </button>
                          <small>
                            把文件本身粘贴到接收窗口，例如聊天附件；这里的文件位置文字不会作为正文输出。
                          </small>
                        </>
                      )}
                      {o.type === 'paste_image' && (
                        <>
                          <input
                            aria-label="图片路径"
                            value={o.path}
                            onChange={(e) => setOp({ path: e.target.value })}
                          />
                          <button onClick={() => choosePath('image', (p) => setOp({ path: p[0] }))}>
                            选择图片
                          </button>
                          <button
                            onClick={() =>
                              run(async () =>
                                setImagePreview({
                                  path: o.path,
                                  url: await call('preview_image', { path: o.path }),
                                }),
                              )
                            }
                          >
                            预览图片
                          </button>
                          {imagePreview?.path === o.path && (
                            <img
                              className="image-preview"
                              src={imagePreview?.url}
                              alt="待粘贴图片预览"
                            />
                          )}
                        </>
                      )}
                      <label className="field">
                        <span>这一步要在什么窗口执行</span>
                        <select
                          value={current.target.type}
                          onChange={(e) =>
                            updateSteps(
                              steps.map((s, i) =>
                                i === stepIndex
                                  ? {
                                      ...s,
                                      target:
                                        e.target.value === 'locked'
                                          ? { type: 'locked' }
                                          : { type: 'declared', process: '' },
                                    }
                                  : s,
                              ),
                            )
                          }
                        >
                          <option value="locked">始终在指令开始时的窗口执行</option>
                          <option value="declared">允许切换到指定程序</option>
                        </select>
                      </label>
                      {current.target.type === 'declared' && (
                        <input
                          aria-label="允许切换的程序名称（例如 notepad.exe）"
                          placeholder="允许切换的程序名称（例如 notepad.exe）"
                          value={current.target.process}
                          onChange={(e) =>
                            updateSteps(
                              steps.map((s, i) =>
                                i === stepIndex
                                  ? { ...s, target: { type: 'declared', process: e.target.value } }
                                  : s,
                              ),
                            )
                          }
                        />
                      )}
                      <div className="inline">
                        <button
                          disabled={stepIndex === 0}
                          onClick={() => {
                            updateSteps(moveItem(steps, stepIndex, stepIndex - 1));
                            setStepIndex(stepIndex - 1);
                          }}
                        >
                          上移
                        </button>
                        <button
                          disabled={stepIndex === steps.length - 1}
                          onClick={() => {
                            updateSteps(moveItem(steps, stepIndex, stepIndex + 1));
                            setStepIndex(stepIndex + 1);
                          }}
                        >
                          下移
                        </button>
                        <button onClick={() => updateSteps([...steps, clone(current)])}>
                          复制
                        </button>
                        <button
                          onClick={() => {
                            updateSteps(steps.filter((_, i) => i !== stepIndex));
                            setStepIndex(Math.max(0, stepIndex - 1));
                          }}
                        >
                          删除
                        </button>
                      </div>
                      <button
                        onClick={() =>
                          run(async () =>
                            show({
                              title: '单步执行结果',
                              data: describeExecution(
                                await execute(
                                  {
                                    type: 'steps',
                                    plan: {
                                      version: 1,
                                      dialect: 'structured',
                                      source: null,
                                      steps: [current],
                                    },
                                  },
                                  actionDiagnostics(a).filter((d) =>
                                    d.path.startsWith(`plan.steps[${stepIndex}].`),
                                  ),
                                ),
                              ),
                            }),
                          )
                        }
                      >
                        单步测试（真实执行）
                      </button>
                    </Fragment>
                  )}
                </div>
              </div>
            </>
          )}
          {a.type === 'replay' && (
            <div className="inline">
              <button
                onClick={() =>
                  run(async () => {
                    await call('start_recording');
                    setRecording(true);
                  }, '录制已开始，停止录制的快捷键不会被记录')
                }
              >
                开始录制
              </button>
              <button
                disabled={!recording}
                onClick={() =>
                  run(async () => {
                    const p = await call('stop_recording');
                    edit({ plan: p });
                    setRecording(false);
                  }, '录制已停止')
                }
              >
                停止录制
              </button>
              <small>点击“开始录制”后才会记录鼠标和按键；平时不会自动录制。</small>
            </div>
          )}
        </>
      )}
      <div className="action-test">
        <button
          onClick={() =>
            run(async () =>
              show({
                title: '动作测试结果',
                data: describeExecution(await execute(a, actionDiagnostics(a))),
              }),
            )
          }
        >
          测试动作（真实执行）
        </button>
        <button onClick={() => run(() => call('cancel_execution'), '已请求停止')}>停止动作</button>
        <small>
          测试会暂时隐藏本窗口，并在刚才使用的窗口中真正输入文字或执行操作。请先打开一个可以安全测试的输入框。显示发送完成后，仍需自行查看目标窗口是否成功。
        </small>
      </div>
    </>
  );
}
