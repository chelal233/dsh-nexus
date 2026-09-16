import { mkdirSync, mkdtempSync, openSync, writeFileSync, fsyncSync, closeSync, renameSync, unlinkSync, rmdirSync } from 'node:fs';
import path from 'node:path';

export const UPDATE_CHECK_INTERVAL_MS = 2 * 60 * 60 * 1000;

// Never truncate the last accepted setting. A killed writer can leave only
// an unpublished private temporary directory, ignored by the next launch.
export function saveUpdateSettings(settingsPath, value) {
  const directory = path.dirname(settingsPath);
  mkdirSync(directory, { recursive: true });
  const temporaryDirectory = mkdtempSync(path.join(directory, '.desktop-update-'));
  const temporary = path.join(temporaryDirectory, 'settings.json');
  try {
    const file = openSync(temporary, 'wx', 0o600);
    try { writeFileSync(file, JSON.stringify(value)); fsyncSync(file); }
    finally { closeSync(file); }
    renameSync(temporary, settingsPath);
  } finally {
    try { unlinkSync(temporary); } catch (error) { if (error.code !== 'ENOENT') throw error; }
    rmdirSync(temporaryDirectory);
  }
}

// The updater owns download/installation; Agent owns the decision to become idle.
export class DesktopUpdater {
  constructor(updater, { coordinate, cancel = async () => {}, changed, settings, save, packaged }) {
    this.updater = updater;
    this.coordinate = coordinate;
    this.cancel = cancel;
    this.changed = changed;
    this.save = save;
    this.packaged = packaged;
    this.state = { enabled: settings.enabled !== false, phase: 'idle' };
    // Downloads are coordinated here so manual checks work with automation off.
    updater.autoDownload = false;
    updater.autoInstallOnAppQuit = false;
    updater.disableDifferentialDownload = true;
    updater.allowDowngrade = false;
    // This repository currently distributes prereleases. The architecture
    // channel is selected by the packaged app-update.yml, never renderer input.
    updater.allowPrerelease = true;
    updater.on('checking-for-update', () => this.publish({ phase: 'checking', error: undefined }));
    updater.on('update-available', info => this.publish({ phase: 'available', version: info.version }));
    updater.on('update-not-available', () => this.publish({ phase: 'idle' }));
    updater.on('download-progress', progress => this.publish({ phase: 'downloading', percent: progress.percent }));
    updater.on('update-downloaded', info => this.publish({ phase: 'ready', version: info.version }));
    updater.on('error', error => {
      if (this.state.phase === 'installing') void this.recoverInstall(error);
      else this.publish({ phase: 'error', error: error.message });
    });
  }
  publish(patch) { Object.assign(this.state, patch); this.changed({ ...this.state }); }
  setEnabled(enabled) {
    if (typeof enabled !== 'boolean') throw new Error('Invalid update setting');
    this.save({ enabled }); this.publish({ enabled });
    return { ...this.state };
  }
  async check({ manual = false } = {}) {
    if (!this.packaged) throw new Error('Update checks require an installed release');
    if (['checking', 'downloading', 'installing', 'ready'].includes(this.state.phase)) return this.state;
    if (!manual && !this.state.enabled) return this.state;
    this.publish({ phase: 'checking', error: undefined, percent: undefined });
    try {
      await this.updater.checkForUpdates();
      if (this.state.phase === 'available' && (manual || this.state.enabled)) {
        this.publish({ phase: 'downloading', percent: 0 });
        await this.updater.downloadUpdate();
      }
      return this.state;
    } catch (error) {
      this.publish({ phase: 'error', error: error.message });
      throw error;
    }
  }
  async install() {
    if (this.state.phase !== 'ready') throw new Error('No verified update is ready');
    this.publish({ phase: 'installing', error: undefined });
    try {
      await this.coordinate();
      this.updater.quitAndInstall(true, true);
    } catch (error) {
      await this.recoverInstall(error);
      throw error;
    }
  }
  recoverInstall(error) {
    if (!this.recovery) {
      this.recovery = Promise.resolve().then(() => this.cancel()).then(
        () => this.publish({ phase: 'ready', error: error.message }),
        failure => this.publish({ phase: 'error', error: `${error.message}; ${failure.message}` }),
      ).finally(() => { this.recovery = undefined; });
    }
    return this.recovery;
  }
  start() {
    if (this.timer) return;
    const tick = () => {
      if (!this.state.enabled) return;
      void this.check().catch(() => {});
    };
    this.timer = setInterval(tick, UPDATE_CHECK_INTERVAL_MS);
    this.timer.unref();
    tick();
  }
  stop() { clearInterval(this.timer); this.timer = undefined; }
}
