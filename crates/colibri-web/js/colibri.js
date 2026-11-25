// colibri.js: runs a Colibri program (a pack) with colibri.wasm in a page, or under node.
//
//   import { run } from "./colibri.js";
//   const result = await run({ wasm: "colibri.wasm", pack: "app.pack", stdout: element });
//
// The wasm module cannot block the page, so it never runs a program to its end: `colibri_start`
// runs it to its first `wait`, and from then on this loop calls `colibri_tick(dt)` with the time
// that has passed. `colibri_next_deadline()` says when the next tick is needed: on the next frame
// (requestAnimationFrame) while a coroutine is due or polls, with a timer (setTimeout) when the
// next one is further away, and not at all while nothing is due (the program then ends).
//
// Everything the loop touches can be injected, so that a test under node drives it with a fake
// clock: `fetch`, `requestAnimationFrame`, `cancelAnimationFrame`, `setTimeout`, `clearTimeout`,
// `now` (milliseconds, monotonic), `nowUnix` (seconds since 1970) and `random` (fills a Uint8Array).
//
// Exports: `run(options)`, `runInPage(options)`, `STATE` and `FRAME_MS`.

/** The states `colibri_start` and `colibri_tick` return. */
export const STATE = Object.freeze({ running: 0, done: 1, failed: 2 });

/** A deadline closer than this is a tick on the next frame; a later one is a timer. */
export const FRAME_MS = 1000 / 60;

/** What `run` resolves to when the program is over. */
const names = ["running", "done", "failed"];

/**
 * Bytes of a file: `source` is a URL (fetched), or bytes already (ArrayBuffer, typed array). A
 * compiled `WebAssembly.Module` is passed through (a test compiles the runner once).
 */
async function load(source, fetchFn, what) {
  if (source instanceof WebAssembly.Module) return source;
  if (typeof source === "string" || source instanceof URL) {
    const response = await fetchFn(source);
    if (!response.ok) {
      throw new Error(`cannot load the ${what} (${source}): ${response.status}`);
    }
    return new Uint8Array(await response.arrayBuffer());
  }
  if (source instanceof ArrayBuffer) return new Uint8Array(source);
  if (ArrayBuffer.isView(source)) {
    return new Uint8Array(source.buffer, source.byteOffset, source.byteLength);
  }
  throw new Error(`the ${what} must be a URL or bytes`);
}

/** A function that takes text, from what the caller gave: a function, an element, or nothing. */
function textSink(target) {
  if (typeof target === "function") return target;
  if (target && typeof target === "object" && "textContent" in target) {
    return (text) => {
      target.textContent += text;
    };
  }
  return () => {};
}

/** Decodes UTF-8 that arrives in pieces, which may split a character. */
function decoderFor(sink) {
  const decoder = new TextDecoder("utf-8");
  return (bytes) => {
    const text = decoder.decode(bytes, { stream: true });
    if (text) sink(text);
  };
}

/** The default `random`: `crypto.getRandomValues` in chunks (it takes at most 65536 bytes). */
function defaultRandom(bytes) {
  for (let at = 0; at < bytes.length; at += 65536) {
    globalThis.crypto.getRandomValues(bytes.subarray(at, at + 65536));
  }
}

/**
 * Runs a program.
 *
 * Options (all but `wasm` and `pack` are optional):
 * - `wasm`, `pack`: a URL or the bytes of `colibri.wasm` (or a compiled `WebAssembly.Module`) and of
 *   the pack.
 * - `stdout`, `stderr`: a function that takes text, or an element (its `textContent` grows). The
 *   error stream also goes to `console.error`, unless `console` is `null`.
 * - `fetch`, `requestAnimationFrame`, `cancelAnimationFrame`, `setTimeout`, `clearTimeout`, `now`,
 *   `nowUnix`, `random`, `console`: what the page has, replaced by a test.
 * - `onState(name)`: called when the program starts and when it ends.
 *
 * Resolves, when the program is over, to `{ state: "done" | "failed", exitCode }`. Rejects when
 * the wasm module cannot be loaded or its instance is lost (a trap, which also writes
 * "internal error" to the error stream first). `stop()` of the result of `start` ends it early.
 */
export async function run(options) {
  const handle = await start(options);
  return handle.finished;
}

