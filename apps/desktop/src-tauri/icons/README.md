# 拾忆应用图标

`icon.svg` 是拾忆唯一可编辑的应用图标母版，应用标题栏、HTML 入口、Windows 主程序、安装器和卸载器均以它为来源。

其余 PNG、ICO 文件是供 Tauri 和 Windows 使用的生成产物，不应单独修改颜色、形状或比例。更新主题图标时，先修改 `icon.svg`，再在仓库根目录执行：

```powershell
& 'apps\desktop\node_modules\.bin\tauri.CMD' icon 'apps\desktop\src-tauri\icons\icon.svg' --output '.artifacts\icon-generated'
```

将生成结果中的 `32x32.png`、`128x128.png`、`128x128@2x.png`、`icon.png` 和 `icon.ico` 分别同步为当前目录下的 `32x32.png`、`128x128.png`、`256x256.png`、`512x512.png` 和 `icon.ico`。同步后必须执行前端测试、构建和 NSIS 安装包验证。
