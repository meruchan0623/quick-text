# Windows 自动输入验收模式

正式版默认过滤外部模拟键盘输入，忽略 `QUICKER_TEXT_DATA_DIR`，保持正常用户数据目录与输入行为。要用最新绿色版进行自动化验收，须完全退出已有实例，再以两个启动条件同时启用：

```powershell
$env:QUICKER_TEXT_TEST_INPUT = '1'
$env:QUICKER_TEXT_DATA_DIR = 'C:\Develop\quicker-text\work\acceptance-data'
& 'C:\path\to\quicker-text.exe' --acceptance-test-input
```

仅环境变量或仅命令行参数不会在 release 启用模式。参数必须为完整的 `--acceptance-test-input`，环境值必须恰好为 `1`。该模式仅支持 Windows；debug 版本仍支持仅通过 `QUICKER_TEXT_TEST_INPUT=1` 开启，且保留原有自定义数据目录支持。启动后模式固定，已有实例不会因第二次启动参数改变模式。

在显式模式下，非空 `QUICKER_TEXT_DATA_DIR` 指向隔离数据目录；未设置时仍使用正常用户数据目录，因此自动化验收应先设置独立目录。诊断输入状态中 `external_injected_keys` 显示 `explicit_test_mode`；正常启动显示 `filtered`。可通过运行状态详情确认。

验收使用同一个 release exe 的真实全局键盘 hook，允许外部 `SendInput` 作为触发输入。应逐条触发导入规则，确认实际目标程序消费输出，并读取目标内容核对结果，保留失败及修复后复测证据。该模式不绕过引擎执行链，也不能用内部函数返回值代替目标消费读回。

这种自动化结果不能代表实物键盘验收；硬件按键路径、输入法与目标程序兼容性仍需实际键盘验证。验收结束退出进程，清除上述环境变量后正常启动即可恢复默认行为。
