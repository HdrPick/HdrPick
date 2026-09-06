// 解析 PE 导入表：列出 exe 依赖的 DLL
const fs = require("fs");
const file = process.argv[2] || "E:\\jietu\\app\\src-tauri\\target\\x86_64-pc-windows-msvc\\debug\\app.exe";
const buf = fs.readFileSync(file);

const eLfanew = buf.readUInt32LE(0x3c);
const optOff = eLfanew + 24;
const magic = buf.readUInt16LE(optOff);
const is64 = magic === 0x20b;
// DataDirectory 位置：64 位 opt+112 / 32 位 opt+96；导入表 = dir[1]
const ddOff = optOff + (is64 ? 112 : 96);
const importRVA = buf.readUInt32LE(ddOff + 8);
const importSize = buf.readUInt32LE(ddOff + 12);

// Section table → RVA 转 file offset
const numSections = buf.readUInt16LE(eLfanew + 6);
const sizeOpt = buf.readUInt16LE(eLfanew + 20);
const secOff = optOff + sizeOpt;
function rvaToOff(rva) {
  for (let i = 0; i < numSections; i++) {
    const s = secOff + i * 40;
    const va = buf.readUInt32LE(s + 12);
    const vs = buf.readUInt32LE(s + 8);
    if (rva >= va && rva < va + vs) return rva - va + buf.readUInt32LE(s + 20);
  }
  return null;
}

console.log(`PE ${is64 ? "64" : "32"} 位，导入表 RVA=0x${importRVA.toString(16)} size=${importSize}`);
let off = rvaToOff(importRVA);
if (off == null) { console.log("无导入表"); process.exit(0); }
const dlls = [];
for (;;) {
  const nameRva = buf.readUInt32LE(off + 12);
  if (nameRva === 0) break;
  const nOff = rvaToOff(nameRva);
  let end = nOff;
  while (buf[end] !== 0) end++;
  dlls.push(buf.toString("ascii", nOff, end));
  off += 20;
}
console.log("依赖 DLL：");
dlls.forEach((d) => console.log("  " + d));
