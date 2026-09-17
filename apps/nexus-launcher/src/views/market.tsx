import { useEffect, useRef, useState, useId } from "react";
import { Storefront, WarningCircle } from "@phosphor-icons/react";
import { useI18n } from "../i18n";
import { proxyRequest } from "../agent-bridge";
import { errorMessage } from "../display-format";
import { Panel, ActionButton, PageIntro } from "../ui-components";

import { type ViewProps } from "../app-types";
import { stringValue, asObject, arrayValue, harnessRuntimeValue } from "../json-values";
import { pluginIsolationChoice } from "../control-state";
export function BuiltinPluginsView(props: ViewProps) {
  const { snapshot } = props;
  const { t } = useI18n();
  return (
    <>
      <PageIntro
        kicker={t("Built-in plugins")}
        title={t("Built-in plugin management")}
        detail={t("Manage Nexus plugin integrations for the active profile.")}
      />
      <MarketplaceSettings
        {...props}
        key={
          stringValue(snapshot.profiles, "active_profile") +
          ":" +
          JSON.stringify(snapshot.config?.harness_preferences)
        }
      />
    </>
  );
}
type MarketState = {
  profile: string;
  scope: string;
  provider: string;
  status: string;
  installed: boolean;
};
export function MarketplaceSettings({
  openProfiles,
  snapshot,
  busyAction,
  runAction,
  refresh,
}: ViewProps) {
  const { locale } = useI18n();
  const text = (en: string, zh: string) => (locale === "zh" ? zh : en);
  const [state, setState] = useState<MarketState>();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [saved, setSaved] = useState(false);
  const pending = useRef(false);
  const helpId = useId();
  useEffect(() => {
    let live = true;
    void proxyRequest<MarketState>("/v1/market")
      .then((value) => {
        if (live) {
          setState(value);
        }
      })
      .catch((e) => {
        if (live) setError(errorMessage(e));
      });
    return () => {
      live = false;
    };
  }, []);
  const runtimeState = stringValue(harnessRuntimeValue(snapshot.harnessRuntime), "state");
  const blocked =
    busy ||
    busyAction !== null ||
    snapshot.startup?.available !== true ||
    !["stopped", "failed", "detached"].includes(runtimeState || "");
  const isolation = pluginIsolationChoice(
    asObject(snapshot.profiles),
    state?.profile || "",
    "dshmarket",
    blocked,
  );
  const manifest = arrayValue(snapshot.profiles, "manifests").find(
    (item) => stringValue(item, "name") === state?.profile,
  );
  const plugin = arrayValue(manifest, "plugins").find(
    (item) => stringValue(item, "package") === "dshmarket",
  );
  const inventoryKnown =
    !!manifest &&
    Array.isArray(asObject(manifest).plugins) &&
    !stringValue(manifest, "source_profile");
  const installed = state?.installed || isolation.disabled === true || !!plugin;
  const canToggle = !blocked && inventoryKnown && (!installed || !!isolation.command);
  async function save() {
    if (!state || pending.current || !canToggle) return;
    pending.current = true;
    setBusy(true);
    setError("");
    setSaved(false);
    try {
      if (installed && isolation.command) {
        const succeeded = await runAction(
          text("Change plugin state", "更改插件状态"),
          "/v1/profiles",
          isolation.command,
        );
        if (succeeded === false) return;
      } else {
        // Recheck before the pinned first install so independent updates are not overwritten.
        const catalog = await proxyRequest("/v1/profiles");
        const latest = await proxyRequest<MarketState>("/v1/market");
        const target = arrayValue(catalog, "manifests").find(
          (item) => stringValue(item, "name") === state.profile,
        );
        if (
          latest.profile !== state.profile ||
          latest.scope !== state.scope ||
          latest.installed ||
          !target ||
          !Array.isArray(asObject(target).plugins) ||
          stringValue(target, "source_profile") ||
          arrayValue(target, "plugins").some(
            (item) => stringValue(item, "package") === "dshmarket",
          ) ||
          arrayValue(catalog, "disabled_plugins").includes("dshmarket")
        ) {
          throw new Error(
            text(
              "Plugin state changed or could not be verified. Refresh before enabling.",
              "插件状态已变化或无法确认，请刷新后再启用。",
            ),
          );
        }
        await proxyRequest<MarketState>("/v1/market", "POST", {
          profile: state.profile,
          scope: state.scope,
          provider: "dsh-market",
        });
      }
      setState(await proxyRequest<MarketState>("/v1/market"));
      await refresh();
      setSaved(true);
    } catch (e) {
      setError(errorMessage(e));
      try {
        setState(await proxyRequest<MarketState>("/v1/market"));
      } catch {
        /* Preserve last known selection. */
      }
      await refresh();
    } finally {
      pending.current = false;
      setBusy(false);
    }
  }
  return (
    <Panel title={text("Available plugins", "可用插件")} icon={<Storefront size={18} />}>
      {state ? (
        <>
          <div className="plugin-target">
            <div>
              <span className="field-label">
                {text("Current installation target", "当前安装目标")}
              </span>
              <strong>{state.profile}</strong>
            </div>
            {openProfiles && (
              <ActionButton onClick={openProfiles}>
                {text("Switch profile", "切换配置档")}
              </ActionButton>
            )}
          </div>
          <p className="field-help">
            {text(
              "Plugins apply only to this profile. To install for another profile, stop Harness and select that profile in Profiles first.",
              "插件仅应用于当前配置档。如需给其他配置档安装，请先停止 Harness，并在“配置与插件”中切换配置档。",
            )}
          </p>
          <div className="builtin-plugin-list">
            <div className="builtin-plugin">
              <div className="builtin-plugin-row">
                <Storefront size={20} aria-hidden="true" />
                <span className="builtin-plugin-name">
                  <strong>{text("dsh-market", "dsh-market")}</strong>
                  <span>
                    {text(
                      "Browse and install community plugins in Harness.",
                      "在 Harness 中浏览和安装社区插件。",
                    )}
                  </span>
                </span>
                <div className="builtin-plugin-actions">
                  <ActionButton
                    tone={installed && !isolation.disabled ? undefined : "primary"}
                    disabled={!canToggle}
                    onClick={() => void save()}
                  >
                    {busy
                      ? text("Applying…", "正在应用…")
                      : installed && !isolation.disabled
                        ? text("Disable", "禁用")
                        : text("Enable", "启用")}
                  </ActionButton>
                  <span className="plugin-help">
                    <button
                      type="button"
                      className="icon-button"
                      aria-label={text("Plugin details", "插件操作说明")}
                      aria-describedby={helpId}
                    >
                      <WarningCircle size={18} />
                    </button>
                    <span role="tooltip" id={helpId} className="plugin-tooltip">
                      {stringValue(plugin, "version") && (
                        <p className="field-help">
                          {text("Profile version declaration: ", "配置档版本声明：")}
                          {stringValue(plugin, "version")}
                        </p>
                      )}
                      <p className="field-help">
                        {text(
                          "Stop Harness before changing this plugin. First enable installs dsh-market 1.38.1 from npm only when absent. Existing versions are not reinstalled. Disabling keeps its files; changes apply on the next compatibility check and launch. After starting Harness, open Settings → Plugin Market.",
                          "请先停止 Harness。仅未安装时，首次启用从 npm 安装 dsh-market 1.38.1；已有版本不会重装，禁用保留插件文件，在下次兼容性检查与启动时生效。启动 Harness 后，在其“设置 → 插件市场”中使用。",
                        )}
                      </p>
                    </span>
                  </span>
                  {saved && <span role="status">{text("Saved", "已保存")}</span>}
                </div>
              </div>
              {blocked && (
                <small className="field-help">
                  {text(
                    "Stop Harness and wait for other operations to finish before changing plugins.",
                    "请停止 Harness，并等待其他操作结束后修改插件。",
                  )}
                </small>
              )}
              {(!inventoryKnown || (installed && !isolation.known)) && (
                <small role="status">
                  {text(
                    "Refresh to verify the plugin state before changing it.",
                    "请刷新并确认插件状态后再修改。",
                  )}
                </small>
              )}
              {state.status !== "ready" && (
                <small role="status">
                  {text(
                    "The previous operation did not finish. Retry or use plugin recovery.",
                    "上次操作未完成，请重试或使用插件恢复。",
                  )}
                </small>
              )}
            </div>
          </div>
        </>
      ) : (
        !error && <p role="status">{text("Loading plugins…", "正在读取插件…")}</p>
      )}
      {error && <p className="form-error" role="alert">{error}</p>}
    </Panel>
  );
}
