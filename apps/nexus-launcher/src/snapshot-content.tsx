import { useState } from "react";
import { load, JSON_SCHEMA } from "js-yaml";
import { useI18n } from "./i18n";
import { asObject, arrayValue, stringValue, numberValue, booleanValue } from "./json-values";
import { formatTimestamp, localizedRuntimeState, snapshotContentNote } from "./display-format";
import { type JsonObject } from "./app-types";

const labels: Record<string, string> = {
  name: "Name",
  version: "Version",
  description: "Description",
  dependencies: "Dependencies",
  devDependencies: "Development dependencies",
  optionalDependencies: "Optional dependencies",
  dsh: "Harness",
  profile: "Profile",
  bundles: "Enabled plugins",
  disabled: "Disabled",
  plugins: "Plugins",
  config: "Configuration",
  settings: "Settings",
  id: "ID",
  enabled: "Enabled",
  port: "Port",
  models: "Models",
  providers: "Providers",
};

// A bounded projection handles cyclic YAML aliases without inventing a schema.
export function readableSnapshotFields(
  content: string,
): Array<{ key: string; value: unknown; note?: "nested" | "empty" }> | null {
  try {
    const value: unknown = load(content, { schema: JSON_SCHEMA });
    if (value === null || typeof value !== "object") return null;
    const rows: Array<{ key: string; value: unknown; note?: "nested" | "empty" }> = [];
    const seen = new WeakSet<object>();
    const visit = (item: unknown, key: string, depth: number) => {
      if (rows.length >= 200) return;
      if (item && typeof item === "object") {
        if (seen.has(item) || depth >= 10) {
          rows.push({ key, value: null, note: "nested" });
          return;
        }
        seen.add(item);
        const entries = Object.entries(item);
        if (!entries.length) rows.push({ key, value: null, note: "empty" });
        for (const [name, child] of entries)
          visit(child, key ? `${key} / ${name}` : name, depth + 1);
        seen.delete(item);
      } else rows.push({ key, value: item });
    };
    visit(value, "", 0);
    return rows;
  } catch {
    return null;
  }
}

export function SnapshotContent({ value }: { value: JsonObject | null }) {
  const { t, locale } = useI18n();
  const [source, setSource] = useState(false);
  const summary = asObject(asObject(value).summary);
  const files = arrayValue(value, "files");
  const errors = arrayValue(value, "errors").map(String);
  const timezone = Intl.DateTimeFormat().resolvedOptions().timeZone;
  return (
    <div className="snapshot-reader">
      <div className="snapshot-reader-toolbar">
        <span className="field-help">
          {t("Local time")} · {timezone}
        </span>
        <div className="snapshot-mode" role="group" aria-label={t("Display mode")}>
          <button type="button" aria-pressed={!source} onClick={() => setSource(false)}>
            {t("Visual")}
          </button>
          <button type="button" aria-pressed={source} onClick={() => setSource(true)}>
            {t("Source code")}
          </button>
        </div>
      </div>
      <dl className="detail-list compact-details snapshot-overview">
        {[
          [
            t("Created"),
            formatTimestamp(
              (numberValue(summary, "created_unix_ms") ?? 0) / 1000,
              t("Time unavailable"),
              locale,
            ),
          ],
          [t("Profile"), stringValue(summary, "profile_name") || t("No profile")],
          [t("Version"), stringValue(summary, "dsh_version") || t("Unknown version")],
          [
            t("Kind"),
            booleanValue(value, "legacy")
              ? t("Legacy metadata only")
              : localizedRuntimeState(stringValue(summary, "kind"), t),
          ],
          [t("Plugins"), numberValue(summary, "plugin_count") ?? t("Not available")],
          [t("Files"), numberValue(summary, "file_count") ?? files.length],
        ].map(([label, text]) => (
          <div key={String(label)}>
            <dt>{label}</dt>
            <dd>{text}</dd>
          </div>
        ))}
      </dl>
      {Object.hasOwn(asObject(value), "valid") && (
        <p role="status" className={booleanValue(value, "valid") ? "field-help" : "form-error"}>
          {t(booleanValue(value, "valid") ? "Integrity check passed" : "Integrity check failed")}
        </p>
      )}
      {errors.map((error, index) => (
        <p className="form-error" key={index}>
          {error}
        </p>
      ))}
      {booleanValue(value, "legacy") && (
        <p className="field-help">{t("Legacy entries restore selection metadata only.")}</p>
      )}
      {source && (
        <details className="snapshot-metadata">
          <summary>{t("Record metadata")}</summary>
          <pre>{JSON.stringify(asObject(value).metadata ?? summary, null, 2)}</pre>
        </details>
      )}
      {!files.length && !booleanValue(value, "legacy") && (
        <p className="field-help">{t("No bounded file content was returned.")}</p>
      )}
      {files.map((item, index) => {
        const filename = stringValue(item, "path") || "";
        const content = stringValue(item, "content");
        const truncated = booleanValue(item, "content_truncated");
        const rows = !source && content && !truncated ? readableSnapshotFields(content) : null;
        const description = filename.endsWith("package.json")
          ? t("Plugins and dependencies")
          : /\.ya?ml$/i.test(filename)
            ? t("Configuration")
            : t("Saved file");
        return (
          <section className="snapshot-readable-file" key={filename || index}>
            <header>
              <strong>{description}</strong>
              <span className="field-help">
                {localizedRuntimeState(stringValue(item, "state"), t)} ·{" "}
                {numberValue(item, "stored_size") ?? 0} B
              </span>
            </header>
            <p className="snapshot-file-path">{filename}</p>
            {arrayValue(item, "redacted_paths").length > 0 && (
              <p className="field-help">
                {t("Redacted fields")}: {arrayValue(item, "redacted_paths").map(String).join(", ")}
              </p>
            )}
            {stringValue(item, "omitted_reason") && (
              <p className="field-help">{t(stringValue(item, "omitted_reason") || "")}</p>
            )}
            {source ? (
              content !== undefined && (
                <pre tabIndex={0} aria-label={filename}>
                  {content}
                </pre>
              )
            ) : rows ? (
              <>
                <dl className="snapshot-values">
                  {rows.map((row, i) => (
                    <div key={i}>
                      <dt>
                        {row.key
                          .split(" / ")
                          .map((key) => (Object.hasOwn(labels, key) ? t(labels[key]) : key))
                          .join(" / ")}
                      </dt>
                      <dd>
                        {row.note === "nested"
                          ? t("Nested content; view source")
                          : row.note === "empty"
                            ? t("Empty")
                            : row.value === null
                              ? t("Not set")
                              : typeof row.value === "boolean"
                                ? t(row.value ? "Yes" : "No")
                                : String(row.value)}
                      </dd>
                    </div>
                  ))}
                </dl>
                {rows.length >= 200 && (
                  <p className="field-help">
                    {t("Showing the first 200 fields. View source for the full returned content.")}
                  </p>
                )}
              </>
            ) : (
              content !== undefined && (
                <p className="field-help">
                  {t(
                    "A structured preview is unavailable. Switch to Source to view the returned content.",
                  )}
                </p>
              )
            )}
            {truncated && (
              <p className="truncation-note">
                {snapshotContentNote(asObject(item).content_note, t)}
              </p>
            )}
          </section>
        );
      })}
    </div>
  );
}
