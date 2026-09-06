// 构建验证：先清空日志，再完整累加 stdout/stderr 输出（含错误）
// 用法: node scripts/build-verify.mjs [frontend|rust|debug|release|all]
import { spawnSync } from "node:child_process";
import { openSync, writeSync, closeSync, ftruncateSync, existsSync } from "node:fs";

const LOG = new URL("../build-verify.log", import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, "$1");
const mode = process.argv[2] ?? "all";

// VS BuildTools 自带 cmake（libjxl 构建需要），不在系统 PATH 时注入
// 剥离 CI=1：tauri CLI 的 --ci 只接受 true/false，环境泄漏的 1 会让构建直接报错
const VSCMAKE = "C:\\Program Files (x86)\\Microsoft Visual Studio\\2022\\BuildTools\\Common7\\IDE\\CommonExtensions\\Microsoft\\CMake\\CMake\\bin";
const BUILD_ENV = {
  ...process.env,
  PATH: existsSync(VSCMAKE) && !process.env.PATH?.includes(VSCMAKE)
    ? `${VSCMAKE};${process.env.PATH}`
    : process.env.PATH,
};
delete BUILD_ENV.CI;

// 先清空日志
const fd = openSync(LOG, "w");
ftruncateSync(fd, 0);
const write = (s) => writeSync(fd, s);
write(`=== build-verify ${new Date().toISOString()} mode=${mode} ===\n`);

function run(name, cmd, args, cwd) {
  write(`\n--- [${name}] ${cmd} ${args.join(" ")} ---\n`);
  const r = spawnSync(cmd, args, { cwd, encoding: "utf8", shell: true, maxBuffer: 64 * 1024 * 1024, env: BUILD_ENV });
  const out = (r.stdout ?? "") + (r.stderr ?? "");
  write(out);
  write(`\n--- [${name}] exit=${r.status} ---\n`);
  return r.status === 0;
}

const appDir = new URL("..", import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, "$1");
let ok = true;

if (mode === "frontend" || mode === "all") {
  ok = run("frontend:pnpm build", "pnpm", ["build"], appDir) && ok;
}
if (mode === "rust" || mode === "all") {
  ok = run("rust:cargo check", "cargo", ["check"], appDir + "/src-tauri") && ok;
}
if (mode === "debug") {
  // 前端重打包 + 强制重链（cargo 对 dist 变化不总是重编，clean -p app 确保嵌入新 dist）
  ok = run("frontend:pnpm build", "pnpm", ["build"], appDir) && ok;
  ok = run("rust:cargo clean -p app", "cargo", ["clean", "-p", "app"], appDir + "/src-tauri") && ok;
  ok = run("rust:cargo build", "cargo", ["build"], appDir + "/src-tauri") && ok;
}
if (mode === "release") {
  // 正式版：必须走官方 CLI（自动 beforeBuildCommand 打包前端并嵌入；
  // 直接 cargo build --release 不嵌 dist，运行时连 devUrl → "localhost 拒绝连接"）
  // --no-bundle 跳过安装包生成，只产出 exe
  ok = run("tauri build --no-bundle", "pnpm", ["tauri", "build", "--no-bundle"], appDir) && ok;
}

write(`\n=== RESULT: ${ok ? "SUCCESS" : "FAILED"} ===\n`);
closeSync(fd);
console.log(ok ? "BUILD OK" : "BUILD FAILED (see build-verify.log)");
process.exit(ok ? 0 : 1);
