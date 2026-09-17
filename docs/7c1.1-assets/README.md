# Phase 7C1.1 回归测试资产（冻结自 Round 2/3 验收）

本目录存放严格验收官在 Round 2 / Round 3 使用的负向探针原件。7C1 (v1.2.1) 是
offline-only 交付，这些探针当时不作为运行测试。

**Phase 7C1.1 已将它们恢复为仓库内永久回归**（AC-10）：

- `round2-https-loopback-probe.rs`：生产 HTTPS 拒绝 loopback 字面与解析结果。
- `round3-inconsistent-origin-probe.rs`：URL 字面拒绝先于 DNS；公开 dev flag
  无法为 HTTPS loopback 解锁；零 DNS 泄漏。

对应运行测试位于 `crates/lato-agent/src/agentfield/probes_7c1_1.rs`（`#[cfg(test)]`
内，经 7C1.1 冻结的测试 seam），原始探针语义逐条保留；`McpDnsResolver` 探针
依赖按 7C1.1 规格替换为 AgentField 内部 resolver seam。全部 4 条探针持续通过。
