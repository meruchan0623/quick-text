// Chinese display names for contract codes. Unknown values (newer backend) are shown as
// 「其他（原文）」 via `label()`, never as an exception (docs/ipc-contract-0.4.md §1).

type Names = Readonly<Record<string, string>>;

/** The name of `code` in `names`; own keys only, so `constructor` and the like stay unknown. */
const lookup = (names: Names, code: unknown): string | undefined =>
  typeof code === 'string' && Object.hasOwn(names, code) ? names[code] : undefined;

/** 「其他」 with the raw value, for an enum value newer than this frontend. */
export function other(raw: unknown): string {
  const text =
    typeof raw === 'string'
      ? raw
      : typeof raw === 'number' || typeof raw === 'boolean'
        ? `${raw}`
        : '';
  return text ? `其他（${text}）` : '其他';
}

export const label = (names: Names, code: unknown): string => lookup(names, code) ?? other(code);

/**
 * Diagnostics of tolerant reading (B-43). The backend's `message` is English developer text, so
 * these codes get Chinese text here; other codes keep `message`.
 */
export const diagnosticTexts: Readonly<Record<string, string>> = {
  rule_unreadable: '这条规则的内容无法读取，已原样保留，暂不生效。',
  field_reset: '这项设置的值无法识别，已按默认值使用；保存后会写入默认值。',
  terminator_unsupported: '触发键 Tab、回车在当前平台上暂不支持，按下它们不会执行指令。',
  schema_newer: '配置来自更新版本的 Quicker Text，当前版本只能查看，不会修改它。',
  config_malformed: '配置文件格式有误，无法读取。',
};

/** Health page tags, one per HealthState (plan §7.2). */
export const healthNames: Readonly<Record<string, string>> = {
  starting: '正在启动',
  healthy: '正常',
  degraded: '降级',
  recovering: '恢复中',
  failed: '故障',
};

export const safeModeNames: Readonly<Record<string, string>> = {
  crash_loop: '连续多次启动失败',
  locked: '配置文件被其他程序占用',
  newer_schema: '配置文件来自更新的版本',
  no_valid_config: '没有找到可用的配置',
};

/** Capability ids of `get_capabilities` (crates/platform/src/capabilities.rs). */
export const capabilityNames: Readonly<Record<string, string>> = {
  keyboard_hook: '键盘监听',
  keys: '逐字键入',
  paste: '粘贴文本',
  html: '粘贴 HTML',
  file_clipboard: '粘贴文件对象',
  image_clipboard: '粘贴图片对象',
  mouse: '鼠标操作',
  recording: '键鼠录制',
  foreground: '目标窗口识别',
  open: '打开文件与应用',
  ime_committed_text: '缩写与输入法候选识别',
  quicker_private_action: 'Quicker 专有动作',
  // macOS only.
  wheel_delta: '精细滚轮滚动',
  media_system_actions: '媒体与系统操作',
  window_ime_actions: '窗口与输入法操作',
  // Ids of older backends.
  selection: '读取选中文本',
  notification: '触发后提示',
};

/** Capability status tags (contract §5 `get_capabilities`). */
export const capabilityStatusNames: Readonly<Record<string, string>> = {
  available: '可用',
  unavailable: '不可用',
  experimental: '实验性',
  degraded: '降级',
};

/**
 * Capability reasons about the platform rather than a component (Rust `PlatformReason`); a
 * capability's `reason` is one of these or a ReasonCode.
 */
export const platformReasonNames: Readonly<Record<string, string>> = {
  experimental_platform: '本平台的实现尚未经过实机验收，可能不稳定',
  permission_missing: '缺少系统权限（辅助功能、输入监控或模拟输入），授权后重启本程序',
  not_implemented: '本平台不提供此功能',
};

export const ioOpNames: Readonly<Record<string, string>> = {
  read: '读取文件',
  write: '写入文件',
  rename: '重命名文件',
  create_dir: '创建文件夹',
  remove: '删除文件',
  open: '打开',
  dialog: '打开文件对话框',
  other: '文件操作',
};

export const entityNames: Readonly<Record<string, string>> = {
  rule: '规则',
  group: '分组',
  revision: '历史版本',
  file: '文件',
  history: '执行记录',
  window: '窗口',
};

/** Supervised components (B-03); shown as 「受影响项」 in the Degraded row. */
export const componentNames: Readonly<Record<string, string>> = {
  executor: '执行器',
  keyboard_hook: '键盘监听',
  mouse_hook: '鼠标监听',
  raw_input: '原始键盘输入',
  supervisor: '监督器',
  focus_events: '前台窗口跟踪',
  clipboard: '剪贴板',
  protection: '目标保护检查',
};

export const pauseNames: Readonly<Record<string, string>> = {
  manual: '手动暂停',
  timed: '定时暂停',
  fullscreen: '全屏程序运行中',
  recording: '正在录制键鼠',
  safe_mode: '安全模式',
  engine_down: '输入组件失效',
  self_test: '正在自检',
};

export const outcomeNames: Readonly<Record<string, string>> = {
  succeeded: '已完成',
  unknown: '已发送 · 结果未知',
  failed: '失败',
  skipped: '已跳过',
  cancelled: '已停止',
  dropped_busy: '未执行',
};

export const reasonNames: Readonly<Record<string, string>> = {
  ok: '正常',
  dropped_busy: '上一条仍在执行',
  cancelled: '已被停止',
  paused: '监听已暂停',
  engine_down: '输入引擎不可用',
  focus_changed: '执行中切换了窗口',
  stale_input: '输入已过期',
  protected_control: '目标是密码框或只读控件',
  modifier_held: '按住了 Ctrl、Alt 等修饰键',
  inject_partial: '只输出了一部分',
  clipboard_busy: '剪贴板被其他程序占用',
  clipboard_changed: '执行中剪贴板被修改',
  clipboard_format_skipped: '部分剪贴板格式未能恢复',
  clipboard_too_large: '剪贴板内容过大，未备份',
  resolve_failed: '内容生成失败',
  preflight_failed: '执行前检查未通过',
  executor_panic: '执行器内部错误',
  executor_timeout: '执行超时',
  hook_missing: '键盘监听失效',
  raw_input_missing: '原始键盘输入失效',
  hook_install_failed: '键盘监听安装失败',
  unsupported: '当前平台不支持',
  internal: '内部错误',
};

export const triggerNames: Readonly<Record<string, string>> = {
  immediate: '即时',
  terminator: '触发键',
  manual: '手动测试',
};

export const selfTestNames: Readonly<Record<string, string>> = {
  raw_input: '原始键盘输入',
  hook_heartbeat: '键盘监听心跳',
  foreground: '前台窗口识别',
  clipboard: '剪贴板',
};

/** `set_log_level` levels, as in the `log_level_elevated` banner. */
export const logLevelNames: Readonly<Record<string, string>> = {
  error: '仅错误',
  warn: '警告',
  info: '常规',
  debug: '调试',
  trace: '最详细',
};

/** Codes look like this; anything else in a reason field is prose from a 0.3 backend. */
const CODE = /^[a-z][a-z0-9_]*$/;

/**
 * Chinese text for a capability's `reason`: a ReasonCode or a PlatformReason, `null` when there is
 * nothing to explain. A 0.3 backend sent Chinese prose here, which is kept as is.
 */
export function capabilityReasonText(reason: unknown): string | null {
  if (typeof reason !== 'string' || reason === '' || reason === 'ok') return null;
  const known = lookup(reasonNames, reason) ?? lookup(platformReasonNames, reason);
  if (known !== undefined) return known;
  return CODE.test(reason) ? other(reason) : reason;
}
