// Chinese display names for contract codes. Unknown values (newer backend) fall back to the raw
// code via `label()`, never to an exception (docs/ipc-contract-0.4.md §1).

export const label = (names: Readonly<Record<string, string>>, code: string): string =>
  names[code] ?? code;

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
  selection: '读取选中文本',
  notification: '触发后提示',
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
