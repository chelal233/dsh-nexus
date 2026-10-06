export function parseSkippedBundles(text: string): {
  entries: { package: string; reason: string }[];
  truncated: boolean;
};
export function diagnoseStartup(text: string): {
  code: string;
  summary: string;
  remedy: string;
  evidence: string[];
  level: string;
  certainty: string;
  help: string;
};
