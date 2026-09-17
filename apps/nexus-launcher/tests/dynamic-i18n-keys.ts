// Exact call expressions are an explicit review boundary: new dynamic call
// sites must declare their key domain or why runtime text is passed through.
export const dynamicTranslationKeys: Record<string, { keys?: string[]; reason: string }> = {
  "App.tsx:`Harness ${action}`": {
    keys: ["Harness start", "Harness stop", "Harness restart"],
    reason: "Harness lifecycle actions",
  },
  "views/workbench.tsx:`Harness ${action}`": {
    keys: ["Harness start", "Harness stop", "Harness restart"],
    reason: "Harness lifecycle actions",
  },
  "views/updates.tsx:copyState": {
    keys: ["Path copied", "Select the path and copy it manually."],
    reason: "Clipboard result state",
  },
  "App.tsx:noticeKey": {
    keys: [
      "complete",
      "Harness startup requested. Check its status and Web entry to confirm readiness.",
      "Offline package request accepted. Follow the current stage to confirm completion.",
      "Installation request accepted. Follow the current stage below to confirm completion.",
      "Cancellation requested. Waiting for cleanup to finish.",
      "The original request is still running. Check its progress before retrying.",
      "The original request was accepted. Check the operation for its final result.",
      "The original request already completed; it was not run again.",
    ],
    reason: "actionNoticeKey and receipt result labels",
  },
  "display-format.ts:suffix": {
    reason: "Known diagnostic suffixes selected from the literal suffix table",
  },
  "display-format.ts:suffix.slice(2)": {
    reason: "Known diagnostic suffixes without the punctuation prefix",
  },
  "display-format.ts:reason": {
    reason: "Runtime diagnostic reasons can contain paths and unknown upstream text",
  },
  "App.tsx:item.title": { reason: "operationSummaries title table" },
  "operation-notices.tsx:item.title": { reason: "operationSummaries title table" },
  "App.tsx:item.status": { reason: "operationSummaries status table" },
  "operation-notices.tsx:item.status": { reason: "operationSummaries status table" },
  "App.tsx:label": { reason: "Shared controls consume labels from local UI definition tables" },
  "ui-components.tsx:label": {
    reason: "Shared controls consume labels from local UI definition tables",
  },
  "views/startup.tsx:item.label": {
    keys: ["Prepare", "Install Harness", "Check and start"],
    reason: "Guide step definition table",
  },
  "views/startup.tsx:item.detail": {
    keys: [
      "Choose the install method",
      "Install a version or select a directory",
      "Run the startup check and start Harness",
    ],
    reason: "Guide step definition table",
  },
  "views/recovery.tsx:label": {
    reason: "Shared controls consume labels from local UI definition tables",
  },
  "views/settings.tsx:label": {
    reason: "Shared controls consume labels from local UI definition tables",
  },
  'App.tsx:modules.find((item) => item.id === activeModule)?.label || "Overview"': {
    reason: "Module navigation definition table",
  },
  "views/settings.tsx:scope": { reason: "Harness argument reference scope table" },
  "views/settings.tsx:description": { reason: "Harness argument reference description table" },
  "views/maintenance.tsx:String(area.kind)": {
    reason: "Agent storage-area kinds; unknown kinds remain readable",
  },
  'views/maintenance.tsx:String(item.reason || "Can be removed")': {
    reason: "Agent cleanup reason, possibly unknown or path-bearing",
  },
  'views/startup.tsx:stringValue(item, "reason") || ""': {
    reason: "Agent compatibility and cleanup explanations",
  },
  'views/startup.tsx:stringValue(entry, "status") || "Unknown"': {
    reason: "Agent startup-check status",
  },
  'views/startup.tsx:stringValue(entry, "id") || "Check"': {
    reason: "Agent startup-check identifier",
  },
  'views/startup.tsx:stringValue(entry, "next") || ""': {
    reason: "Agent startup-check next action, possibly upstream text",
  },
  "operation-notices.tsx:line": {
    reason: "Operation errors can include unregistered upstream messages and paths",
  },
  'views/profiles.tsx:stringValue(item, "omitted_reason") || ""': {
    reason: "Package component omission reason from Agent",
  },
  'views/updates.tsx:stages[stage || ""] || "Preparing package"': {
    reason: "Offline stage label table; unknown stage uses a registered fallback",
  },
  'views/recovery.tsx:stringValue(item,"kind")||"Unknown"': {
    reason: "Request receipt kind from Agent",
  },
  'views/recovery.tsx:stringValue(item,"reason")||"Unreadable or unsupported diagnostic record was preserved"':
    { reason: "Preserved diagnostic reason from Agent" },
  'views/settings.tsx:stringValue(row, "name") || "Unknown"': {
    reason: "Runtime tool name from Agent",
  },
  'views/settings.tsx:stringValue(row, "source") || "Unknown"': {
    reason: "Runtime tool source from Agent",
  },
  "views/settings.tsx:help": { reason: "Preference field help from local definition tables" },
  "views/settings.tsx:error": {
    reason: "Preference validation error, including dynamic backend text",
  },
};
