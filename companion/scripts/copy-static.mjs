// Copies the panel's static files next to the compiled TypeScript, so dist/ is the whole frontend.
import { copyFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
mkdirSync(join(root, 'dist'), { recursive: true });
copyFileSync(join(root, 'index.html'), join(root, 'dist', 'index.html'));
copyFileSync(join(root, 'src', 'styles.css'), join(root, 'dist', 'styles.css'));
