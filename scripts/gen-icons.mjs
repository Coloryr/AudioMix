// 生成应用图标：icon.ico / 32x32.png / 128x128.png / 128x128@2x.png
// 纯 node 实现（zlib + 手写 PNG/ICO 编码），无需外部依赖。
import zlib from "node:zlib";
import fs from "node:fs";
import path from "node:path";

const OUT = new URL("../app/src-tauri/icons/", import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, "$1");

// ---------- PNG ----------
function crc32(buf) {
  let table = crc32.table;
  if (!table) {
    table = crc32.table = new Int32Array(256);
    for (let n = 0; n < 256; n++) {
      let c = n;
      for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
      table[n] = c;
    }
  }
  let c = -1;
  for (let i = 0; i < buf.length; i++) c = (c >>> 8) ^ table[(c ^ buf[i]) & 0xff];
  return (c ^ -1) >>> 0;
}

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body));
  return Buffer.concat([len, body, crc]);
}

function encodePng(width, height, rgba) {
  const sig = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // RGBA
  const raw = Buffer.alloc(height * (1 + width * 4));
  for (let y = 0; y < height; y++) {
    raw[y * (1 + width * 4)] = 0; // filter none
    rgba.copy(raw, y * (1 + width * 4) + 1, y * width * 4, (y + 1) * width * 4);
  }
  const idat = zlib.deflateSync(raw, { level: 9 });
  return Buffer.concat([sig, chunk("IHDR", ihdr), chunk("IDAT", idat), chunk("IEND", Buffer.alloc(0))]);
}

// ---------- 图案：深蓝圆角底 + 青色声波柱 ----------
function drawIcon(size) {
  const rgba = Buffer.alloc(size * size * 4);
  const r = size * 0.18; // 圆角半径
  const bars = [0.32, 0.62, 0.44, 0.8, 0.52]; // 声波柱高度
  const barW = size * 0.09;
  const gap = size * 0.055;
  const totalW = bars.length * barW + (bars.length - 1) * gap;
  const startX = (size - totalW) / 2;
  const centerY = size / 2;

  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      const i = (y * size + x) * 4;
      // 圆角矩形
      const dx = Math.max(r - x, x - (size - 1 - r), 0);
      const dy = Math.max(r - y, y - (size - 1 - r), 0);
      const inside = dx * dx + dy * dy <= r * r;
      if (!inside) continue;
      // 渐变底
      let cr = 30 + Math.floor((y / size) * 20);
      let cg = 58 + Math.floor((y / size) * 30);
      let cb = 138 + Math.floor((y / size) * 40);
      // 声波柱（青色，带垂直渐变）
      for (let b = 0; b < bars.length; b++) {
        const bx = startX + b * (barW + gap);
        const half = (bars[b] * size) / 2;
        if (x >= bx && x < bx + barW && Math.abs(y - centerY) <= half) {
          const t = 1 - Math.abs(y - centerY) / half;
          cr = 90 + Math.floor(t * 60);
          cg = 220;
          cb = 230;
          break;
        }
      }
      rgba[i] = cr;
      rgba[i + 1] = cg;
      rgba[i + 2] = cb;
      rgba[i + 3] = 255;
    }
  }
  return rgba;
}

fs.mkdirSync(OUT, { recursive: true });
for (const [name, size] of [["32x32.png", 32], ["128x128.png", 128], ["128x128@2x.png", 256]]) {
  fs.writeFileSync(path.join(OUT, name), encodePng(size, size, drawIcon(size)));
}

// ICO（内嵌 256px PNG）
const png256 = fs.readFileSync(path.join(OUT, "128x128@2x.png"));
const icondir = Buffer.alloc(6);
icondir.writeUInt16LE(0, 0);
icondir.writeUInt16LE(1, 2); // type icon
icondir.writeUInt16LE(1, 4); // count
const entry = Buffer.alloc(16);
entry[0] = 0; // 256
entry[1] = 0;
entry[2] = 0;
entry[3] = 0;
entry.writeUInt16LE(1, 4); // planes
entry.writeUInt16LE(32, 6); // bpp
entry.writeUInt32LE(png256.length, 8);
entry.writeUInt32LE(22, 12); // offset
fs.writeFileSync(path.join(OUT, "icon.ico"), Buffer.concat([icondir, entry, png256]));

console.log("icons generated at", OUT);
