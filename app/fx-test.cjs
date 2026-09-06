// V18 FX 改动回归测试（videorec crate）
// 日志规则：启动时清空 fx-test.log，之后完整累加全部输出
const { spawnSync } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");

const LOG = path.join(__dirname, "fx-test.log");
fs.writeFileSync(LOG, "", "utf8");
const log = (c) => fs.appendFileSync(LOG, c, "utf8");

const CARGO = "C:\\Users\\Administrator\\.cargo\\bin\\cargo.exe";
const r = spawnSync(CARGO, ["test", "--message-format", "short"], {
  cwd: path.join(__dirname, "..", "videorec"),
  encoding: "utf8",
  shell: true,
  env: { ...process.env, FORCE_COLOR: "0" },
  maxBuffer: 64 * 1024 * 1024,
});
const out = (r.stdout || "") + (r.stderr || "");
log(out);
const failed = out.split("\n").filter((l) => /FAILED|panicked|test result: FAILED/.test(l));
log("\n===== FAILURES =====\n" + (failed.join("\n") || "(none)") + "\n");
const summary = out.split("\n").filter((l) => /test result:/.test(l));
console.log(summary.join("\n") || "(no summary)");
console.log(`exit=${r.status} 完整日志: ${LOG}`);
