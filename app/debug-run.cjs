// 捕获 app.exe 启动输出（stdout/stderr/退出码）
const { spawn } = require("child_process");
const exe = "E:\\jietu\\app\\src-tauri\\target\\x86_64-pc-windows-msvc\\debug\\app.exe";
const cwd = "E:\\jietu\\app\\src-tauri\\target\\x86_64-pc-windows-msvc\\debug";

const p = spawn(exe, [], { cwd, stdio: ["ignore", "pipe", "pipe"] });
let out = "", err = "";
p.stdout.on("data", (d) => (out += d));
p.stderr.on("data", (d) => (err += d));
p.on("exit", (code) => {
  console.log("=== 退出码:", code, "===");
  console.log("--- stdout ---");
  console.log(out || "(空)");
  console.log("--- stderr ---");
  console.log(err || "(空)");
  process.exit(0);
});
// 20 秒后仍未退出视为启动成功（正常驻留）
setTimeout(() => {
  console.log("=== 20 秒未退出：进程存活，启动正常 ===");
  console.log("--- stdout ---");
  console.log(out || "(空)");
  console.log("--- stderr ---");
  console.log(err || "(空)");
  p.kill();
  process.exit(0);
}, 20000);
