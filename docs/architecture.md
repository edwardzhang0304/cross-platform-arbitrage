# Windows 交付边界

- 原 `openai_inventory` 核心按文件复制（测试配置改用无身份信息的合成样例），交易信号、风控、双腿执行、资金费和 SQLite 状态格式保持一致。源文件哈希见 `source-manifest.json`。
- 保留核心所需的行情适配器、Hyperliquid/Lighter 通讯、签名、nonce 互斥、密钥库加密和风险回归测试。其他策略、研究脚本、模拟运行入口与原综合控制台不进入此包。
- `portable.rs` 管理 EXE 旁的独立数据目录、单实例锁与只读一致性迁移导出；配置、密钥、账本各有目录。
- `server.rs` 是新的本机控制入口，调用原策略服务的 typed Control；每个写操作验证本机 Host、Origin、随机会话 token 和操作 ID。静态监控页沿用现有代码。
- 所有交易仍经过原策略/操作意图、执行风险检查、各平台账户工作器；网页不直接调用交易所。
- `power.rs` 使用专用线程调用 `SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED)`。不用 `ES_DISPLAY_REQUIRED`，所以可以息屏；退出释放。无法拦截手动睡眠、合盖和断电。
- 程序启动时只启动本机网页；首次解锁或检查账户不提交交易。加载会按现有恢复规则处理账本，启动新增交易需要操作者的明确操作。
- GitHub 与 ZIP 不包含实际配置、Vault、会话缓存、成交数据或运行日志。

参考：[Microsoft 电源 API](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-setthreadexecutionstate)、[Rust 静态 CRT](https://doc.rust-lang.org/reference/linkage.html#static-and-dynamic-c-runtimes)。

密钥管理保留原加密格式；交付版增加原子写入、解密明文临时缓冲区清零和 Debug 脱敏。源码清单分别记录原始与交付文件哈希；精简的模块清单见 Cargo.toml 与 src/lib.rs。

Windows 回归测试清理 SQLite 临时文件时，需等待异步模拟工作器释放文件句柄；仅调整该测试的清理等待，交易核心不变。
