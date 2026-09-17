# Phase 7C1.1 回归测试资产（冻结自 Round 2/3 验收）

本目录存放严格验收官在 Round 2 / Round 3 使用的负向探针原件。7C1 (v1.2.1) 是
offline-only 交付：生产 transport 与网络边界已整体移出本刀（规格 §0.3），这些
探针引用的 `ReqwestTransport` / `connect_with_resolver` / `with_pinned_addrs`
API 在 7C1 中不存在，因此**不作为 7C1 的运行测试**。

Phase 7C1.1（7C2 前置）必须将它们恢复为运行测试并全部通过：

- `round2-https-loopback-probe.rs`：生产 HTTPS 拒绝 loopback 字面与解析结果。
- `round3-inconsistent-origin-probe.rs`：URL 字面拒绝先于 DNS；公开 dev flag
  无法为 HTTPS loopback 解锁；零 DNS 泄漏。

恢复时的配套 API 由 7C1.1 规格定义（生产 transport 重新进场并附带完整地址
策略与不可绕过的公开面）。
