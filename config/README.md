# 配置和密钥分别放哪里

这里是空白模板。首次运行时，在程序页面填写，程序自动生成真正使用的文件；不用把私钥写进 JSON。

| 内容 | 实际保存位置 | 怎么填写 |
|---|---|---|
| Entropy 主账户公开地址、账户名称 | `data/config/accounts.json` | 主钱包公开地址，不是 API 代理钱包地址 |
| Lighter 账户 INDEX、策略参数 | `data/config/strategy.json` | INDEX 是平台显示的数字；迁移时以旧账本为准 |
| Entropy 交易 API 私钥 | `data/keys/trading.vault` 内 | 本人在页面填写，与 Hyperliquid 已授权的 API 钱包对应 |
| Lighter 交易 API 私钥、API KEY INDEX | 同一个加密 Vault 内，独立条目 | Lighter 生成的 40 字节 API 私钥，及其数字索引 |
| 密钥库密码 | 只在已解锁程序的内存中 | 不写入配置、脚本或 GitHub |
| 持仓、成交和资金费账本 | `data/runtime/openai-inventory/live.sqlite` | 程序管理，不手动修改 |

`accounts.example.json` 和 `strategy.example.json` 不含真实账户数据。交易 API 私钥不等于 MetaMask 主私钥；这里不要填写助记词或主钱包私钥。

已有 Mac 实盘的用户，应先迁移原配置、完整账本和加密 Vault，再用原密码解锁；不要把旧持仓当作新账户重新配置。升级只替换程序，保留整个 `data` 目录。


## 两个标的的配置隔离

- OPENAI 继续使用原来的 `data/config`、`data/keys/trading.vault`、`data/runtime/openai-inventory`，旧账本不用改名。
- ANTH 实盘配置由 `/anth` 页面保存到 `data/profiles/anth-live/config/profile.json`；密钥库和账本分别在同目录的 `keys`、`runtime`。
- `strategy.anth.example.json` 只作公开模板，空地址不能启动。请在 ANTH 页面填写新账户，不复制 OPENAI 密钥库。
- 首次升级后的 OPENAI 解锁会保存公开账户绑定（`account-bindings.json`）；不改变旧账本。在录入 ANTH 前先解锁一次原 OPENAI，以便检查新钱包没有串用。
- Mac 模拟另用全新的数据根目录。目录标记、配置、远端虚拟成交记录和主账本都会核对标的和模式；模拟配置拒绝真实地址或账户编号。
- 不要手动改已有账本的标的、账户或小数位。不上传 `data`、账户配置、绑定文件、密钥库和运行日志到 GitHub。
