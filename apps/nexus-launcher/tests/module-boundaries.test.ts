import assert from "node:assert/strict";
import { readFileSync, readdirSync, existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import ts from "typescript";

test("UI feature modules have explicit, acyclic dependencies on shared code", () => {
  const root = fileURLToPath(new URL("../src/", import.meta.url));
  const files = readdirSync(root, { recursive: true })
    .filter(file => /\.tsx?$/.test(file))
    .map(file => file.replaceAll("\\", "/"));
  const dependencies = new Map<string, string[]>();
  for (const file of files) {
    const source = ts.createSourceFile(file, readFileSync(path.join(root, file), "utf8"), ts.ScriptTarget.Latest, true);
    const imports: string[] = [];
    for (const statement of source.statements) {
      if (!ts.isImportDeclaration(statement) && !ts.isExportDeclaration(statement)) continue;
      const specifier = statement.moduleSpecifier;
      if (!specifier || !ts.isStringLiteral(specifier) || !specifier.text.startsWith(".")) continue;
      const base = path.resolve(root, path.dirname(file), specifier.text);
      const resolved = [base, base + ".ts", base + ".tsx"].find(candidate => existsSync(candidate));
      assert.ok(resolved, `Missing local module: ${file} -> ${specifier.text}`);
      if (!/\.tsx?$/.test(resolved)) continue; // Styles and assets are not code dependencies.
      const target = path.relative(root, resolved).replaceAll("\\", "/");
      if (file !== "main.tsx") assert.notEqual(target, "App.tsx", `${file} must not depend on the application entry`);
      if (file !== "App.tsx" && !file.startsWith("views/")) {
        assert.ok(!target.startsWith("views/"), `Shared module ${file} must not depend on page ${target}`);
      }
      imports.push(target);
    }
    dependencies.set(file, imports);
  }
  const checked = new Set<string>();
  function visit(file: string, stack: string[]) {
    assert.ok(!stack.includes(file), `Module cycle: ${[...stack, file].join(" -> ")}`);
    if (checked.has(file)) return;
    for (const dependency of dependencies.get(file) ?? []) visit(dependency, [...stack, file]);
    checked.add(file);
  }
  for (const file of files) visit(file, []);
});
