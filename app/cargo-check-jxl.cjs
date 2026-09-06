// JXL/JXR 编解码集成编译验证
// 日志规则：启动时清空 cargo-check-jxl.log，之后完整累加全部输出
const { spawnSync } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");

const LOG = path.join(__dirname, "cargo-check-jxl.log");
// 先清空日志
fs.writeFileSync(LOG, "", "utf8");

function log(chunk) {
  fs.appendFileSync(LOG, chunk, "utf8");
}

function run(name, cmd, args, cwd) {
  log(`\n===== [${name}] ${cmd} ${args.join(" ")} =====\n`);
  // VS BuildTools 自带 cmake（不在系统 PATH，prepend 进子进程 PATH）
  const cmakeDir = "C:\\Program Files (x86)\\Microsoft Visual Studio\\2022\\BuildTools\\Common7\\IDE\\CommonExtensions\\Microsoft\\CMake\\CMake\\bin";
  const env = { ...process.env, FORCE_COLOR: "0" };
  env.PATH = cmakeDir + ";" + (env.PATH || "");
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

// 1. 结束残留应用进程（避免 exe/dll 文件锁导致 os error 5）
try {
  spawnSync("taskkill", ["/IM", "app.exe", "/F"], { shell: true, encoding: "utf8" });
} catch (_) {}
try {
  spawnSync("taskkill", ["/IM", "hdr.exe", "/F"], { shell: true, encoding: "utf8" });
} catch (_) {}

// 2. Rust 编译检查（src-tauri，首次会 cmake 构建 libjxl，耗时较长）
const CARGO = "C:\\Users\\Administrator\\.cargo\\bin\\cargo.exe";
const r = run("rust-check", CARGO, ["check", "--message-format", "short"], path.join(__dirname, "src-tauri"));

// 摘要：错误行 + 尾部 60 行
const errorLines = r.out.split("\n").filter((l) => /error(\[|:)/.test(l));
log("\n===== ERROR LINES =====\n" + (errorLines.join("\n") || "(none)") + "\n");

console.log(`[rust-check] ${r.status === 0 ? "OK" : "FAIL(exit " + r.status + ")"}`);
if (r.status !== 0) {
  console.log(r.out.split("\n").slice(-60).join("\n"));
}
console.log(`完整日志: ${LOG}`);
