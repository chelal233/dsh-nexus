export type ApiErrorInfo = {
  code: string | null;
  message: string;
  retryable: boolean;
  actions: string[];
};
export function apiErrorInfo(error: unknown): ApiErrorInfo {
  const value = error && typeof error === "object" ? (error as Record<string, unknown>) : {};
  return {
    code: typeof value.code === "string" ? value.code : null,
    message:
      typeof value.message === "string"
        ? value.message
        : typeof error === "string"
          ? error
          : "The native bridge returned an unknown error",
    retryable: value.retryable === true,
    actions: Array.isArray(value.actions)
      ? value.actions.filter((v): v is string => typeof v === "string")
      : [],
  };
}
export function recoverableNoop(error: unknown): boolean {
  const info = apiErrorInfo(error);
  if (info.code)
    return [
      "harness_start_cancelled",
      "harness_operation_busy",
      "harness_already_running",
      "harness_already_stopped",
      "harness_unattached",
      "harness_not_configured",
    ].includes(info.code);
  const legacy = info.message.toLowerCase();
  return [
    "already running",
    "already stopped",
    "not attached",
    "unattached",
    "not configured",
  ].some((fragment) => legacy.includes(fragment));
}
export function errorWithExplanation(original: string, explanation: string, label: string): string {
  return explanation && explanation !== original
    ? explanation + "\n\n" + label + "\n" + original
    : original;
}
