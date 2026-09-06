// 运行 HDR 编码验证 example（证明 JXL/JXR 存的是原生 HDR 数据）
// 日志规则：启动时清空 hdr-verify-example.log，之后完整累加全部输出
const { spawnSync, execSync } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");

const LOG = path.join(__dirname, "hdr-verify-example.log");
fs.writeFileSync(LOG, "", "utf8");

const cmakeDir = "C:\\Program Files (x86)\\Microsoft Visual Studio\\2022\\BuildTools\\Common7\\IDE\\CommonExtensions\\Microsoft\\CMake\\CMake\\bin";
const env = { ...process.env, PATH: cmakeDir + ";" + (process.env.PATH || "") };

const r = spawnSync(
  "C:\\Users\\Administrator\\.cargo\\bin\\cargo.exe",
  ["run", "--example", "jxl_jxr_hdr_verify"],
  {
    cwd: path.join(__dirname, "src-tauri"),
    encoding: "utf8",
    shell: true,
    env,
    maxBuffer: 64 * 1024 * 1024,
  },
);
const out = (r.stdout || "") + (r.stderr || "");
fs.appendFileSync(LOG, out);
console.log("exit=" + r.status);
console.log(out.split("\n").slice(-60).join("\n"));
