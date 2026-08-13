# 翻翻应用图标

`fanfan-source.png` 是当前唯一图形标志母版。它直接裁剪自用户提供的品牌图，只对与画布连通的近白背景设置透明度，没有重新生成、重绘或改变图形结构。应用标题栏使用同源的 `src/assets/fanfan-logo.png`，Windows 主程序、安装器和卸载器均由此母版缩放生成。

其余 PNG、ICO 文件是供 Tauri 和 Windows 使用的生成产物，不应单独修改颜色、形状或比例。更新品牌图时，应重新从用户确认的源图确定性裁剪；不得用文生图或二次生成图替代。然后在仓库根目录执行：

```powershell
& 'apps\desktop\node_modules\.bin\tauri.CMD' icon 'apps\desktop\src-tauri\icons\fanfan-source.png' --output '.artifacts\icon-generated'
```

将生成结果中的 `32x32.png`、`128x128.png`、`128x128@2x.png`、`icon.png` 和 `icon.ico` 分别同步为当前目录下的 `32x32.png`、`128x128.png`、`256x256.png`、`512x512.png` 和 `icon.ico`。同步后必须执行前端测试、构建和 NSIS 安装包验证。
