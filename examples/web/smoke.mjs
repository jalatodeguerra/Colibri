// Smoke test of a built web folder, as a browser would use it, without a browser:
//
//   node smoke.mjs DIR EXPECTED.TXT
//
// It serves DIR (serve.mjs), fetches the page and checks that it names the files it loads and
// the element it prints into, fetches the files, then runs the fetched colibri.js against them
// with a stand-in for the page's output element, and compares what the program printed with
// EXPECTED.TXT (the output of the program, one line per line).

import assert from "node:assert/strict";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { serve } from "./serve.mjs";

const [dir, expectedFile] = process.argv.slice(2);
if (!dir || !expectedFile) {
  console.error("usage: node smoke.mjs DIR EXPECTED.TXT");
  process.exit(2);
}

const server = await serve(dir);
const scratch = await mkdtemp(join(tmpdir(), "colibri-smoke-"));
try {
  const get = async (name, type) => {
    const response = await fetch(server.url + name);
    assert.equal(response.status, 200, `${name} is served`);
    assert.match(response.headers.get("content-type"), type, `${name} has a usable type`);
    return Buffer.from(await response.arrayBuffer());
  };

  const page = (await get("", /^text\/html/)).toString();
  assert.equal(page, (await get("index.html", /^text\/html/)).toString(), "/ is index.html");
  for (const needed of ["colibri.js", "colibri.wasm", "app.pack", 'id="colibri-output"']) {
    assert.ok(page.includes(needed), `index.html names ${needed}`);
  }
  const script = await get("colibri.js", /javascript/);
  const wasm = await get("colibri.wasm", /^application\/wasm/);
  const pack = await get("app.pack", /./);
  assert.deepEqual([...wasm.subarray(0, 4)], [0, 0x61, 0x73, 0x6d], "colibri.wasm is a module");
  console.log(`colibri.wasm ${wasm.length} bytes, app.pack ${pack.length} bytes, colibri.js ${script.length} bytes`);

  // The page's element and `document`, as far as colibri.js reaches them.
  const output = { textContent: "" };
  globalThis.document = { getElementById: (id) => (id === "colibri-output" ? output : null) };
  // colibri.js is a module: the copy that was served, written where node can import it.
  const copy = join(scratch, "colibri.mjs");
  await writeFile(copy, script);
  const { runInPage } = await import(pathToFileURL(copy));
  const result = await runInPage({
    wasm: server.url + "colibri.wasm",
    pack: server.url + "app.pack",
    console: null,
  });

  const expected = await readFile(expectedFile, "utf8");
  assert.equal(output.textContent, expected, "what the page shows");
  assert.deepEqual(result, { state: "done", exitCode: 0 });
  console.log(`the page showed ${expected.split("\n").length - 1} lines, as expected`);
} finally {
  await server.close();
  await rm(scratch, { recursive: true, force: true });
}
