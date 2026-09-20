// A separate process hosts the unmodified official source application using
// Nexus's Electron binary. No Nexus windows, IPC handlers, or Agent run here.
import { app } from 'electron';
import fs from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

const recipeFile = app.commandLine.getSwitchValue('nexus-official-desktop');
if (!recipeFile) {
  await import('./main.mjs');
} else {
  try {
    if (!path.isAbsolute(recipeFile)) throw Error('desktop_invalid_source');
    const recipe = JSON.parse(fs.readFileSync(recipeFile, 'utf8'));
    const { desktopCapability } = await import('./harness-desktop.mjs');
    const { desktopSourceView } = await import('./desktop-paths.mjs');
    const { version } = desktopCapability(recipe.source);
    const officialApp = path.join(desktopSourceView(recipe.source), 'apps/desktop');
    const electron = JSON.parse(fs.readFileSync(path.join(officialApp, 'node_modules/electron/package.json'), 'utf8'));
    if (electron.version !== process.versions.electron || !path.isAbsolute(recipe.home) || !path.isAbsolute(recipe.userData)) throw Error('desktop_runtime_incompatible');
    // Match Electron's default_app setup for a source application. In packaged
    // Nexus the executable name would otherwise select Harness's installed-app
    // layout. Keep its existing development layout and update policy unchanged.
    Object.defineProperty(app, 'isPackaged', { value: false });
    app.setAppPath(officialApp);
    app.setVersion(version);
    app.name = '@deepseek-ai/dsh-desktop';
    app.setPath('userData', recipe.userData);
    process.env.DSH_HOME = recipe.home;
    delete process.env.DSH_DESKTOP_HOST_INSPECT_PORT;
    process.env.DSH_DESKTOP_OPEN_DEVTOOLS = '0';
    const { installDesktopStartupAudit, startupEvidenceFile } = await import('./desktop-startup-audit.mjs');
    const { ipcMain } = await import('electron');
    process.env.DSH_DESKTOP_DIAGNOSTIC_FILE = `${startupEvidenceFile(recipe)}.error`;
    installDesktopStartupAudit({ app, ipcMain, recipe });
    await import(pathToFileURL(path.join(officialApp, 'lib/main.js')).href);
  } catch (error) {
    console.error(error);
    app.exit(1);
  }
}
