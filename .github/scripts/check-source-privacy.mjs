import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

// A source hygiene gate, not a replacement for a full credential scanner.
export function inspectFile(name, text) {
  const findings = [];
  if (/(^|\/)\.agent-memory(\/|$)/.test(name)) findings.push({ line: 0, rule: 'internal-memory' });
  if (/(^|\/)(?:\.env(?:\.(?!example$)[^/]+)?|id_rsa|id_ed25519|[^/]+\.(?:pfx|p12|kdbx))$/i.test(name)) {
    findings.push({ line: 0, rule: 'private-file' });
  }
  if (text.includes('\0')) return findings;
  text.split(/\r?\n/).forEach((line, index) => {
    if (name.endsWith('.md') && /(?<![\w])[A-Za-z]:[\\/](?![\\/])/.test(line)) {
      findings.push({ line: index + 1, rule: 'absolute-document-path' });
    }
    for (const match of line.matchAll(/(?:[A-Za-z]:[\\/]+Users[\\/]+|(?<![\w/:])\/(?:home|Users)\/)([\w.-]+)/g)) {
      if (!/^(?:fixture|test|user|example|public|default)$/i.test(match[1])) {
        findings.push({ line: index + 1, rule: 'personal-home-path' });
      }
    }
  });
  return findings;
}

export function checkRepository(root) {
  const files = execFileSync('git', ['ls-files', '-z'], { cwd: root, encoding: 'utf8' }).split('\0').filter(Boolean);
  let failures = 0;
  for (const name of files) {
    const findings = inspectFile(name, readFileSync(path.join(root, name), 'utf8'));
    for (const finding of findings) {
      // Never echo the matched contents: CI logs are public too.
      console.error(`${name}:${finding.line}: ${finding.rule}`);
      failures++;
    }
  }
  if (failures) throw new Error(`Source privacy check failed: ${failures} findings`);
  console.log(`Source privacy check passed (${files.length} tracked files)`);
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  checkRepository(path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..'));
}
