import { useRef, useState } from 'react';
import { Modal } from './Modal';

/** An in-app replacement for `window.confirm` / `window.prompt`. */
export interface AskOptions {
  /** Phrased as a question, e.g. 「删除这条指令？」. */
  title: string;
  detail?: string | undefined;
  /** Verb on the confirming button, e.g. 「删除」. */
  confirm: string;
  /** Asks for a value (text, or one of `options`) instead of a yes/no answer. */
  input?:
    | {
        label: string;
        value: string;
        options?: readonly { value: string; label: string }[] | undefined;
      }
    | undefined;
}

function AskDialog({
  request,
  onDone,
}: {
  request: AskOptions;
  onDone: (answer: string | null) => void;
}) {
  const [value, setValue] = useState(request.input?.value ?? '');
  const input = request.input;
  return (
    <Modal title={request.title} className="overlay top" onClose={() => onDone(null)}>
      <form
        className="report ask"
        onSubmit={(e) => {
          e.preventDefault();
          onDone(input ? value : '');
        }}
      >
        <div className="modal-title">
          <h2>{request.title}</h2>
        </div>
        {(request.detail || input) && (
          <div className="ask-body">
            {request.detail && <p>{request.detail}</p>}
            {input && (
              <label className="field">
                <span>{input.label}</span>
                {input.options ? (
                  <select data-autofocus value={value} onChange={(e) => setValue(e.target.value)}>
                    {input.options.map((o) => (
                      <option key={o.value} value={o.value}>
                        {o.label}
                      </option>
                    ))}
                  </select>
                ) : (
                  <input data-autofocus value={value} onChange={(e) => setValue(e.target.value)} />
                )}
              </label>
            )}
          </div>
        )}
        <div className="modal-footer">
          {/* A yes/no question starts on the safe answer; Esc also cancels. */}
          <button
            type="button"
            data-autofocus={input ? undefined : true}
            onClick={() => onDone(null)}
          >
            取消
          </button>
          <button type="submit" className="primary">
            {request.confirm}
          </button>
        </div>
      </form>
    </Modal>
  );
}

/**
 * Returns the dialog element to render plus two promise-based helpers:
 * `confirmAction` resolves to true/false, `askValue` to the entered value or null on cancel.
 */
export function useAsk() {
  const [request, setRequest] = useState<{ id: number; options: AskOptions } | null>(null);
  const resolver = useRef<((answer: string | null) => void) | null>(null);
  const ask = (options: AskOptions) =>
    new Promise<string | null>((resolve) => {
      resolver.current?.(null);
      resolver.current = resolve;
      setRequest((current) => ({ id: (current?.id ?? 0) + 1, options }));
    });
  const finish = (answer: string | null) => {
    const resolve = resolver.current;
    resolver.current = null;
    setRequest(null);
    resolve?.(answer);
  };
  return {
    askDialog: request && <AskDialog key={request.id} request={request.options} onDone={finish} />,
    confirmAction: async (options: Omit<AskOptions, 'input'>) => (await ask(options)) !== null,
    askValue: (options: AskOptions & { input: NonNullable<AskOptions['input']> }) => ask(options),
  };
}