/** [`run`], with the handle: `{ finished: Promise, stop(), tickCount }`. */
export async function start(options) {
  const g = globalThis;
  const fetchFn = options.fetch ?? g.fetch?.bind(g);
  const raf = options.requestAnimationFrame ?? g.requestAnimationFrame?.bind(g);
  const caf = options.cancelAnimationFrame ?? g.cancelAnimationFrame?.bind(g);
  const setTimer = options.setTimeout ?? g.setTimeout.bind(g);
  const clearTimer = options.clearTimeout ?? g.clearTimeout.bind(g);
  const now = options.now ?? (() => g.performance.now());
  const nowUnix = options.nowUnix ?? (() => Date.now() / 1000);
  const random = options.random ?? defaultRandom;
  const con = options.console === undefined ? g.console : options.console;
  const outText = decoderFor(textSink(options.stdout));
  const pageErr = textSink(options.stderr);
  const errText = decoderFor((text) => {
    pageErr(text);
    con?.error(text.replace(/\n$/, ""));
  });

  const [wasmBytes, packBytes] = await Promise.all([
    load(options.wasm, fetchFn, "runner"),
    load(options.pack, fetchFn, "program"),
  ]);

  let memory = null;
  const view = (ptr, len) => new Uint8Array(memory.buffer, ptr, len);
  const imports = {
    env: {
      host_write: (fd, ptr, len) => (fd === 2 ? errText : outText)(view(ptr, len)),
      host_now_real: () => now(),
      host_now_unix: () => nowUnix(),
      host_random: (ptr, len) => random(view(ptr, len)),
    },
  };
  const instance = wasmBytes instanceof WebAssembly.Module
    ? await WebAssembly.instantiate(wasmBytes, imports)
    : (await WebAssembly.instantiate(wasmBytes, imports)).instance;
  const x = instance.exports;
  memory = x.memory;

  let over = false;
  let timer = null; // { kind: "frame" | "timer", id }
  let last = now();
  let ticks = 0;
  let finish;
  let abandon;
  const finished = new Promise((resolve, reject) => {
    finish = resolve;
    abandon = reject;
  });

  /** Ends the loop; the caller says how. */
  const conclude = (state) => {
    if (over) return;
    over = true;
    cancelPending();
    options.onState?.(names[state]);
    finish({ state: names[state], exitCode: x.colibri_exit_code() });
  };

  /** A trap: the instance is lost. What the engine could say is already on the error stream. */
  const lost = (error) => {
    if (over) return;
    over = true;
    cancelPending();
    errText(new TextEncoder().encode("the program stopped: the runner trapped\n"));
    options.onState?.("failed");
    abandon(error);
  };

  function cancelPending() {
    if (!timer) return;
    if (timer.kind === "frame") caf?.(timer.id);
    else clearTimer(timer.id);
    timer = null;
  }

  /** Calls into the wasm module, turning a trap into `lost`. */
  const guarded = (call) => {
    try {
      return call();
    } catch (error) {
      lost(error);
      return null;
    }
  };

  /** Arranges the next tick from what the program says it needs. */
  function schedule() {
    if (over || timer) return;
    const delay = guarded(() => x.colibri_next_deadline());
    if (delay === null || over) return;
    if (delay < 0) {
      // Running, yet nothing will ever be due. The module ends a program that is stuck, so this
      // is a module that disagrees with itself: stop, as a failure.
      conclude(STATE.failed);
      return;
    }
    if (delay <= FRAME_MS && raf) {
      timer = { kind: "frame", id: raf(onWake) };
    } else {
      // Rounded up: a timer that fires a hair early would find nothing due and ask again.
      timer = { kind: "timer", id: setTimer(onWake, Math.ceil(delay)) };
    }
  }

  function onWake() {
    timer = null;
    if (over) return;
    const at = now();
    const dt = Math.max(0, (at - last) / 1000);
    last = at;
    ticks += 1;
    const state = guarded(() => x.colibri_tick(dt));
    if (state === null) return;
    if (state !== STATE.running) conclude(state);
    else schedule();
  }

  // The program starts here and runs to its first `wait`; the pack is copied by the module.
  const ptr = x.colibri_alloc(packBytes.length);
  view(ptr, packBytes.length).set(packBytes);
  options.onState?.("running");
  last = now();
  const first = guarded(() => x.colibri_start(ptr, packBytes.length));
  guarded(() => x.colibri_free(ptr, packBytes.length));
  if (first !== null) {
    if (first !== STATE.running) conclude(first);
    else schedule();
  }

  return {
    finished,
    /** Ends the run now, as a failure the page asked for. */
    stop() {
      if (over) return;
      over = true;
      cancelPending();
      options.onState?.("failed");
      finish({ state: "failed", exitCode: 1 });
    },
    /** How many ticks the loop made (for tests). */
    get tickCount() {
      return ticks;
    },
  };
}

/**
 * [`run`] in a page: the output goes to the element `#colibri-output` (or `options.output`) and
 * the files are `colibri.wasm` and `app.pack` next to the page, unless `options` names others.
 */
export function runInPage(options = {}) {
  const output = options.output ?? document.getElementById("colibri-output");
  return run({
    wasm: new URL("colibri.wasm", import.meta.url),
    pack: new URL("app.pack", import.meta.url),
    stdout: output,
    stderr: output,
    ...options,
  });
}
