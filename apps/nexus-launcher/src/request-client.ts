type Body = Record<string, unknown>;
type StoragePort = Pick<Storage, "getItem" | "setItem">;
type Transport = (path: string, method: "GET" | "POST", body?: Body) => Promise<Body>;
const STORAGE_KEY = "nexus.pending-requests.v1";
const MAX_PENDING_REQUESTS = 64;

export function requiresRequestId(path: string, body: Body): boolean {
  return (
    (path === "/v1/harness" && body.action === "restart") ||
    (path === "/v1/releases" && body.action === "rollback") ||
    (path === "/v1/updates" && ["switch", "offline_import"].includes(String(body.action))) ||
    (path === "/v1/checkpoints" && body.action === "restore") ||
    (path === "/v1/profiles" && ["delete", "restore_deleted"].includes(String(body.action)))
  );
}
function stableJson(value: unknown): string {
  if (Array.isArray(value)) {
    return "[" + value.map(stableJson).join(",") + "]";
  }
  if (value && typeof value === "object") {
    const properties = Object.entries(value)
      .sort(([left], [right]) => left.localeCompare(right))
      .map(([key, item]) => JSON.stringify(key) + ":" + stableJson(item));
    return "{" + properties.join(",") + "}";
  }
  return JSON.stringify(value) ?? "null";
}

function hex(bytes: Uint8Array): string {
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
}

export function createRequestClient(
  storage: StoragePort,
  transport: Transport,
  dataRootId: string,
  cryptoPort: Crypto = globalThis.crypto,
  now = () => Date.now(),
) {
  if (!dataRootId || dataRootId.length > 256) {
    throw {
      code: "request_root_unavailable",
      message: "Verify the Nexus data directory before submitting this request.",
    };
  }
  const storageKey = STORAGE_KEY + "." + encodeURIComponent(dataRootId);
  const readPending = (): Record<string, string> => {
    const raw = storage.getItem(storageKey);
    if (!raw) return {};
    const value = JSON.parse(raw);
    if (
      !value ||
      typeof value !== "object" ||
      Array.isArray(value) ||
      Object.keys(value).length > MAX_PENDING_REQUESTS ||
      Object.entries(value).some(
        ([fingerprint, requestId]) =>
          !/^[a-f0-9]{64}$/.test(fingerprint) ||
          typeof requestId !== "string" ||
          !/^\d+-[a-f0-9]{32}$/.test(requestId),
      )
    ) {
      throw {
        code: "request_storage_invalid",
        message: "Pending request references cannot be read safely.",
      };
    }
    return value;
  };
  const savePending = (value: Record<string, string>) =>
    storage.setItem(storageKey, JSON.stringify(value));

  const forget = (id: string) => {
    const entries = readPending();
    for (const key of Object.keys(entries)) {
      if (entries[key] === id) delete entries[key];
    }
    savePending(entries);
  };

  return {
    forget,
    pending: () => Object.values(readPending()),
    async post(path: string, body: Body): Promise<Body> {
      if (!requiresRequestId(path, body)) return transport(path, "POST", body);
      const payload = new TextEncoder().encode(path + "\n" + stableJson(body));
      const digest = await cryptoPort.subtle.digest("SHA-256", payload);
      const key = hex(new Uint8Array(digest));
      const entries = readPending();
      let id = entries[key];
      if (!id) {
        if (Object.keys(entries).length >= MAX_PENDING_REQUESTS) {
          throw {
            code: "request_storage_full",
            message:
              "Too many unresolved requests. Review request history before starting another operation.",
          };
        }
        const issuedAt = Math.floor(now() / 1000);
        const random = hex(cryptoPort.getRandomValues(new Uint8Array(16)));
        id = `${issuedAt}-${random}`;
        entries[key] = id;
        savePending(entries); // Persist before a side effect can be sent.
      }
      try {
        const response = await transport(path, "POST", { ...body, request_id: id });
        const receipt = response.request as Body | undefined;
        if (!receipt || receipt.state === "completed") forget(id);
        return response;
      } catch (error) {
        const code = error && typeof error === "object" ? (error as Body).code : undefined;
        // Unknown transport outcomes and interrupted/running receipts retain identity.
        // Domain failures are proven responses, so the next intentional attempt is new.
        const domainFailure =
          typeof code === "string" &&
          typeof (error as Body).status === "number" &&
          !code.startsWith("request_");
        if (domainFailure || code === "request_previous_failed") forget(id);
        throw error;
      }
    },
  };
}

/** Missing receipts are uncertain outcomes, never proof that an operation did not run. */
export function mergeRequestHistory(server: Body[], pending: string[]): Body[] {
  const known = new Set(server.map((item) => item.request_id));
  const unconfirmed = pending
    .filter((id) => !known.has(id))
    .map((id) => ({
      request_id: id,
      state: "unconfirmed",
      created_at_unix: Number(id.split("-")[0]),
    }));
  return [...server, ...unconfirmed];
}
