import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import path from "node:path";

// The Windows build already requires Visual Studio. Inspect actual PE imports,
// including delay imports, rather than trusting compiler flags alone.
export function verifyStaticRuntime(binaryPaths) {
  if (process.platform !== "win32") return;
  const vswhere = path.join(process.env["ProgramFiles(x86)"] || "C:/Program Files (x86)", "Microsoft Visual Studio/Installer/vswhere.exe");
  const installation = execFileSync(vswhere, ["-latest", "-products", "*", "-requires", "Microsoft.VisualStudio.Component.VC.Tools.x86.x64", "-property", "installationPath"], { encoding: "utf8", windowsHide: true }).trim();
  if (!installation) throw new Error("Visual Studio C++ tools were not found for import verification");
  const version = readFileSync(path.join(installation, "VC/Auxiliary/Build/Microsoft.VCToolsVersion.default.txt"), "utf8").trim();
  const dumpbin = path.join(installation, "VC/Tools/MSVC", version, "bin/Hostx64/x64/dumpbin.exe");
  // shell32 supplies CommandLineToArgvW; user32 supplies the native uninstall
  // data-choice dialog. Both are Windows APIs used by the compiled helper.
  const systemImports = new Set(["kernel32.dll", "ntdll.dll", "ws2_32.dll", "bcrypt.dll", "bcryptprimitives.dll", "advapi32.dll", "userenv.dll", "winhttp.dll", "rpcrt4.dll", "ole32.dll", "crypt32.dll", "shell32.dll", "user32.dll"]);
  for (const binary of binaryPaths) {
    const output = execFileSync(dumpbin, ["/DEPENDENTS", binary], { encoding: "utf8", windowsHide: true });
    const imports = [...new Set([...output.matchAll(/^\s+([\w.-]+\.dll)\s*$/gmi)].map(match => match[1].toLowerCase()))];
    if (!imports.length) throw new Error(`No PE imports could be verified for ${binary}`);
    const unexpected = imports.filter(name => !systemImports.has(name) && !name.startsWith("api-ms-win-core-"));
    if (unexpected.length) throw new Error(`${binary} imports non-approved DLLs: ${unexpected.join(", ")}`);
    console.log(`[static-runtime] ${path.basename(binary)}: Windows system imports only (${imports.join(", ")})`);
  }
}
