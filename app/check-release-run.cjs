// 验证正式版运行：等 6 秒后读运行日志尾部 + 文件关联注册表检查
const { execSync } = require("node:child_process");
const fs = require("node:fs");

execSync('powershell -Command "Start-Sleep -Seconds 6"', { shell: true });

// 1. 运行日志（release exe 目录）
const logPath = "e:/jietu/app/src-tauri/target/x86_64-pc-windows-msvc/release/jietu-hdr.log";
if (fs.existsSync(logPath)) {
  console.log("===== jietu-hdr.log 尾部 =====");
  console.log(fs.readFileSync(logPath, "utf8").split("\n").slice(-24).join("\n"));
} else {
  console.log("log not found: " + logPath);
}

// 2. 文件关联注册表验证：jxr/wdp/exr 的 OpenWithProgids 应含 jietu.hdr.Image
const { spawnSync } = require("node:child_process");
for (const ext of ["jxl", "jxr", "wdp", "exr"]) {
  const r = spawnSync(
    "reg",
    ["query", `HKCU\\Software\\Classes\\.${ext}\\OpenWithProgids`, "/v", "jietu.hdr.Image"],
    { shell: true, encoding: "utf8" },
  );
  const out = (r.stdout || "") + (r.stderr || "");
  console.log(
    `\n[assoc .${ext}] ` + (out.includes("jietu.hdr.Image") ? "已注册" : "未注册"),
  );
}
