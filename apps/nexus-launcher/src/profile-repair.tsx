import { useEffect, useRef, useState } from "react";
import { Wrench } from "@phosphor-icons/react";
import { proxyRequest } from "./agent-bridge";
import { Panel, ActionButton } from "./ui-components";
import { confirmAction } from "./confirmation";
import { errorMessage } from "./display-format";
import { useI18n } from "./i18n";
import type { ViewProps } from "./app-types";
import { OfficialPlugins } from "./official-plugins";
import { asObject } from "./json-values";
type File = { file: string; content: string | null; fingerprint: string; error?: string };
type Report = {
  profile: string;
  files: File[];
  backups: { id: string; file: string; created: number }[];
  saved_backup?: string;
};
export function OfflineProfileRepair(props: ViewProps) {
  const { locale } = useI18n();
  const zh = locale === "zh";
  const text = (en: string, cn: string) => (zh ? cn : en);
  const [names, setNames] = useState<string[]>([]),
    [target, setTarget] = useState("");
  const [report, setReport] = useState<Report>(),
    [file, setFile] = useState("package.json"),
    [draft, setDraft] = useState("");
  const [pending, setPending] = useState(false),
    [error, setError] = useState(""),
    [notice, setNotice] = useState("");
  const sequence = useRef(0),
    running = useRef(false);
  const recovery = asObject(props.snapshot.recovery);
  const stopped =
    ["stopped", "failed"].includes(String(asObject(recovery.harness).state)) &&
    recovery.harness_stop_required !== true;
  const disabled = pending || !stopped || props.busyAction !== null;
  const selected = report?.files.find((item) => item.file === file);
  async function action(kind: string, extra: Record<string, unknown> = {}, profile = target) {
    if (running.current || !stopped || props.busyAction !== null) return;
    running.current = true;
    setPending(true);
    setError("");
    setNotice("");
    const token = ++sequence.current;
    try {
      const value = await proxyRequest<any>("/v1/profile-repair", "POST", {
        action: kind,
        ...(profile ? { profile } : {}),
        ...extra,
      });
      if (sequence.current !== token) return;
      if (kind === "list") {
        const next = value.profiles.includes(target) ? target : value.profiles[0] || "";
        setNames(value.profiles);
        setTarget(next);
        setReport(undefined);
        setDraft("");
        if (next) {
          const inspected = await proxyRequest<Report>("/v1/profile-repair", "POST", {
            action: "inspect",
            profile: next,
          });
          if (sequence.current === token) {
            setReport(inspected);
            setDraft(inspected.files.find((item) => item.file === file)?.content || "");
          }
        }
      } else {
        setReport(value);
        setDraft(value.files.find((item: File) => item.file === file)?.content || "");
      }
      if (kind === "save" || kind === "restore") {
        setNotice(
          text(
            "Saved and read back. Review the checks below; Harness has not been started. A recovery point was kept.",
            "已保存并回读验证。请查看下方检查结果；尚未启动 Harness，修改前已保存恢复点。",
          ),
        );
        await props.refresh();
      }
    } catch (cause) {
      if (sequence.current === token) setError(errorMessage(cause));
    } finally {
      if (sequence.current === token) {
        running.current = false;
        setPending(false);
      }
    }
  }
  useEffect(() => {
    void action("list", {}, "");
    return () => {
      sequence.current++;
      running.current = false;
    };
  }, [stopped, props.snapshot.health?.instance_id]);
  const loadFile = (name: string) => {
    setFile(name);
    setDraft(report?.files.find((item) => item.file === name)?.content || "");
  };
  return (
    <section>
      <Panel title={text("Offline profile repair", "离线配置档修复")} icon={<Wrench size={20} />}>
        <p>
          {text(
            "Repair an existing profile without selecting or starting it. Invalid configuration remains visible. Recovery points restore configuration files, not removed plugin packages or conversations.",
            "无需切换或启动配置档即可修复。损坏配置仍可查看。恢复点只回退配置文件，不恢复已卸载的插件包，也不修改会话。",
          )}
        </p>
        {!stopped && (
          <p role="status">
            {text("Stop Web and Desktop before repair.", "请先停止 Web 和 Desktop，再进行修复。")}
          </p>
        )}
        <label>
          {text("Repair target", "修复对象")}
          <select
            value={target}
            disabled={disabled}
            onChange={(event) => {
              setTarget(event.target.value);
              setReport(undefined);
              setDraft("");
              void action("inspect", {}, event.target.value);
            }}
          >
            {names.map((name) => (
              <option key={name}>{name}</option>
            ))}
          </select>
        </label>
        <ActionButton disabled={disabled} onClick={() => void action("list", {}, "")}>
          {text("Refresh profiles", "刷新配置档")}
        </ActionButton>
        <ActionButton disabled={disabled || !target} onClick={() => void action("inspect")}>
          {text("Check configuration again", "重新检查配置")}
        </ActionButton>
        {error && (
          <p role="alert" className="error-text">
            {error}
          </p>
        )}
        {notice && <p role="status">{notice}</p>}
        {report && (
          <>
            <p>
              <strong>
                {text("Editing", "正在修复")}: {report.profile}
              </strong>{" "}
              · {text("Current selection is unchanged", "不改变当前配置档")}
            </p>
            <ul>
              {report.files.map((item) => (
                <li key={item.file}>
                  <strong>{item.file}</strong>:{" "}
                  {item.error ||
                    text(
                      "Format check passed; plugin compatibility not verified",
                      "格式检查通过，未验证插件兼容性",
                    )}
                  {item.error && (
                    <ActionButton disabled={disabled} onClick={() => loadFile(item.file)}>
                      {text("Edit this file", "修复此文件")}
                    </ActionButton>
                  )}
                </li>
              ))}
            </ul>
            <label>
              {text("Configuration file", "配置文件")}
              <select
                value={file}
                disabled={disabled}
                onChange={(event) => loadFile(event.target.value)}
              >
                {report.files.map((item) => (
                  <option key={item.file}>{item.file}</option>
                ))}
              </select>
            </label>
            <textarea
              aria-label={text("Configuration source", "配置源码")}
              value={draft}
              disabled={disabled}
              onChange={(event) => setDraft(event.target.value)}
              rows={14}
              spellCheck={false}
              style={{ width: "100%", fontFamily: "monospace" }}
            />
            <ActionButton
              disabled={disabled || !selected}
              onClick={async () => {
                if (
                  await confirmAction(
                    text(`Back up and save ${target}/${file}?`, `备份并保存 ${target}/${file}？`),
                  )
                )
                  void action("save", { file, content: draft, fingerprint: selected?.fingerprint });
              }}
            >
              {text("Back up, save and check", "备份、保存并检查")}
            </ActionButton>
            <details>
              <summary>{text("Configuration recovery points", "配置恢复点")}</summary>
              {report.backups
                .filter((item) => item.file === file)
                .map((item) => (
                  <p key={item.id}>
                    {new Date(item.created * 1000).toLocaleString(locale)} · {item.file}{" "}
                    <ActionButton
                      disabled={disabled}
                      onClick={async () => {
                        if (
                          await confirmAction(
                            text(
                              `Restore ${target}/${file}? The saved file may contain the original error. Current content will also be backed up.`,
                              `恢复 ${target}/${file}？恢复点可能包含原来的错误，当前内容也会先备份。`,
                            ),
                          )
                        )
                          void action("restore", {
                            file,
                            fingerprint: selected?.fingerprint,
                            backup: item.id,
                          });
                      }}
                    >
                      {text("Restore this file", "恢复此文件")}
                    </ActionButton>
                  </p>
                ))}
              {!report.backups.some((item) => item.file === file) && (
                <p>{text("No recovery point for this file yet.", "此文件暂无恢复点。")}</p>
              )}
            </details>
            <p>
              {text(
                "For missing packages, inspect Local Harness dependencies above. For plugin errors, use the official controls below; do not disable plugins based only on missing service names.",
                "缺包请使用上方的本地依赖检查；插件错误请使用下方官方管理，不应仅凭缺失服务名称判断要禁用哪个插件。",
              )}
            </p>
            {props.openWorkbench && (
              <ActionButton disabled={disabled} onClick={props.openWorkbench}>
                {text("Workbench: verify actual startup", "前往工作台验证实际启动")}
              </ActionButton>
            )}
          </>
        )}
      </Panel>
      {target &&
        !report?.files.some((item) => item.error) &&
        report &&
        asObject(props.snapshot.profiles).official_plugin_management === true && (
          <OfficialPlugins {...props} profile={target} key={target} />
        )}
    </section>
  );
}
