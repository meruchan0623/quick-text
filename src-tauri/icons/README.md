# Quicktext 图标

深蓝底上的白色文本行与青绿色向右箭头表示“输入缩写，展开文字或动作”。图形不依赖字母和细小文字，便于在托盘、任务栏和桌面识别。

- `icon.svg`：可编辑的标准矢量源图；`icon-small.svg`：16–24 像素光学校正版。
- `icon.ico`：16、20、24、32、40、48、64、128、256 像素独立图层，供 Windows 选择。
- `icon.png`：512 像素；其余 PNG 提供 32、128、256 像素版本。

在仓库根目录使用 PowerShell 7 重新生成，无需安装图形依赖：

```powershell
pwsh -NoProfile -File scripts/generate-icons.ps1
```

生成器使用 Windows System.Drawing，读取上述 SVG 中的图形，逐尺寸绘制并校验 ICO 图层。浅色与深色背景下的原尺寸预览默认写入 `work/icon-refresh/icon-preview.png`。

Windows 托盘按系统小图标尺寸从 EXE 图标资源选取图层；窗口、任务栏、文件管理器与安装包使用同一 ICO。前端页面图标使用标准 SVG。
