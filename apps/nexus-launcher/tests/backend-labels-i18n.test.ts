import assert from "node:assert/strict";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { createUiTestLoader } from "./ui-test-loader.ts";

test("Canary round outcomes have Chinese labels and retain unknown outcomes", async () => {
  const loader = await createUiTestLoader();
  try {
    const { CanaryProgress } = await loader.loadModule("/src/App.tsx");
    const { I18nProvider } = await loader.loadModule("/src/i18n.ts");
    const html = renderToStaticMarkup(createElement(I18nProvider, { initialLocale: "zh" },
      createElement(CanaryProgress, { running: false, progress: {
        completed_rounds: ["passed", "failed", "inconclusive", "future-outcome"].map(outcome => ({outcome})),
      } })));
    for (const label of ["通过", "失败", "无法确定", "future-outcome"]) assert.ok(html.includes(label), label);
    assert.ok(!html.includes("未知状态：passed"));
    assert.ok(!html.includes("未知状态：inconclusive"));
  } finally { await loader.close(); }
});

test("Chinese maintenance localizes known categories, protection reasons and states without changing user data", async () => {
  const loader = await createUiTestLoader();
  try {
    const { SpaceMaintenancePanel } = await loader.loadModule("/src/App.tsx");
    const { I18nProvider } = await loader.loadModule("/src/i18n.ts");
    const areas = ["Nexus data", "Release slots", "Bundled runtimes", "Downloads (protected)", "Logs", "Diagnostics and recovery backups", "Checkpoints (protected)", "Agent program file", "Harness data (protected)"];
    const reasons = ["Current, rollback, or recovery version", "Referenced by the configured runtime or Harness launch path", "Current or recent failure logs are retained", "Within the log retention period", "Recent diagnostics and the last failure are retained", "Unrecognized diagnostic contents are preserved", "Within the retention period"];
    const html = renderToStaticMarkup(createElement(I18nProvider, { initialLocale: "zh" }, createElement(SpaceMaintenancePanel, {
      busyAction: null, snapshot: { startup: { available: true }, maintenance: {
        preview: { preview_id: "preview-a", areas: areas.map((kind, i) => ({ kind, path: `C:/Raw-${i}`, bytes: 1 })), items: reasons.map((reason, i) => ({ id: `item-${i}`, name: i === 0 ? "Ready" : `User slot ${i}`, reason, eligible: false })) },
        result: { state: "completed", items: [{ name: "User slot", state: "removed" }, { name: "Another slot", state: "deleting" }, { name: "RAW-NAME", state: "new_upstream_state", error: "RAW-OS-ERROR: access denied" }] },
      } },
    })));
    for (const label of [...areas, ...reasons]) assert.ok(!html.includes(label), label);
    for (const label of ["Nexus 数据", "版本槽", "已完成", "已删除", "正在删除", "Ready", "C:/Raw-0", "new_upstream_state", "RAW-OS-ERROR: access denied"]) assert.ok(html.includes(label), label);
  } finally { await loader.close(); }
});

test("Chinese startup, snapshot and launch input text preserves paths, profiles, raw errors and content", async () => {
  const loader = await createUiTestLoader();
  try {
    const { BasicStartupCheck, SnapshotDetail, LaunchInputsPanel } = await loader.loadModule("/src/App.tsx");
    const { I18nProvider } = await loader.loadModule("/src/i18n.ts");
    const zh = (component: unknown, props: Record<string, unknown>) => renderToStaticMarkup(createElement(I18nProvider, { initialLocale: "zh" }, createElement(component as any, props)));
    const startup = zh(BasicStartupCheck, { disabled: false, initialResult: { ready: false, checks: [
      { id: "home", status: "ok", reason: "C:/Ready · read/write access verified" },
      { id: "profile", status: "ok", reason: "Ready: initialized" },
      { id: "node", status: "ok", reason: "v24.20.0 · bundled" },
      { id: "port", status: "ok", reason: "127.0.0.1:3080 is currently available" },
      { id: "entry", status: "blocked", reason: "RAW-ERROR Entry file is missing: C:/Raw", next: "Reinstall the selected version or correct the launch path." },
    ] } });
    for (const value of ["C:/Ready", "Ready: 已初始化", "已验证读写权限", "127.0.0.1:3080 当前可用", "v24.20.0", "RAW-ERROR Entry file is missing: C:/Raw", "请重新安装所选版本或修正启动路径。"]) assert.ok(startup.includes(value), value);
    assert.ok(!startup.includes(" · bundled"));
    const snapshot = zh(SnapshotDetail, { value: { summary: { kind: "healthy" }, files: [
      { path: "profiles/Ready/settings.yaml", state: "omitted", stored_size: 0, omitted_reason: "invalid YAML; file omitted to avoid unsafe byte copying" },
      { path: "raw.json", state: "present", stored_size: 3, content: "RAW-UPSTREAM-CONTENT", content_truncated: true, content_note: "content is truncated by the 123 byte per-file and 456 byte response limits" },
      { path: "unknown.json", state: "new_state", omitted_reason: "RAW-UNKNOWN-REASON", content_truncated: true, content_note: "RAW-UNKNOWN-NOTE" },
    ] } });
    for (const value of ["未收录", "已收录", "profiles/Ready/settings.yaml", "YAML 无效", "每个文件上限 123 字节", "每次响应上限 456 字节", "new_state", "RAW-UNKNOWN-REASON", "RAW-UNKNOWN-NOTE"]) assert.ok(snapshot.includes(value), value);
    assert.ok(!snapshot.includes("RAW-UPSTREAM-CONTENT"), "Raw content is deferred to the Source view");
    assert.ok(snapshot.includes("源码"));
    const inputs = zh(LaunchInputsPanel, { snapshot: { config: { launch_inputs: { next_launch: { fields: [
      { name: "Profile", value: "Ready", source: "Selected or compatibility profile" },
      { name: "Program", value: "System", source: "Resolved launch configuration" },
      { name: "Permission mode", value: "read-only", source: "Nexus override" },
      { name: "Open browser", value: "false", source: "Nexus override" },
    ] } } } } });
    for (const value of [">Ready<", ">System<", "只读（read-only）", "已关闭"]) assert.ok(inputs.includes(value), value);
  } finally { await loader.close(); }
});


test("durable request kinds are localized without hiding unknown future kinds", async () => {
  const loader = await createUiTestLoader();
  try {
    const { I18nProvider, useI18n } = await loader.loadModule("/src/i18n.ts");
    const kinds = ["cold_switch", "harness_restart", "rollback", "offline_import", "snapshot_restore", "profile_delete", "profile_restore", "future-operation"];
    function Labels() { const { t } = useI18n(); return createElement("span", null, kinds.map(kind => t(kind)).join(" | ")); }
    const html = renderToStaticMarkup(createElement(I18nProvider, {initialLocale: "zh"}, createElement(Labels)));
    for (const kind of kinds.slice(0, -1)) assert.ok(!html.includes(kind), kind);
    for (const label of ["安装或切换版本", "重启 Harness", "回退版本", "导入离线运行包", "恢复快照", "删除配置档", "还原配置档", "future-operation"]) assert.ok(html.includes(label), label);
  } finally { await loader.close(); }
});
