export type JsonObject = Record<string, unknown>;

// Mirrors HealthResponse in nexus-protocol, including read-only recovery metadata.
export type AgentHealthResponse = {
  api_version: string;
  auth_version: number;
  service: string;
  status: "ok" | "shutting_down";
  data_root_id: string;
  instance_id: string;
  binary_path?: string;
  build_id?: string;
  harness_config_wire_version: number;
  degraded?: boolean;
  read_only?: boolean;
  recovery_reason?: string;
  data_root?: string;
};

export type AgentRequestMethod = "GET" | "POST";

export type HarnessState = "detached" | "starting" | "running" | "stopped" | "failed";

export type AgentStateResponse = {
  api_version: string;
  state: {
    lifecycle: "starting" | "running" | "shutting_down";
    harness: HarnessState;
    profile: string | null;
    release: string | null;
    started_at_unix: number;
    updated_at_unix: number;
  };
};

export type HarnessRuntimeInfo = {
  state: HarnessState;
  pid?: number;
  exit_code?: number;
  error?: string;
  started_at_unix?: number;
  updated_at_unix?: number;
};

export type HarnessResponse = {
  api_version: string;
  harness: HarnessRuntimeInfo;
  generation?: number;
  log_session_run_id?: string;
  log_session_generation?: number;
  log_stdout_watermark?: number;
  log_stderr_watermark?: number;
  log_stdout_file_identity?: string;
  log_stderr_file_identity?: string;
  log_stdout_name?: string;
  log_stderr_name?: string;
  log_session_launch_pending?: boolean;
};

export type StartupStatus = {
  available: boolean;
  running: boolean;
  api_base?: string;
  agent_program?: string;
  agent_pid?: number;
  data_root?: string;
  data_root_id?: string;
  instance_id?: string;
  message?: string;
  harness_startup_error?: string;
};

export type Snapshot = {
  startup: StartupStatus | null;
  endpointErrors: Record<string, string>;
  lifecycleBusy?: boolean;
  status: StartupStatus | null;
  health: AgentHealthResponse | null;
  state: AgentStateResponse | null;
  harnessRuntime: HarnessResponse | null;
  harnessUi: JsonObject | null;
  profiles: JsonObject | null;
  checkpoints: JsonObject | null;
  releases: JsonObject | null;
  updates: JsonObject | null;
  diagnostics: JsonObject | null;
  maintenance?: JsonObject | null;
  recovery: JsonObject | null;
  config: JsonObject | null;
};

export type ModuleId = "workbench" | "guide" | "versions" | "profiles" | "maintenance" | "settings";

export type ThemeMode = "system" | "light" | "dark";

export type RuntimeToolSource = "system" | "nexus" | "bundled";

export type ViewProps = {
  snapshot: Snapshot;
  busyAction: string | null;
  actionPending?: boolean;
  credentialInvalidationPending: boolean;
  runAction: (label: string, path: string, body: JsonObject) => Promise<void | boolean>;
  refresh: () => Promise<void>;
  themeMode: ThemeMode;
  setThemeMode: (mode: ThemeMode) => void;
  openSettings?: () => void;
  openWorkbench?: () => void;
  onRepair?: (id: string) => void;
  recheckEpoch?: number;
  repairSection?: { section: string; id: number };
  embedded?: boolean;
  autoLoadTags?: boolean;
};

export type HarnessPanelProps = Pick<
  ViewProps,
  "snapshot" | "busyAction" | "runAction" | "actionPending"
>;

export type HarnessAuthPanelProps = HarnessPanelProps &
  Pick<ViewProps, "credentialInvalidationPending">;

export type HarnessWebPanelProps = Pick<
  ViewProps,
  "snapshot" | "credentialInvalidationPending" | "busyAction" | "runAction"
>;
