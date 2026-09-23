declare module "js-yaml" {
  export const JSON_SCHEMA: unknown;
  export function load(text: string, options?: { schema?: unknown }): unknown;
}
