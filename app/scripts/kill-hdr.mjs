// 结束占用 hdr.exe 的进程（旧实例运行中会锁住 release 产物导致链接失败）
import { execSync } from "node:child_process";

try {
  const out = execSync('tasklist /FI "IMAGENAME eq hdr.exe"', { encoding: "utf8" });
  console.log(out.trim());
  if (out.includes("hdr.exe")) {
    execSync("taskkill /F /IM hdr.exe");
    console.log(">>> hdr.exe 已结束");
  } else {
    console.log(">>> 没有运行中的 hdr.exe");
  }
} catch (e) {
  console.log(">>> taskkill 输出:", e.stdout ? e.stdout.toString() : "", e.message);
}
