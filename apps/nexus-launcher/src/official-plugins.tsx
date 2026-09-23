import { useEffect, useRef, useState } from "react";
import { PuzzlePiece } from "@phosphor-icons/react";
import { proxyRequest } from "./agent-bridge";
import { useI18n } from "./i18n";
import { Panel, ActionButton, Modal } from "./ui-components";
import { confirmAction } from "./confirmation";
import { errorMessage } from "./display-format";
import { asObject, stringValue } from "./json-values";
import type { ViewProps } from "./app-types";
type LocalText = string | Record<string, string>;
type Bundle = {
  name: string;
  version?: string;
  repository?: string;
  description?: string;
  meta?: { title?: LocalText; description?: LocalText; error?: string };
  installed: boolean;
  optional: boolean;
  enabled: boolean;
  removable: boolean;
  readOnlyReason?: string;
  error?: { code: string; diagnostic?: string };
  rows: {
    rowId: string;
    moduleName: string;
    meta?: { title?: LocalText; description?: LocalText; error?: string };
  }[];
};
type Reply = {
  profile: string;
  bundles: Bundle[];
  result?: {
    application?: string;
    error?: { code: string; diagnostic?: string };
    status?: string;
    reason?: string;
    bundle?: string;
    packageResult?: { output: string };
    pendingBuilds?: string[];
  };
};
export function pluginRepository(value?: string): string | undefined {
  if (!value) return;
  const normalized = value
    .replace(/^git\+/, "")
    .replace(/^git@github\.com:/, "https://github.com/")
    .replace(/^github:/, "https://github.com/")
    .replace(/^git:\/\/github\.com\//, "https://github.com/");
  try {
    const url = new URL(normalized);
    if (
      url.protocol !== "https:" ||
      url.hostname !== "github.com" ||
      url.username ||
      url.password ||
      url.port
    )
      return;
    if (!/^\/[^/]+\/[^/]+\/?$/.test(url.pathname)) return;
    url.pathname = url.pathname.replace(/\.git\/?$/, "");
    url.search = "";
    url.hash = "";
    return url.href;
  } catch {
    return;
  }
}

export function OfficialPlugins({
  snapshot,
  busyAction,
  refresh,
  profile,
}: ViewProps & { profile: string }) {
  const { locale, t } = useI18n();
  const text = (en: string, zh: string) => (locale === "zh" ? zh : en);
  const local = (value: LocalText | undefined, fallback = "") =>
    typeof value === "string"
      ? value
      : value?.[locale === "zh" ? "zh-CN" : "en"] || value?.[locale] || value?.en || fallback;
  const [data, setData] = useState<Reply>();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [selected, setSelected] = useState<string>();
  const [add, setAdd] = useState(false);
  const [spec, setSpec] = useState("");
  const [checked, setChecked] = useState("");
  const request = useRef(0);
  const pending = useRef(false);
  const recovery = asObject(snapshot.recovery),
    harness = asObject(recovery.harness);
  const stopped =
    ["stopped", "failed"].includes(String(harness.state)) &&
    recovery.harness_stop_required !== true;
  const active = stringValue(snapshot.profiles, "active_profile") === profile;
  const blocked = !stopped || busyAction !== null || busy;
  const reason = (code: string) =>
    ({
      "management-required": text(
        "Required for plugin management; it cannot be disabled or removed.",
        "插件管理需要此插件，不能禁用或移除。",
      ),
      "not-removable": text("This plugin cannot be removed.", "此插件不可移除。"),
      "stop-profile": text(
        "Stop Harness before removing this plugin.",
        "请先停止 Harness 再移除此插件。",
      ),
    })[code] || code;
  async function perform(action: string, name?: string) {
    if (pending.current || !stopped || busyAction !== null) return;
    pending.current = true;
    setBusy(true);
    setError("");
    setNotice("");
    const token = ++request.current;
    try {
      const reply = await proxyRequest<Reply>("/v1/plugin-manager", "POST", {
        profile,
        action,
        ...(name ? { package: name } : {}),
      });
      if (token !== request.current) return;
      setData(reply);
      const result = reply.result;
      if (result?.application === "failed" || result?.status === "refused") {
        setError(
          [
            result.error ? reason(result.error.code) : result.reason,
            result.error?.diagnostic,
            result.packageResult?.output,
            result.pendingBuilds?.length
              ? text(
                  "Package scripts need explicit approval in Harness.",
                  "包脚本需要在 Harness 中明确批准。",
                )
              : null,
          ]
            .filter(Boolean)
            .join("\n"),
        );
        setChecked("");
      } else if (action === "inspect") setChecked(name || "");
      else if (action !== "list") {
        setNotice(
          text(
            "Saved. The change takes effect the next time Harness starts.",
            "已保存，下次启动 Harness 时生效。",
          ),
        );
        if (action === "install") {
          setAdd(false);
          setSpec("");
          setChecked("");
        }
        await refresh();
      }
    } catch (cause) {
      if (token === request.current) setError(errorMessage(cause));
    } finally {
      if (token === request.current) {
        pending.current = false;
        setBusy(false);
      }
    }
  }
  useEffect(() => {
    setBusy(false);
    void perform("list");
    return () => {
      request.current++;
      pending.current = false;
    };
  }, [profile, stopped]);
  const bundles = data?.bundles || [];
  const opened = bundles.find((row) => row.name === selected);
  const official = bundles.filter((row) => row.optional && !row.installed);
  const installed = bundles.filter((row) => row.installed || (!row.optional && !!row.error));
  const core = bundles.filter((row) => !row.optional && !row.installed && !row.error);
  const title = (row: Bundle) => local(row.meta?.title, row.name);
  const warnings = (row: Bundle) =>
    [row.meta?.error, ...row.rows.map((item) => item.meta?.error)].filter(
      (item): item is string => !!item,
    );
  function toggle(row: Bundle) {
    return (
      <button
        type="button"
        role="switch"
        aria-checked={row.enabled}
        aria-label={title(row)}
        className="official-plugin-switch"
        disabled={blocked || !!row.readOnlyReason || (!row.enabled && !!row.error)}
        title={row.readOnlyReason ? reason(row.readOnlyReason) : undefined}
        onClick={() => void perform(row.enabled ? "disable" : "enable", row.name)}
      >
        <span />
      </button>
    );
  }
  function group(rows: Bundle[], label: string) {
    return rows.length ? (
      <section className="official-plugin-group">
        <h3>
          {label} <span className="muted">{rows.length}</span>
        </h3>
        {rows.map((row) => (
          <div
            className={`official-plugin-row${row.error ? " plugin-has-error" : warnings(row).length ? " plugin-has-warning" : ""}`}
            key={row.name}
          >
            <PuzzlePiece size={24} />
            <button className="official-plugin-title" onClick={() => setSelected(row.name)}>
              <strong>{title(row)}</strong>
              <span>
                {row.version ? `v${row.version}` : text("Version unavailable", "版本未知")}
              </span>
              {(row.error || warnings(row).length > 0) && (
                <span className="plugin-issue-label">
                  {row.error
                    ? text("Plugin error", "插件异常")
                    : text("Metadata warning", "元数据告警")}
                </span>
              )}
              <span>{local(row.meta?.description, row.description)}</span>
              {row.error && <span className="error-text">{reason(row.error.code)}</span>}
            </button>
            {pluginRepository(row.repository) && (
              <a
                href={pluginRepository(row.repository)}
                target="_blank"
                rel="noopener noreferrer"
                aria-label={`${title(row)} GitHub`}
              >
                {t("GitHub repository")} ↗
              </a>
            )}
            {toggle(row)}
          </div>
        ))}
      </section>
    ) : null;
  }
  return (
    <Panel title={text("Plugins", "插件")} icon={<PuzzlePiece size={18} />}>
      <div className="actions">
        <span className="field-help">
          {text("Repair target", "修复对象")}: <strong>{profile}</strong> ·{" "}
          {active
            ? text("Current profile", "当前配置档")
            : text("Current selection remains unchanged", "不改变当前配置档")}
        </span>
        <ActionButton disabled={blocked} onClick={() => void perform("list")}>
          {text("Refresh", "刷新")}
        </ActionButton>
        <ActionButton disabled={blocked} onClick={() => setAdd(true)}>
          {text("Add plugin", "添加插件")}
        </ActionButton>
      </div>
      {!stopped ? (
        <p role="status">
          {text(
            "Stop Harness to manage this profile here.",
            "请先停止 Harness，再在此管理配置档。",
          )}
        </p>
      ) : null}
      <p className="field-help">
        {text(
          "Offline enable, disable and removal use the selected Harness implementation without starting profile plugins. Installing new packages may require a network connection.",
          "离线启用、禁用和移除使用所选 Harness 的官方实现，不启动配置档插件。安装新包可能需要联网。",
        )}
      </p>
      {busy && <p role="status">{text("Working…", "正在处理…")}</p>}
      {error && (
        <p className="error-text" role="alert" style={{ whiteSpace: "pre-wrap" }}>
          {error}
        </p>
      )}
      {notice && <p role="status">{notice}</p>}
      {group(official, text("Official", "官方"))}
      {group(installed, text("Installed", "已安装"))}
      {core.length > 0 && (
        <details>
          <summary>{text("Built-in profile components", "配置档内置组件")}</summary>
          {group(core, text("Built-in", "内置"))}
        </details>
      )}
      {data && !bundles.length && <p>{text("No plugins reported.", "未发现插件。")}</p>}
      {opened && (
        <Modal variant="drawer" title={title(opened)} onClose={() => setSelected(undefined)}>
          <p>
            {opened.name} · {opened.version || text("Version unavailable", "版本未知")}
          </p>
          <p>{local(opened.meta?.description, opened.description)}</p>
          {pluginRepository(opened.repository) && (
            <p>
              <a
                href={pluginRepository(opened.repository)}
                target="_blank"
                rel="noopener noreferrer"
              >
                {t("GitHub repository")} ↗
              </a>
            </p>
          )}
          {opened.error && (
            <p className="error-text" role="alert">
              {text("Plugin error", "插件异常")}: {reason(opened.error.code)}
              {opened.error.diagnostic && ` — ${opened.error.diagnostic}`}
            </p>
          )}
          {warnings(opened).length > 0 && (
            <div role="status">
              <strong>{text("Metadata warning", "元数据告警")}</strong>
              <ul>
                {[...new Set(warnings(opened))].map((message) => (
                  <li key={message}>{message}</li>
                ))}
              </ul>
            </div>
          )}
          {opened.readOnlyReason && <p>{reason(opened.readOnlyReason)}</p>}
          {toggle(opened)}
          <dl className="snapshot-fields">
            {opened.rows.map((row) => (
              <div key={row.rowId}>
                <dt>{local(row.meta?.title, row.rowId)}</dt>
                <dd>{local(row.meta?.description, row.moduleName)}</dd>
              </div>
            ))}
          </dl>
          {opened.removable && (
            <ActionButton
              disabled={blocked}
              onClick={async () => {
                if (await confirmAction(text(`Remove ${opened.name}?`, `移除 ${opened.name}？`)))
                  await perform("remove", opened.name);
              }}
            >
              {text("Remove", "移除")}
            </ActionButton>
          )}
        </Modal>
      )}
      {add && (
        <Modal
          title={text("Add plugin", "添加插件")}
          onClose={() => {
            if (!busy) {
              setAdd(false);
              setChecked("");
            }
          }}
        >
          <label>
            {text("Package name or local path", "包名或本地路径")}
            <input
              value={spec}
              disabled={busy}
              onChange={(e) => {
                setSpec(e.target.value);
                setChecked("");
              }}
            />
          </label>
          <p className="field-help">
            {text(
              "Installation uses the selected Harness manager. New plugins remain disabled until you enable them.",
              "安装使用所选 Harness 的官方管理器。新插件安装后保持禁用，由你决定何时启用。",
            )}
          </p>
          {error && (
            <p role="alert" className="error-text">
              {error}
            </p>
          )}
          <p className="field-help">
            {text(
              "Offline enable, disable and removal use the selected Harness implementation without starting profile plugins. Installing new packages may require a network connection.",
              "离线启用、禁用和移除使用所选 Harness 的官方实现，不启动配置档插件。安装新包可能需要联网。",
            )}
          </p>
          {busy && <p role="status">{text("Working…", "正在处理…")}</p>}
          <div className="actions">
            <ActionButton
              disabled={blocked || !spec.trim()}
              onClick={() => void perform("inspect", spec.trim())}
            >
              {text("Check package", "检查插件")}
            </ActionButton>
            <ActionButton
              disabled={blocked || !checked || checked !== spec.trim()}
              onClick={() => void perform("install", checked)}
            >
              {text("Install", "安装")}
            </ActionButton>
          </div>
        </Modal>
      )}
    </Panel>
  );
}
