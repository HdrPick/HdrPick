// 检查 debug exe 时间戳 + 运行进程
const fs = require("node:fs");
const { execSync } = require("node:child_process");

const p = "e:/jietu/app/src-tauri/target/x86_64-pc-windows-msvc/debug/app.exe";
if (fs.existsSync(p)) {
  const st = fs.statSync(p);
  console.log("debug exe:", st.mtime.toLocaleString(), (st.size / 1024 / 1024).toFixed(1) + "MB");
}

try {
  const r = execSync('tasklist /FI "IMAGENAME eq app.exe" /FO CSV', {
    shell: true,
    encoding: "utf8",
  });
  const lines = r.split("\n").filter((l) => l.includes(".exe"));
  console.log(lines.length ? lines.join("\n") : "app.exe 未运行");
} catch (e) {
  console.log("tasklist 失败");
}
