// V18 视频画质增强（FX 参数化）编译验证
// 日志规则：启动时清空 fx-check.log，之后完整累加全部输出
const { spawnSync } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");

const LOG = path.join(__dirname, "fx-check.log");
// 先清空日志
fs.writeFileSync(LOG, "", "utf8");

function log(chunk) {
  fs.appendFileSync(LOG, chunk, "utf8");
}

function run(name, cmd, args, cwd) {
  log(`\n===== [${name}] ${cmd} ${args.join(" ")} =====\n`);
  const r = spawnSync(cmd, args, {
    cwd: cwd || __dirname,
    encoding: "utf8",
    shell: true,
    env: { ...process.env, FORCE_COLOR: "0" },
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

// 2. videorec crate 编译检查（FX 改动核心）
const CARGO = "C:\\Users\\Administrator\\.cargo\\bin\\cargo.exe";
const r1 = run("videorec-check", CARGO, ["check", "--message-format", "short"], path.join(__dirname, "..", "videorec"));

// 3. app Rust 构建（当前构建方式：显式 triple；crt-static 由 src-tauri/.cargo/config.toml 提供）
const r2 = run(
  "app-build",
  CARGO,
  ["build", "--target", "x86_64-pc-windows-msvc", "--message-format", "short"],
  path.join(__dirname, "src-tauri"),
);

// 摘要：错误行 + 尾部 60 行
for (const r of [r1, r2]) {
  const errorLines = r.out.split("\n").filter((l) => /error(\[|:)/.test(l));
  log(`\n===== ERROR LINES [${r.name}] =====\n` + (errorLines.join("\n") || "(none)") + "\n");
  console.log(`[${r.name}] ${r.status === 0 ? "OK" : "FAIL(exit " + r.status + ")"}`);
  if (r.status !== 0) {
    console.log(r.out.split("\n").slice(-40).join("\n"));
  }
}
console.log(`完整日志: ${LOG}`);
if (r1.status !== 0 || r2.status !== 0) process.exit(1);
