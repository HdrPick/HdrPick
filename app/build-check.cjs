// 构建验证脚本：前端 vue-tsc+vite build / Rust cargo check
// 日志规则：启动时清空 build-check.log，之后完整累加全部输出
const { spawnSync } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");

const LOG = path.join(__dirname, "build-check.log");
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
    shell: true, // Windows 下 npm/cargo 需要
    env: { ...process.env, FORCE_COLOR: "0" },
    maxBuffer: 64 * 1024 * 1024,
  });
  const out = (r.stdout || "") + (r.stderr || "");
  log(out);
  log(`\n===== [${name}] exit=${r.status} =====\n`);
  return { name, status: r.status, tail: out.split("\n").slice(-5).join("\n") };
}

const results = [];
// 前端：类型检查 + 构建
results.push(run("frontend-build", "npm", ["run", "build"]));
// Rust：编译检查（src-tauri）
results.push(
  run("rust-check", "cargo", ["check", "--message-format", "short"], path.join(__dirname, "src-tauri")),
);

// 摘要
log("\n===== SUMMARY =====\n");
for (const r of results) {
  log(`[${r.name}] ${r.status === 0 ? "OK" : "FAIL(exit " + r.status + ")"}\n`);
  if (r.status !== 0) log(r.tail + "\n");
}
console.log(results.map((r) => `[${r.name}] ${r.status === 0 ? "OK" : "FAIL"}`).join("  "));
console.log(`完整日志: ${LOG}`);
