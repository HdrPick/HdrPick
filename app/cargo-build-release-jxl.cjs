// 正式版构建脚本（JXL/JXR HDR 编解码 + 文件关联 + 离线资源版）
// 日志规则：启动时清空 tauri-build-release-jxl.log，之后完整累加全部输出
const { spawnSync, execSync } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");

const LOG = path.join(__dirname, "tauri-build-release-jxl.log");
// 先清空日志
fs.writeFileSync(LOG, "", "utf8");

function log(chunk) {
  fs.appendFileSync(LOG, chunk, "utf8");
}

function run(name, cmd, args, cwd, extraEnv) {
  log(`\n===== [${name}] ${cmd} ${args.join(" ")} =====\n`);
  // VS BuildTools 自带 cmake + nodejs（均不在系统 PATH，prepend 进子进程 PATH）
  const cmakeDir = "C:\\Program Files (x86)\\Microsoft Visual Studio\\2022\\BuildTools\\Common7\\IDE\\CommonExtensions\\Microsoft\\CMake\\CMake\\bin";
  const nodeDir = "C:\\Program Files\\nodejs";
  const env = { ...process.env, FORCE_COLOR: "0", ...(extraEnv || {}) };
  env.PATH = cmakeDir + ";" + nodeDir + ";" + (env.PATH || "");
  const r = spawnSync(cmd, args, {
    cwd: cwd || __dirname,
    encoding: "utf8",
    shell: true,
    env,
    maxBuffer: 64 * 1024 * 1024,
  });
  const out = (r.stdout || "") + (r.stderr || "");
  log(out);
  log(`\n===== [${name}] exit=${r.status} =====\n`);
  return { name, status: r.status, out };
}

// 1. 结束残留应用进程（避免 exe/dll 文件锁；/T 含 WebView2 子进程树）
for (const img of ["app.exe", "hdr.exe"]) {
  try {
    spawnSync("taskkill", ["/IM", img, "/F", "/T"], { shell: true, encoding: "utf8" });
  } catch (_) {}
}
// 等待文件句柄释放（taskkill 异步完成句柄关闭）
execSync('powershell -Command "Start-Sleep -Milliseconds 1500"', { shell: true });

const results = [];

// 2. 前端构建（生成 dist：release exe 的内嵌离线资源）
//    pnpm 不在系统 PATH（node 全局 bin 在 %APPDATA%\npm）
const PNPM = path.join(process.env.APPDATA || "", "npm", "pnpm.cmd");
results.push(run("frontend-build", PNPM, ["build"], __dirname));

// 3. Rust release 构建：
//    TAURI_CONFIG 覆盖 devUrl=null → 内嵌 frontendDist(dist) 而非 localhost:1420
//    release profile：opt-level=3 + LTO(fat) + codegen-units=1 + strip symbols（耗时较长）
if (results[0].status === 0) {
  const CARGO = "C:\\Users\\Administrator\\.cargo\\bin\\cargo.exe";
  results.push(
    run("rust-release-build", CARGO, ["build", "--release"], path.join(__dirname, "src-tauri"), {
      TAURI_CONFIG: JSON.stringify({ build: { devUrl: null } }),
    }),
  );
}

// 摘要
log("\n===== SUMMARY =====\n");
for (const r of results) {
  log(`[${r.name}] ${r.status === 0 ? "OK" : "FAIL(exit " + r.status + ")"}\n`);
  if (r.status !== 0) {
    log(r.out.split("\n").slice(-40).join("\n") + "\n");
  }
}

// 4. 验证：exe 内不得包含 devUrl（否则运行时连 localhost:1420 拒绝连接）
//    bin 名为 hdr（Cargo.toml [[bin]]），旧残留 app.exe 兜底
const exeDir = path.join(__dirname, "src-tauri", "target", "x86_64-pc-windows-msvc", "release");
const exe = ["hdr.exe", "app.exe"]
  .map((f) => path.join(exeDir, f))
  .find((p) => fs.existsSync(p) && fs.statSync(p).size > 1024 * 1024);
let offline = "未知（exe 未生成）";
if (exe) {
  const buf = fs.readFileSync(exe);
  const hasDevUrl = buf.includes(Buffer.from("localhost:1420"));
  offline = hasDevUrl ? "FAIL：仍内嵌 devUrl（会连 localhost:1420）" : "OK：内嵌离线资源";
  log(`[devurl-check] ${path.basename(exe)} ${offline}\n`);
}
console.log(results.map((r) => `[${r.name}] ${r.status === 0 ? "OK" : "FAIL"}`).join("  "));
console.log(`[devurl-check] ${offline}`);
console.log(
  `exe: ${exe ? path.basename(exe) + " " + (fs.statSync(exe).size / 1024 / 1024).toFixed(1) + " MB, " + fs.statSync(exe).mtime.toLocaleString() : "未生成"}`,
);
console.log(`完整日志: ${LOG}`);
