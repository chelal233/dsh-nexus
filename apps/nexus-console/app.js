const DEFAULT_API = "http://127.0.0.1:3090";
const apiInput = document.querySelector("#api-base");
const activity = document.querySelector("#activity-log");
let launcherAvailable = false;

const savedApi = localStorage.getItem("nexus.console.api") || DEFAULT_API;
apiInput.value = savedApi;

function apiBase() {
  const value = apiInput.value.trim().replace(/\/$/, "");
  if (!value) throw new Error("Agent API 不能为空");
  const url = new URL(value);
  if (url.protocol !== "http:") {
    throw new Error("Console 只允许使用本地 HTTP Agent");
  }
  if (!["127.0.0.1", "localhost", "[::1]"].includes(url.hostname)) {
    throw new Error("Console 只允许连接 loopback Agent");
  }
  if (url.username || url.password || url.pathname !== "/" || url.search || url.hash) {
    throw new Error("Agent API 只能是 loopback 根地址");
  }
  localStorage.setItem("nexus.console.api", value);
  return value;
}

function log(message, error = false) {
  const item = document.createElement("li");
  item.textContent = `${new Date().toLocaleTimeString()}  ${message}`;
  if (error) item.style.color = "#ffaaa3";
  activity.prepend(item);
  while (activity.children.length > 30) activity.lastElementChild.remove();
}

async function request(path, options = {}) {
  const init = { ...options, headers: { ...(options.headers || {}) } };
  if (options.body && typeof options.body !== "string") {
    init.headers["content-type"] = "application/json";
    init.body = JSON.stringify(options.body);
  }
  const response = await fetch(`${apiBase()}${path}`, init);
  const text = await response.text();
  let body = null;
  if (text) {
    try { body = JSON.parse(text); } catch { body = text; }
  }
  if (!response.ok) {
    const message = body?.message || `${response.status} ${response.statusText}`;
    throw new Error(message);
  }
  return body;
}

async function launcherRequest(path, options = {}) {
  const init = { ...options, headers: { ...(options.headers || {}) } };
  if (options.body && typeof options.body !== "string") {
    init.headers["content-type"] = "application/json";
    init.body = JSON.stringify(options.body);
  }
  const response = await fetch(`${window.location.origin}${path}`, init);
  const text = await response.text();
  let body = null;
  if (text) {
    try { body = JSON.parse(text); } catch { body = text; }
  }
  if (!response.ok) {
    const message = body?.message || `${response.status} ${response.statusText}`;
    throw new Error(message);
  }
  return body;
}

function setText(selector, value) {
  const node = document.querySelector(selector);
  if (node) node.textContent = value == null || value === "" ? "—" : String(value);
}

function setPill(selector, value, tone = "muted") {
  const node = document.querySelector(selector);
  if (!node) return;
  node.textContent = value || "—";
  node.className = `pill ${tone}`;
}

function stateTone(value) {
  const good = ["running", "ok", "stopped", "succeeded", "completed"];
  const bad = ["failed", "error", "shutting_down"];
  const lower = String(value || "").toLowerCase();
  return good.includes(lower) ? "good" : bad.includes(lower) ? "bad" : "muted";
}

function renderList(selector, values, empty = "暂无记录", render = value => value) {
  const list = document.querySelector(selector);
  list.replaceChildren();
  if (!values?.length) {
    const item = document.createElement("li");
    item.className = "empty";
    item.textContent = empty;
    list.append(item);
    return;
  }
  values.forEach(value => {
    const item = document.createElement("li");
    const rendered = render(value);
    if (typeof rendered === "string") item.textContent = rendered;
    else item.append(rendered);
    list.append(item);
  });
}

function formatUnix(value) {
  if (!value) return "—";
  return new Date(Number(value) * 1000).toLocaleString();
}

async function refreshLauncher() {
  try {
    const status = await launcherRequest("/launcher/status");
    launcherAvailable = true;
    const label = status.running
      ? "Launcher · Agent 运行"
      : status.desired_agent_running
        ? "Launcher · Agent 启动中"
        : "Launcher · Agent 已停止";
    setPill("#launcher-state", label, status.running ? "good" : "muted");
    if (status.agent_api && apiInput.value === DEFAULT_API) {
      apiInput.value = status.agent_api;
    }
    return status;
  } catch (error) {
    const wasAvailable = launcherAvailable;
    launcherAvailable = false;
    setPill("#launcher-state", "静态预览", "muted");
    if (wasAvailable) log(`Launcher 主机不可用：${error.message}`, true);
    return null;
  }
}

