import { type JsonObject } from "./app-types";

export function isObject(value: unknown): value is JsonObject {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export function asObject(value: unknown): JsonObject {
  return isObject(value) ? value : {};
}

export function stringValue(value: unknown, key: string): string | undefined {
  const item = asObject(value)[key];
  if (typeof item === "string" && item.trim()) return item;
  if (typeof item === "number" || typeof item === "boolean") return String(item);
  return undefined;
}

export function numberValue(value: unknown, key: string): number | undefined {
  const item = asObject(value)[key];
  return typeof item === "number" && Number.isFinite(item) ? item : undefined;
}

export function arrayValue(value: unknown, key: string): unknown[] {
  const item = asObject(value)[key];
  return Array.isArray(item) ? item : [];
}

export function nestedValue(value: unknown, key: string): JsonObject {
  return asObject(asObject(value)[key]);
}

export function harnessRuntimeValue(value: unknown): JsonObject {
  const response = asObject(value);
  const nested = asObject(response.harness);
  return Object.keys(nested).length ? nested : response;
}

export function booleanValue(value: unknown, key: string): boolean {
  return asObject(value)[key] === true;
}
