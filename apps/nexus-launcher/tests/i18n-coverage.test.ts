import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync, mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import ts from "typescript";
import { dynamicTranslationKeys } from "./dynamic-i18n-keys.ts";

const root = fileURLToPath(new URL("../src/", import.meta.url));
// Formatting may wrap a call; its file and expression tokens remain the review boundary.
function dynamicCallId(id: string): string {
  const separator = id.indexOf(":");
  const scanner = ts.createScanner(ts.ScriptTarget.Latest, true, ts.LanguageVariant.Standard, id.slice(separator + 1));
  const tokens: string[] = [];
  while (scanner.scan() !== ts.SyntaxKind.EndOfFileToken) tokens.push(scanner.getTokenText());
  return id.slice(0, separator) + ":" + JSON.stringify(tokens);
}
const registeredDynamicCalls = new Map(Object.entries(dynamicTranslationKeys)
  .map(([id, value]) => [dynamicCallId(id), value]));
function parse(file: string) {
  return ts.createSourceFile(file, readFileSync(file, "utf8"), ts.ScriptTarget.Latest, true,
    file.endsWith(".tsx") ? ts.ScriptKind.TSX : ts.ScriptKind.TS);
}
function walk(node: ts.Node, visit: (node: ts.Node) => void) {
  visit(node); ts.forEachChild(node, child => walk(child, visit));
}
function strings(expression: ts.Expression): string[] {
  if (ts.isStringLiteralLike(expression)) return [expression.text];
  if (ts.isParenthesizedExpression(expression)) return strings(expression.expression);
  if (ts.isConditionalExpression(expression)) return [...strings(expression.whenTrue), ...strings(expression.whenFalse)];
  if (ts.isBinaryExpression(expression) &&
      [ts.SyntaxKind.BarBarToken, ts.SyntaxKind.QuestionQuestionToken].includes(expression.operatorToken.kind))
    return [...strings(expression.left), ...strings(expression.right)];
  return [];
}
function sourceFiles(directory: string): string[] {
  return readdirSync(directory, { withFileTypes: true }).flatMap(entry => {
    const file=path.join(directory,entry.name);
    return entry.isDirectory() ? sourceFiles(file) : entry.isFile() && /\.tsx?$/.test(file) ? [file] : [];
  });
}
function staticExpression(expression: ts.Expression): boolean {
  return ts.isStringLiteralLike(expression) || (ts.isParenthesizedExpression(expression) && staticExpression(expression.expression)) ||
    (ts.isConditionalExpression(expression) && staticExpression(expression.whenTrue) && staticExpression(expression.whenFalse));
}
function dynamicCalls(source: ts.SourceFile): string[] {
  const calls: string[]=[];
  walk(source,node=>{if(ts.isCallExpression(node) && ts.isIdentifier(node.expression) && node.expression.text==="t" && node.arguments[0] && !staticExpression(node.arguments[0])) calls.push(node.arguments[0].getText(source));});
  return calls;
}
test("translation tables agree and all static UI translation keys are registered", () => {
  const tables = new Map<string, Set<string>>();
  const tableSource = parse(path.join(root, "i18n.ts"));
  walk(tableSource, node => {
    if (!ts.isVariableDeclaration(node) || !ts.isIdentifier(node.name) ||
        !["english", "chinese"].includes(node.name.text) || !node.initializer ||
        !ts.isObjectLiteralExpression(node.initializer)) return;
    const keys = node.initializer.properties.map(property =>
      property.name && ts.isStringLiteralLike(property.name) ? property.name.text : "");
    assert.ok(keys.every(Boolean), "Translation keys must be explicit string literals");
    assert.equal(new Set(keys).size, keys.length, "Duplicate translation key");
    tables.set(node.name.text, new Set(keys));
  });
  assert.equal(tables.size, 2);
  const english = tables.get("english")!;
  assert.deepEqual([...english].sort(), [...tables.get("chinese")!].sort());
  const missing: string[] = [];
  const dynamicSeen = new Set<string>();
  function scan(directory: string) {
    for (const entry of readdirSync(directory, { withFileTypes: true })) {
      const file = path.join(directory, entry.name);
      if (entry.isDirectory()) { scan(file); continue; }
      if (!entry.isFile() || !/\.tsx?$/.test(entry.name) || entry.name === "i18n.ts") continue;
      walk(parse(file), node => {
        if (!ts.isCallExpression(node) || !ts.isIdentifier(node.expression) ||
            node.expression.text !== "t" || !node.arguments[0]) return;
        for (const key of strings(node.arguments[0])) {
          if (key && !english.has(key)) missing.push(path.relative(root, file) + ": " + key);
        }
        if (!staticExpression(node.arguments[0])) {
          const id=path.relative(root,file).replaceAll("\\","/")+":"+node.arguments[0].getText();
          const registered=registeredDynamicCalls.get(dynamicCallId(id));
          assert.ok(registered?.reason, "Unregistered dynamic translation call: "+id);
          for(const key of registered.keys??[]) assert.ok(english.has(key), "Unregistered dynamic translation key: "+key);
          dynamicSeen.add(dynamicCallId(id));
        }
      });
    }
  }
  scan(root);
  assert.deepEqual(missing, [], "Unregistered translation keys");
  assert.deepEqual([...dynamicSeen].sort(),[...registeredDynamicCalls.keys()].sort(),"Remove stale dynamic key registrations");
});

