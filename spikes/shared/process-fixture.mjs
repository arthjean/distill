import { spawn } from "node:child_process";
import { closeSync, writeFileSync } from "node:fs";

const mode = process.argv[2];

if (mode === "argv") {
  process.stdout.write(`argv:${JSON.stringify(process.argv.slice(3))}\n`);
  process.stderr.write("synthetic-stderr\n");
} else if (mode === "timeout") {
  setTimeout(() => process.stdout.write("late\n"), 5_000);
} else if (mode === "chatter") {
  setInterval(() => process.stdout.write("tick\n"), 10);
} else if (mode === "closed-streams") {
  closeSync(1);
  closeSync(2);
  setTimeout(() => {}, 5_000);
} else if (mode === "signal") {
  process.kill(process.pid, "SIGTERM");
} else if (mode === "descendant") {
  const descendant = spawn(
    process.execPath,
    [process.argv[1], "marker", process.argv[3]],
    { stdio: ["ignore", "inherit", "inherit"] },
  );
  descendant.unref();
  process.stdout.write("parent-exited\n");
} else if (mode === "marker") {
  setTimeout(() => writeFileSync(process.argv[3], "survived\n"), 300);
} else {
  process.stderr.write("unknown fixture mode\n");
  process.exitCode = 2;
}
