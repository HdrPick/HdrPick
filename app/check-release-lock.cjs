// 检查 release 目录 exe 文件 + 查找锁定进程
const { execSync } = require("node:child_process");
const fs = require("node:fs");

const dir = "e:/jietu/app/src-tauri/target/x86_64-pc-windows-msvc/release";
const exes = fs.readdirSync(dir).filter((f) => f.endsWith(".exe"));
console.log("release 目录 exe:", exes.join(", "));

for (const n of exes) {
  try {
    const r = execSync(`tasklist /FI "IMAGENAME eq ${n}" /FO CSV`, {
      shell: true,
      encoding: "utf8",
    });
    const lines = r.split("\n").filter((l) => l.includes(".exe"));
    if (lines.length) console.log(`${n} 正在运行:`, lines.join(" | "));
  } catch (_) {}
}
console.log("检查完成");
