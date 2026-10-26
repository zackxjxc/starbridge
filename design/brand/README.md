# StarBridge 品牌资源

采用 2026-10-26 确认的「01 星形桥梁」方案：两端桥墩与拱形桥面连接设备，中央四角星通过留白体现 StarBridge 名称。正式版本使用石墨灰 `#4e5a6a`，应用图标为石墨灰圆角方块与白色标志。

- `client/icons/logo.svg`：可编辑的单色矢量主文件，透明底，星形为真实镂空。
- `client/icons/app-icon.svg`：由主文件生成的方形应用图标源。
- `client/icons/icon.png`：1024×1024 RGBA 应用图标。
- `client/icons/icon.ico`：Windows 使用，包含 16、24、32、48、64、128、256 像素。
- `client/icons/icon.icns`：macOS 使用，包含 16–1024 像素及 Retina 表示。

客户端顶栏保留 20×20 像素品牌区，以 CSS 遮罩绘制桥梁，前景与背景跟随浅色、深色主题；现有装饰文字隐藏于零字号。管理台标题使用相同标志。两端 favicon 均使用内嵌 SVG，兼容客户端内嵌窗口与无界面 HTTP 控制台，不增加静态资源接口。

## 重新生成

仅重新绘制品牌资源时需要 Node.js 与 sharp；正常运行、Rust 编译与发布打包不需要安装这些工具。

```sh
npm install --prefix .cache/brand-assets --no-save --no-package-lock sharp
node tools/generate_brand_assets.cjs
node tools/generate_brand_assets.cjs --check
```

脚本从 `logo.svg` 读取同一份几何路径，生成应用图标、ICO、ICNS、顶栏与管理台的 CSS 遮罩及两端 favicon。它仅更新带 `starbridge-brand`、`starbridge-favicon` 标记的品牌块，保留其他页面代码；`--check` 只检查资源是否同步。

Windows 在下一次编译时嵌入 ICO；macOS 打包时复制 ICNS。现有编译产物不会因源图标更新而自动改变。
