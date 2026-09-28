# Inventory 页面组件与布局核查

日期：2026-09-28。以下为页面组件改造阶段的独立核查记录，改动随后纳入 rc.7；版本交付及升级见 `rc7升级说明.md`。Windows 实际安装由用户完成。

## 原因

OPENAI 和 ANTH 已经引用同一个 `openai-live-monitor.html`，但各表格使用浏览器的自动列宽。数字长度、数量精度及是否有记录都会改变字段的横向位置。行情卡片另有按价差方向倒序的逻辑，会交换 Lighter 和 Entropy 的位置。

## 修改

- `frontend/inventory-components.js` 统一生成收益概览、账户、持仓和成交表。每张表的字段名称、列宽、对齐和格式化来自一份列定义。
- 使用固定表格布局和列宽；空表、有记录及长数字时保持相同的列位置。数值右对齐，溢出的内容可通过提示查看完整值。
- 窄屏在表格内部横向滚动，页面本身不被表格撑宽。列表高度仍随实际记录条数变化。
- 行情卡片和账户行固定先 Lighter、后 Entropy，图表继续显示实际交易方向。
- 两标的、实盘与模拟盘共用组件。OPENAI 显示 4 位数量小数，ANTH 显示 5 位；标的、模式及接口匹配检查继续保留。
- 组件只读取现有接口的数据，不提交交易或修改账本。资金费按 lot ID 对应；未知值显示 `—`。

## 验证

- `node --test tests/*.test.cjs`：19 项通过，包含精度、收益归属、未知收益、文本转义、报价平台顺序及原有前端检查。
- 临时 Playwright 浏览器使用合成数据验证 36 组组合：1440、900、390 像素宽度 × 实盘/模拟盘 × OPENAI/ANTH × 正常/空表/长数字。对应字段的列位置、宽度、对齐和收益概览宽度一致；页面无整体横向溢出、无浏览器脚本错误。
- Mac 本机两个入口通过 `cargo check --locked --offline --bins --no-default-features --features openai-inventory-live` 和 `--features paper-runtime`。仅保留原有 `lighter_manual.rs` 未使用代码警告。
- `node scripts/check-shared-core.cjs` 通过；策略核心哈希仍为 `493dbd6bbd5b04024787fc6aa4d8e430cc4620dc3c8cd976d8dc354419bed6ec`。
- 已将新组件资源加入 Windows EXE 冒烟检查脚本，随 rc.7 构建一起验收。

测试没有启动交易后台或连接真实账户。浏览器临时会话已结束，合成数据预览已关闭，临时 Rust 编译目录已清理。预览截图使用合成数据，保存在忽略 Git 的 `output/playwright/inventory-layout/`。
