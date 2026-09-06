// 检查 app.exe 是否在运行 + 读取运行日志尾部
const { execSync } = require("node:child_process");
const fs = require("node:fs");

try {
  const out = execSync('tasklist /FI "IMAGENAME eq app.exe" /FO CSV', {
    shell: true,
    encoding: "utf8",
  });
  const lines = out.split("\n").filter((l) => l.includes("app.exe"));
  console.log(lines.length ? lines.join("\n") : "app.exe not running");
} catch (e) {
  console.log("tasklist check failed:", e.message);
}

// 运行日志（exe 目录 jietu-hdr.log，按约定：启动截断，运行中累加）
const logCandidates = [
  "e:/jietu/app/src-tauri/target/x86_64-pc-windows-msvc/debug/jietu-hdr.log",
];
for (const p of logCandidates) {
  if (fs.existsSync(p)) {
    const c = fs.readFileSync(p, "utf8");
    console.log("\n===== jietu-hdr.log 尾部 =====");
    console.log(c.split("\n").slice(-30).join("\n"));
    break;
  }
}
