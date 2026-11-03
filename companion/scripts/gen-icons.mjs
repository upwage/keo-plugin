// Draws the 1024x1024 Keo Companion app icon (src-tauri/icons/source.png) as plain PNG bytes, with
// no image library. The tray icons are not drawn here: src-tauri/icons/tray/build.py writes them.
// Run it again after a change to the shape; the output is committed. To refresh the bundle icons,
// run `npx tauri icon src-tauri/icons/source.png -o <temp folder>` and copy 32x32.png,
// 128x128.png, icon.png, icon.icns and icon.ico from it into src-tauri/icons/.
import { Buffer } from 'node:buffer';
import { writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { deflateSync } from 'node:zlib';

const here = dirname(fileURLToPath(import.meta.url));
const iconsDir = join(here, '..', 'src-tauri', 'icons');

const CRC_TABLE = Array.from({ length: 256 }, (_, n) => {
  let c = n;
  for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
  return c >>> 0;
});
const crc32 = (buf) => {
  let c = 0xffffffff;
  for (const b of buf) c = CRC_TABLE[(c ^ b) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
};
const chunk = (type, data) => {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type, 'ascii'), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body));
  return Buffer.concat([len, body, crc]);
};

/** RGBA pixels from a coverage function f(x, y) -> 0..1, sampled 4x4 per pixel for smooth edges. */
function encode(size, shade, color) {
  const raw = Buffer.alloc(size * (size * 4 + 1));
  for (let y = 0; y < size; y++) {
    raw[y * (size * 4 + 1)] = 0;
    for (let x = 0; x < size; x++) {
      let cover = 0;
      for (let sy = 0; sy < 4; sy++)
        for (let sx = 0; sx < 4; sx++) cover += shade((x + (sx + 0.5) / 4) / size, (y + (sy + 0.5) / 4) / size);
      const a = Math.round((cover / 16) * 255);
      const o = y * (size * 4 + 1) + 1 + x * 4;
      const [r, g, b] = color(x / size, y / size);
      raw[o] = r;
      raw[o + 1] = g;
      raw[o + 2] = b;
      raw[o + 3] = a;
    }
  }
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // RGBA
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk('IHDR', ihdr),
    chunk('IDAT', deflateSync(raw)),
    chunk('IEND', Buffer.alloc(0)),
  ]);
}

// Shapes in unit coordinates (0..1), each returns 1 inside and 0 outside.
const dist = (x, y, cx, cy) => Math.hypot(x - cx, y - cy);
const line = (x0, y0, x1, y1, w) => (x, y) => {
  const dx = x1 - x0;
  const dy = y1 - y0;
  const t = Math.max(0, Math.min(1, ((x - x0) * dx + (y - y0) * dy) / (dx * dx + dy * dy)));
  return dist(x, y, x0 + t * dx, y0 + t * dy) <= w / 2 ? 1 : 0;
};
const union = (...fs) => (x, y) => Math.max(...fs.map((f) => f(x, y)));

// The "K" on the app icon.
const k = union(line(0.38, 0.3, 0.38, 0.7, 0.09), line(0.4, 0.5, 0.62, 0.3, 0.09), line(0.45, 0.46, 0.63, 0.7, 0.09));

// The app icon: a green rounded square with a white K.
const rounded = (x, y) => {
  const r = 0.2;
  const cx = Math.min(Math.max(x, 0.06 + r), 0.94 - r);
  const cy = Math.min(Math.max(y, 0.06 + r), 0.94 - r);
  return dist(x, y, cx, cy) <= r ? 1 : 0;
};
const appShape = rounded;
const appColor = (x, y) => (k(x, y) ? [255, 255, 255] : [22, 128, 82]);
writeFileSync(join(iconsDir, 'source.png'), encode(1024, appShape, appColor));
console.log('icons written to', iconsDir);
