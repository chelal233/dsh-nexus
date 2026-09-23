import test from "node:test";
import assert from "node:assert/strict";
import { JSDOM } from "jsdom";
import { mockIPC, clearMocks } from "./desktop-mocks.ts";
import { createUiTestLoader } from "./ui-test-loader.ts";

test("settings controls send every Harness preference, runtime pin and notification category", async () => {
  const dom = new JSDOM('<div id="root"></div>', { url: "http://localhost/" });
  const bindings = {
    window: dom.window,
    document: dom.window.document,
    navigator: dom.window.navigator,
    HTMLElement: dom.window.HTMLElement,
    Node: dom.window.Node,
    Event: dom.window.Event,
    CustomEvent: dom.window.CustomEvent,
    IS_REACT_ACT_ENVIRONMENT: true,
  };
  const old = new Map(
    Object.keys(bindings).map((k) => [k, Object.getOwnPropertyDescriptor(globalThis, k)]),
  );
  for (const [k, v] of Object.entries(bindings))
    Object.defineProperty(globalThis, k, { value: v, writable: true, configurable: true });
  const React = await import("react");
  const { createRoot } = await import("react-dom/client");
  const root = createRoot(document.getElementById("root")!);
  const calls: any[] = [];
  let failAutostart = false;
  mockIPC((command, args) => {
    calls.push({ command, args });
    if (command === "autostart_status") return false;
    if (command === "autostart_set" && failAutostart) throw Error("fixture permission denied");
    if (command === "update_status") return { enabled: true, phase: "idle" };
    if (command === "proxy_request" && args.path === "/v1/notifications")
      return { settings: {}, capabilities: [] };
    return {};
  });
  const loader = await createUiTestLoader();
  const actions: any[] = [];
  const themes: string[] = [];
  try {
    const { SettingsView } = await loader.loadModule("/src/App.tsx");
    const { translateForTest, I18nProvider } = await loader.loadModule("/src/i18n.ts");
    const snapshot = {
      startup: { available: true },
      config: { revision: "r1" },
      harnessRuntime: { harness: { state: "stopped" } },
      updates: { update: { state: "idle" } },
      health: { harness_config_wire_version: 2 },
    };
    await React.act(async () =>
      root.render(
        React.createElement(
          I18nProvider,
          { initialLocale: "en" },
          React.createElement(SettingsView, {
            snapshot,
            busyAction: null,
            themeMode: "system",
            setThemeMode: (v: string) => themes.push(v),
            runAction: async (...args: any[]) => {
              actions.push(args);
              return true;
            },
          }),
        ),
      ),
    );
    const change = async (label: string, value: string) => {
      label = translateForTest("en", label);
      const wrapper = [...document.querySelectorAll("label")].find(
        (e) =>
          e.querySelector(".field-label")?.textContent?.trim().startsWith(label) ||
          e.firstChild?.textContent?.trim() === label,
      );
      assert.ok(wrapper, `label ${label}`);
      const input = wrapper.querySelector("input,select,textarea") as HTMLInputElement;
      assert.ok(input, `input ${label}`);
      assert.equal(input.disabled, false, label);
      await React.act(async () => {
        Object.getOwnPropertyDescriptor(Object.getPrototypeOf(input), "value")!.set!.call(
          input,
          value,
        );
        input.dispatchEvent(new dom.window.Event("input", { bubbles: true }));
        input.dispatchEvent(new dom.window.Event("change", { bubbles: true }));
      });
    };
    const click = async (label: string) => {
      const button = [...document.querySelectorAll("button")].find(
        (b) => b.textContent?.trim() === label,
      );
      assert.ok(button, `button ${label}`);
      assert.equal(button.disabled, false, label);
      await React.act(async () => button.click());
    };
    const select = async (input: HTMLSelectElement, value: string) =>
      React.act(async () => {
        input.value = value;
        input.dispatchEvent(new dom.window.Event("change", { bubbles: true }));
      });
    for (const theme of ["light", "dark", "system"])
      await select(document.getElementById("theme-mode") as HTMLSelectElement, theme);
    assert.deepEqual(themes, ["light", "dark", "system"]);
    for (const zoom of [80, 90, 100, 110, 125, 150, 175, 200]) {
      await change("Page zoom", String(zoom));
      assert.equal(document.documentElement.style.zoom, String(zoom / 100));
      assert.equal(window.localStorage.getItem("nexus.launcher.zoom"), String(zoom));
    }
    for (const level of ["error", "warn", "info", "debug", "trace"]) {
      await select(
        document.querySelector('select[aria-label="Agent log level"]') as HTMLSelectElement,
        level,
      );
      assert.ok(calls.some((c) => c.command === "agent_log_set" && c.args.level === level));
      assert.equal(window.localStorage.getItem("nexus.launcher.agent-log-level"), level);
    }
    const fields = [
      ["Harness data directory", "home", "C:/audit/home"],
      ["Web port", "port", "0"],
      ["Open browser after launch", "open_browser", "false"],
      ["Disable session telemetry", "telemetry_disabled", "false"],
      ["DeepSeek model API address", "deepseek_base_url", "https://model.example"],
      ["DeepSeek search API address", "search_base_url", "https://search.example"],
      ["Search provider ID", "search_provider", "search-a"],
      ["Web fetch provider ID", "fetch_provider", "fetch-a"],
      ["Shared agent skills directory", "agents_home", "C:/audit/agents"],
      ["Bundled skills directory", "bundled_skill_dir", "C:/audit/skills"],
      ["Permission mode", "permission_mode", "read-only"],
      ["Tool mode (temporary upstream option)", "tools_mode", "ptc"],
      ["Context window (sdk-minimal only)", "context_window", "12345"],
      ["Treat token limit as success (sdk only)", "max_tokens_as_success", "false"],
      ["System prompt (sdk-minimal only)", "system_prompt", "audit prompt"],
    ];
    for (const [label, , value] of fields) await change(label, value);
    await click("Save Harness preferences");
    const saved = actions.find((a) => a[2].action === "set_harness_preferences")?.[2]
      .harness_preferences;
    assert.ok(saved);
    for (const [, key, value] of fields)
      assert.equal(
        saved[key],
        ["port", "context_window"].includes(key)
          ? Number(value)
          : value === "false"
            ? false
            : value,
        key,
      );
    for (const name of ["node", "pnpm", "git"]) await change(`${name} pin`, `C:/audit/${name}.exe`);
    await click("Save runtime settings");
    const runtime = actions.find((a) => a[2].action === "set_runtime")[2].runtime;
    for (const name of ["node", "pnpm", "git"])
      assert.equal(runtime[name].path, `C:/audit/${name}.exe`);
    await change("Readiness timeout (seconds)", "45");
    await change("Readiness URL", "http://127.0.0.1:12345/health");
    const token = [...document.querySelectorAll<HTMLInputElement>('input[type="checkbox"]')].find(
      (e) => e.closest("label")?.textContent?.includes("Require a fresh Harness token"),
    )!;
    assert.ok(token);
    await React.act(async () => token.click());
    await click("Save startup parameters");
    const launch = actions.find((a) => a[2].action === "set_harness")[2].harness;
    assert.equal(launch.readiness_timeout_secs, 45);
    assert.equal(launch.readiness_url, "http://127.0.0.1:12345/health");
    assert.equal(launch.readiness_token_required, true);
    await change("External Harness directory", "C:/audit/harness");
    await click("Confirm external directory");
    assert.equal(
      actions.find((a) => a[2].action === "set_external_harness")[2].external_harness_path,
      "C:/audit/harness",
    );
    const categories = [
      ...document.querySelectorAll(".notification-category-grid input"),
    ] as HTMLInputElement[];
    assert.equal(categories.length, 9);
    for (const input of categories) await React.act(async () => input.click());
    await click("Save notifications");
    const notification = calls.find(
      (c) =>
        c.command === "proxy_request" &&
        c.args.path === "/v1/notifications" &&
        c.args.method === "POST",
    ).args.body;
    assert.equal(Object.keys(notification.categories).length, 9);
    assert.ok(Object.values(notification.categories).every((v) => v === false));
    const channels = document.querySelectorAll<HTMLSelectElement>(".notification-channels select");
    for (const mode of ["off", "unfocused", "always"]) {
      await select(channels[0], mode);
      await select(channels[1], mode);
      await click("Save notifications");
      const saved = calls
        .filter(
          (c) =>
            c.command === "proxy_request" &&
            c.args.path === "/v1/notifications" &&
            c.args.method === "POST",
        )
        .at(-1).args.body;
      assert.equal(saved.desktop, mode);
      assert.equal(saved.terminal, mode);
    }
    for (const method of ["auto", "osc9", "bel"]) {
      await select(channels[2], method);
      await click("Save notifications");
      assert.equal(
        calls.filter((c) => c.args?.method === "POST" && c.args.path === "/v1/notifications").at(-1)
          .args.body.method,
        method,
      );
    }
    const observer = document.querySelector<HTMLInputElement>(".notification-observer input")!;
    await React.act(async () => observer.click());
    await click("Save notifications");
    assert.equal(
      calls.filter((c) => c.args?.method === "POST" && c.args.path === "/v1/notifications").at(-1)
        .args.body.observer,
      false,
    );
    await click("Add patch");
    await change(
      "Local absolute path or HTTPS / GitHub file URL",
      "https://github.com/example/project/blob/main/config.yml",
    );
    await change("GitHub reference type", "tag");
    await change("GitHub reference name", "v2");
    await change("GitHub file path", "config/desktop.yml");
    const enabled = document.querySelector<HTMLInputElement>(
      '.patch-entry input[type="checkbox"]',
    )!;
    await React.act(async () => enabled.click());
    await click("Save Harness preferences");
    const patch = actions.filter((a) => a[2].action === "set_harness_preferences").at(-1)[2]
      .harness_preferences.patch_entries[0];
    assert.equal(patch.enabled, false);
    assert.equal(patch.github_ref_kind, "tag");
    assert.equal(patch.github_ref_name, "v2");
    assert.equal(patch.github_file_path, "config/desktop.yml");
    assert.equal(patch.source, "https://github.com/example/project/blob/main/config.yml");
    const autostart = [...document.querySelectorAll(".integration-list input")].find((e) =>
      e.closest("div")?.textContent?.includes("Launch on system startup"),
    ) as HTMLInputElement;
    assert.ok(autostart);
    failAutostart = true;
    await React.act(async () => autostart.click());
    assert.match(document.body.textContent!, /fixture permission denied/);
    assert.equal(autostart.checked, false);
    await select(document.getElementById("locale-mode") as HTMLSelectElement, "zh");
    assert.equal(document.documentElement.lang, "zh-CN");
    assert.equal(window.localStorage.getItem("nexus.launcher.locale"), "zh");
    assert.match(document.body.textContent!, /通知设置/);
    await select(document.getElementById("locale-mode") as HTMLSelectElement, "en");
    assert.equal(document.documentElement.lang, "en");
  } finally {
    await React.act(async () => root.unmount());
    clearMocks();
    await loader.close();
    dom.window.close();
    for (const [k, v] of old) {
      if (v) Object.defineProperty(globalThis, k, v);
      else delete (globalThis as any)[k];
    }
  }
});
