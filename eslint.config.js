// ESLint flat config for the frontend (FE-48).
import js from '@eslint/js';
import reactHooks from 'eslint-plugin-react-hooks';
import globals from 'globals';
import tseslint from 'typescript-eslint';

// Native blocking dialogs freeze the WebView and cannot be styled or tested;
// polling timers are replaced by event subscriptions (S-19). Timers that are
// genuinely needed live in src/lib/timers.ts, which is exempt below.
const bannedDialogs = ['prompt', 'confirm', 'alert'];
const bannedTimers = ['setInterval'];
const restrictedGlobals = [...bannedDialogs, ...bannedTimers].map((name) => ({
  name,
  message: bannedDialogs.includes(name)
    ? `Do not use ${name}(): show an in-app confirmation instead.`
    : `Do not poll with ${name}(): subscribe to events, or use src/lib/timers.ts.`,
}));
const restrictedProperties = ['window', 'globalThis', 'self'].flatMap((object) =>
  restrictedGlobals.map(({ name, message }) => ({ object, property: name, message })),
);

export default tseslint.config(
  {
    // Only the frontend is linted. `.claude/` holds agent worktrees (full checkouts with their
    // own dist/), and work/, private-imports/ are local scratch directories (.gitignore).
    ignores: [
      '**/dist/',
      'node_modules/',
      'src-tauri/',
      'target/',
      'crates/',
      'vendor/',
      'scripts/',
      'docs/',
      '.claude/',
      'work/',
      'private-imports/',
    ],
  },
  {
    files: ['src/**/*.{ts,tsx}'],
    extends: [js.configs.recommended, ...tseslint.configs.strictTypeChecked],
    languageOptions: {
      ecmaVersion: 2022,
      globals: globals.browser,
      parserOptions: { projectService: true, tsconfigRootDir: import.meta.dirname },
    },
    plugins: { 'react-hooks': reactHooks },
    rules: {
      ...reactHooks.configs.recommended.rules,
      'max-len': [
        'error',
        { code: 160, ignoreUrls: true, ignoreStrings: true, ignoreTemplateLiterals: true },
      ],
      'no-restricted-globals': ['error', ...restrictedGlobals],
      'no-restricted-properties': ['error', ...restrictedProperties],
      '@typescript-eslint/restrict-template-expressions': ['error', { allowNumber: true }],
      // `onClick={() => setOpen(false)}` is idiomatic React.
      '@typescript-eslint/no-confusing-void-expression': ['error', { ignoreArrowShorthand: true }],
      // Event-handler props may be async; their promise is intentionally not awaited by React.
      '@typescript-eslint/no-misused-promises': [
        'error',
        { checksVoidReturn: { attributes: false } },
      ],
    },
  },
  {
    // Legacy 0.3 screens are built on `Data = Record<string, any>` (plan §2, src/model.ts).
    // Typing them is FE-08 (generated bindings, 0.5) and they are replaced page by page then;
    // until that, only the any-propagation rules are relaxed here. New code (src/ipc, src/lib)
    // gets the full strictTypeChecked set.
    files: [
      'src/App.tsx',
      'src/ActionEditor.tsx',
      'src/CaptureTemplate.tsx',
      'src/model.ts',
      'src/RuntimeStatus.tsx',
    ],
    rules: {
      '@typescript-eslint/no-explicit-any': 'off',
      '@typescript-eslint/no-unsafe-argument': 'off',
      '@typescript-eslint/no-unsafe-assignment': 'off',
      '@typescript-eslint/no-unsafe-call': 'off',
      '@typescript-eslint/no-unsafe-member-access': 'off',
      '@typescript-eslint/no-unsafe-return': 'off',
    },
  },
  {
    files: ['src/lib/timers.ts'],
    rules: { 'no-restricted-globals': 'off', 'no-restricted-properties': 'off' },
  },
  {
    files: ['*.{js,ts}'],
    extends: [js.configs.recommended, ...tseslint.configs.strict],
    languageOptions: { globals: globals.node },
  },
);