async function refresh() {
  await refreshLauncher();
  setPill("#connection-state", "连接中", "muted");
  const results = await Promise.allSettled([
    request("/v1/health"),
    request("/v1/state"),
    request("/v1/harness"),
    request("/v1/profiles"),
    request("/v1/releases"),
    request("/v1/updates"),
    request("/v1/checkpoints"),
    request("/v1/config"),
    request("/v1/diagnostics"),
  ]);
  const values = results.map(result => result.status === "fulfilled" ? result.value : null);
  const [health, state, harness, profiles, releases, updates, checkpoints, config, diagnostics] = values;
  const firstError = results.find(result => result.status === "rejected");
  if (firstError) {
    setPill("#connection-state", "不可用", "bad");
    log(firstError.reason?.message || "Agent 请求失败", true);
    return;
  }
  setPill("#connection-state", health.status || "ok", stateTone(health.status));
  setText("#agent-api-version", health.api_version);
  setPill("#agent-lifecycle", state.state.lifecycle, stateTone(state.state.lifecycle));
  setText("#agent-harness", state.state.harness);
  setText("#agent-profile", state.state.profile);
  setText("#agent-release", state.state.release);
  setPill("#harness-state", harness.harness.state, stateTone(harness.harness.state));
  setText("#harness-pid", harness.harness.pid);
  setText("#harness-exit", harness.harness.exit_code);
  setText("#harness-error", harness.harness.error);

  setText("#profile-active", `当前：${profiles.active_profile}`);
  setText("#profile-count", profiles.profiles?.length || 0);
  renderList("#profile-list", profiles.profiles, "暂无 profile", name => name === profiles.active_profile ? `${name}  · active` : name);

  const releaseValues = releases.releases || [];
  renderList("#release-list", releaseValues, "暂无 release slot", release => {
    const marker = [releases.current_release === release.id ? "current" : "", releases.last_known_good === release.id ? "last-good" : ""].filter(Boolean).join(" · ");
    return `${release.id}  ·  ${release.version}${marker ? `  ·  ${marker}` : ""}`;
  });
  setPill("#update-state", updates.update.state, stateTone(updates.update.state));
  setText("#update-release", updates.update.release_id);
  setText("#update-version", updates.release?.version);
  setText("#update-error", updates.update.error);

  const checkpointValues = checkpoints.checkpoints || [];
  setText("#checkpoint-count", checkpointValues.length);
  renderList("#checkpoint-list", checkpointValues, "暂无记录点", checkpoint => `${checkpoint.id}  ·  ${checkpoint.profile}  ·  ${formatUnix(checkpoint.created_at_unix)}`);

  setText("#config-harness", config.harness ? `${config.harness.program}  ·  ${config.harness.args?.length || 0} args` : "未配置");
  setText("#config-update", config.update ? `${config.update.source}  ·  ${config.update.ref_name}` : "未配置");

  const diagnosticValues = diagnostics.bundles || [];
  renderList("#diagnostics-list", diagnosticValues, "暂无诊断包", bundle => `${bundle.id}  ·  ${bundle.files.length} files  ·  ${formatUnix(bundle.created_at_unix)}`);
  log("Agent 状态已刷新");
}

async function act(label, path, body) {
  try {
    await request(path, { method: "POST", body });
    log(`${label}：完成`);
    await refresh();
  } catch (error) {
    log(`${label}：${error.message}`, true);
  }
}

async function launcherAct(label, action) {
  if (!launcherAvailable) {
    log(`${label}：当前是静态预览，请使用 nexus-launcher console 启动宿主`, true);
    return;
  }
  try {
    await launcherRequest("/launcher/agent", { method: "POST", body: { action } });
    log(`${label}：完成`);
    await refresh();
  } catch (error) {
    log(`${label}：${error.message}`, true);
  }
}

document.addEventListener("click", event => {
  const action = event.target.closest("[data-action]")?.dataset.action;
  if (!action) return;
  if (action === "refresh") refresh();
  if (action.startsWith("agent-")) launcherAct(`Agent ${action.slice(6)}`, action.slice(6));
  if (action.startsWith("harness-")) act(`Harness ${action.slice(8)}`, "/v1/harness", { action: action.slice(8) });
  if (action === "profile-select") {
    const profile = document.querySelector("#profile-name").value.trim();
    if (profile) act("Profile 切换", "/v1/profiles", { action: "select", profile });
    else log("请输入 profile 名称", true);
  }
  if (action === "create-checkpoint") act("创建记录点", "/v1/checkpoints", { action: "create", note: "Console checkpoint" });
  if (action === "collect-diagnostics") act("收集诊断", "/v1/diagnostics", { action: "collect", note: "Console collection" });
  if (action === "release-promote") {
    const id = document.querySelector("#release-id").value.trim();
    if (id) act("提升 release", "/v1/releases", { action: "promote", id });
    else log("请输入 release slot id", true);
  }
  if (action === "release-rollback") act("release 回滚", "/v1/releases", { action: "rollback" });
  if (action === "update-install") {
    const id = document.querySelector("#update-id").value.trim();
    const version = document.querySelector("#update-version-input").value.trim();
    act("执行更新", "/v1/updates", { action: "install", release_id: id || null, version: version || null });
  }
  if (action === "clear-activity") activity.replaceChildren();
});

apiInput.addEventListener("change", () => refresh());
refresh();
