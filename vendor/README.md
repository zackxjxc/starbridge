# 外部运行时目录

这里保存经过版本锁定和哈希校验后、供发布构建使用的 FRP/OpenSSH 运行时文件。

当前阶段不把可执行文件直接提交到 Git。发布脚本根据 [../docs/dependencies-and-packaging.md](../docs/dependencies-and-packaging.md) 中约定的清单下载、校验并复制它们。构建缓存可以放在工作区外；运行中的客户端不从互联网自动下载依赖。
