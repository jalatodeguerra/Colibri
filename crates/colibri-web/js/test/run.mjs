// The web harness: runs colibri.js, with the real colibri.wasm, in node, on virtual time.
//
//   node run.mjs COLIBRI.WASM PACKS_DIR
//
// PACKS_DIR holds what `cargo run -p colibri-web --example make_packs -- PACKS_DIR` writes: for each
// program NAME.pack (what the runner loads) and NAME.out (what it must print), and when the
// program asks for them NAME.exit (it must fail with this exit code), NAME.err (the error stream
// must contain this text) and NAME.ms (the run takes this long on the page's clock, to within
// three frames).
//
// There is no browser. requestAnimationFrame and setTimeout are fakes that share one virtual
// clock: nothing sleeps, and the clock jumps to the next thing that is due, so a program that
// waits for a minute takes no time and the time it took is exact. A program whose timers are
// not armed right (the page would hang, or wake late) ends with no output, or at a wrong time.

import { readFileSync, readdirSync, existsSync } from "node:fs";
import { join } from "node:path";
import { start, FRAME_MS } from "../colibri.js";

/** A virtual clock with the timers of a page. */
class VirtualPage {
  now = 0;
  #queue = [];
  #next = 1;

  #add(at, fn) {
    const id = this.#next++;
    this.#queue.push({ at, id, fn });
    return id;
  }

  #remove(id) {
    this.#queue = this.#queue.filter((e) => e.id !== id);
  }

  setTimeout = (fn, ms) => this.#add(this.now + Math.max(0, ms), fn);
  clearTimeout = (id) => this.#remove(id);
  /** A frame is the next multiple of FRAME_MS. */
  requestAnimationFrame = (fn) =>
    this.#add((Math.floor(this.now / FRAME_MS + 1e-9) + 1) * FRAME_MS, () => fn(this.now));
  cancelAnimationFrame = (id) => this.#remove(id);

  /** Runs what is due, in order, until nothing is armed. */
  drain(limit = 1_000_000) {
    for (let steps = 0; this.#queue.length > 0; steps++) {
      if (steps > limit) throw new Error("the program does not end");
      this.#queue.sort((a, b) => a.at - b.at || a.id - b.id);
      const event = this.#queue.shift();
      this.now = Math.max(this.now, event.at);
      event.fn();
    }
  }
}

function sidecar(dir, name, ext) {
  const path = join(dir, `${name}.${ext}`);
  return existsSync(path) ? readFileSync(path, "utf8").trim() : null;
}

async function runOne(module, dir, name) {
  const page = new VirtualPage();
  let stdout = "";
  let stderr = "";
  const handle = await start({
    wasm: module,
    pack: readFileSync(join(dir, `${name}.pack`)),
    stdout: (text) => (stdout += text),
    stderr: (text) => (stderr += text),
    console: null,
    requestAnimationFrame: page.requestAnimationFrame,
    cancelAnimationFrame: page.cancelAnimationFrame,
    setTimeout: page.setTimeout,
    clearTimeout: page.clearTimeout,
    now: () => page.now,
    nowUnix: () => 1_700_000_000 + page.now / 1000,
    random: (bytes) => bytes.fill(0xa5),
  });
  const problems = [];
  let result = null;
  try {
    page.drain();
    result = await Promise.race([handle.finished, Promise.resolve(null)]);
  } catch (error) {
    problems.push(`the runner was lost: ${error}`);
  }
  if (result === null && problems.length === 0) {
    problems.push("the program never ended: nothing was armed to run it again");
  }
  const expectedOut = readFileSync(join(dir, `${name}.out`), "utf8");
  if (stdout !== expectedOut) {
    problems.push(`printed:\n${stdout}wanted:\n${expectedOut}`);
  }
  const exit = sidecar(dir, name, "exit");
  const wantState = exit === null ? "done" : "failed";
  const wantCode = exit === null ? 0 : Number(exit);
  if (result && (result.state !== wantState || result.exitCode !== wantCode)) {
    problems.push(`ended ${result.state} with ${result.exitCode}, wanted ${wantState} with ${wantCode}`);
  }
  const err = sidecar(dir, name, "err");
  if (err !== null && !stderr.includes(err)) {
    problems.push(`the error stream lacks "${err}": ${stderr}`);
  }
  const ms = sidecar(dir, name, "ms");
  if (ms !== null && (page.now < Number(ms) || page.now > Number(ms) + 3 * FRAME_MS)) {
    problems.push(`took ${page.now.toFixed(1)} ms of the page's clock, wanted about ${ms}`);
  }
  return { problems, stderr, ticks: handle.tickCount, ms: page.now };
}

async function main() {
  const [wasmPath, dir] = process.argv.slice(2);
  if (!wasmPath || !dir) {
    console.error("usage: node run.mjs COLIBRI.WASM PACKS_DIR");
    process.exit(2);
  }
  const wasm = readFileSync(wasmPath);
  console.log(`colibri.wasm: ${wasm.length} bytes`);
  const module = await WebAssembly.compile(wasm);
  const names = readdirSync(dir)
    .filter((f) => f.endsWith(".pack"))
    .map((f) => f.slice(0, -5))
    .sort();
  if (names.length < 8) {
    console.error(`only ${names.length} programs in ${dir}`);
    process.exit(2);
  }
  let failed = 0;
  for (const name of names) {
    const { problems, stderr, ticks, ms } = await runOne(module, dir, name);
    if (problems.length === 0) {
      console.log(`ok   ${name} (${ticks} ticks, ${ms.toFixed(0)} ms)`);
    } else {
      failed += 1;
      console.log(`FAIL ${name}\n  ${problems.join("\n  ").replaceAll("\n", "\n  ")}\n  stderr: ${stderr}`);
    }
  }
  console.log(`${names.length - failed} passed, ${failed} failed`);
  process.exit(failed === 0 ? 0 : 1);
}

main();
