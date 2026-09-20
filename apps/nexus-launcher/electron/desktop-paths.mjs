import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { createHash } from 'node:crypto';

// Native Python extensions can still encounter MAX_PATH under a deeply nested
// imported release. A short junction changes only execution paths, not the data.
export function desktopSourceView(source) {
  const root = fs.realpathSync(source);
  if (process.platform !== 'win32') return root;
  const parent = path.join(os.tmpdir(), 'nexus-desktop');
  fs.mkdirSync(parent, { recursive: true });
  const id = createHash('sha256').update(root.toLowerCase()).digest('hex').slice(0, 16);
  const view = path.join(parent, id);
  try { fs.symlinkSync(root, view, 'junction'); }
  catch (error) { if (error.code !== 'EEXIST') throw error; }
  if (fs.realpathSync(view).toLowerCase() !== root.toLowerCase()) throw new Error('desktop_invalid_source');
  return view;
}