// Deliberately narrow: product names, units, protocol/argument examples and
// literal paths stay intact. New prose must go through the translator.
const technicalUiLiterals = new Set([
  "npmmirror", "Node", "/ npm", "/ pnpm", "B", "--flag",
  "D:\\Offline\\harness.tar.gz", "D:\\Offline\\harness-export.tar.gz",
  "https://github.com/deepseek-ai/deepseek-harness", "github.com/deepseek-ai/deepseek-harness",
]);
const visibleAttributes = new Set(["title", "placeholder", "aria-label", "aria-description", "alt", "label", "detail", "emptyTitle", "emptyDetail", "kicker"]);

function untranslatedJsx(source: ts.SourceFile): string[] {
  const missing: string[] = [];
  const check = (value: string, node: ts.Node) => {
    const normalized = value.trim();
    if (/\p{L}/u.test(normalized) && !technicalUiLiterals.has(normalized)) {
      missing.push(`${source.getLineAndCharacterOfPosition(node.getStart(source)).line + 1}: ${normalized}`);
    }
  };
  walk(source, node => {
    if (ts.isJsxText(node)) check(node.text, node);
    if (ts.isJsxExpression(node) && node.expression && !ts.isJsxAttribute(node.parent)) {
      for (const value of strings(node.expression)) check(value, node);
    }
    if (ts.isJsxAttribute(node) && visibleAttributes.has(node.name.getText(source)) && node.initializer) {
      if (ts.isStringLiteral(node.initializer)) check(node.initializer.text, node);
      if (ts.isJsxExpression(node.initializer) && node.initializer.expression) {
        for (const value of strings(node.initializer.expression)) check(value, node);
      }
    }
  });
  return missing;
}

test("visible JSX prose and accessibility labels cannot bypass translation", () => {
  const missing = sourceFiles(root).filter(file => file.endsWith(".tsx"))
    .flatMap(file => untranslatedJsx(parse(file)).map(value => `${path.relative(root,file)}:${value}`));
  assert.deepEqual(missing, []);
  const fixture = ts.createSourceFile("fixture.tsx", '<><p>New untranslated prose</p><input placeholder="Choose directory" aria-label={"Directory"}/><span>{ready ? "Ready now" : "Waiting now"}</span></>', ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  assert.equal(untranslatedJsx(fixture).length, 5, "guard catches text, literal/expression attributes and conditional text");
});

test("dynamic variable and interpolated template calls require registration", () => {
  assert.equal(dynamicCallId('view.tsx:stringValue(row, "name") || "Unknown"'), dynamicCallId('view.tsx:stringValue(\nrow, "name"\n) || "Unknown"'));
  assert.notEqual(dynamicCallId('view.tsx:"a b"'), dynamicCallId('view.tsx:"ab"'));
  assert.notEqual(dynamicCallId('one.tsx:key'), dynamicCallId('two.tsx:key'));
  const source=ts.createSourceFile("nested/component.tsx", 't("Known"); t(`Known`); t(statusKey); t(`Harness ${action}`); t(ok ? "Known" : otherKey)',ts.ScriptTarget.Latest,true,ts.ScriptKind.TSX);
  assert.deepEqual(dynamicCalls(source),["statusKey","`Harness ${action}`",'ok ? "Known" : otherKey']);
  assert.equal(dynamicTranslationKeys["nested/component.tsx:statusKey"],undefined);
  const nested=ts.createSourceFile("nested/component.tsx","<section>Nested untranslated text</section>",ts.ScriptTarget.Latest,true,ts.ScriptKind.TSX);
  assert.equal(untranslatedJsx(nested).length,1);
  const directory=mkdtempSync(path.join(tmpdir(),"nexus-i18n-scan-"));
  try {
    mkdirSync(path.join(directory,"nested"));
    const file=path.join(directory,"nested","component.tsx");writeFileSync(file,nested.text);
    assert.deepEqual(sourceFiles(directory),[file]);
    assert.equal(sourceFiles(directory).flatMap(file=>untranslatedJsx(parse(file))).length,1,"nested prose must reach the guard");
  } finally { rmSync(directory,{recursive:true,force:true}); }
});
test("installer languages explicitly offer Simplified Chinese rather than generic Chinese", () => {
  const source=readFileSync(path.join(root,"../electron-builder.cjs"),"utf8");
  assert.match(source, /installerLanguages:.*zh_CN/);
  assert.match(source, /target: \['nsis'\]/);
});
