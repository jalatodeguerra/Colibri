// A tiny static file server (no dependencies) for the folder `colibri build --web` writes:
//
//   node serve.mjs DIR [PORT]      # PORT 0, the default, picks a free one
//
// Open the printed address in a browser. A page opened straight from disk (file://) cannot load
// its files, which is why a server is needed at all. `serve(dir)` is also what smoke.mjs uses.

import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { extname, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const TYPES = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".wasm": "application/wasm",
  ".pack": "application/octet-stream",
};

/** Serves `dir` on 127.0.0.1; resolves to `{ url, close() }`. */
export function serve(dir, port = 0) {
  const root = resolve(dir);
  const server = createServer(async (request, response) => {
    try {
      const { pathname } = new URL(request.url, "http://localhost");
      const wanted = decodeURIComponent(pathname.endsWith("/") ? `${pathname}index.html` : pathname);
      const file = resolve(root, `.${wanted}`);
      if (file !== root && !file.startsWith(root + sep)) throw new Error("outside the folder");
      const body = await readFile(file);
      response.writeHead(200, {
        "content-type": TYPES[extname(file)] ?? "application/octet-stream",
        "content-length": body.length,
      });
      response.end(body);
    } catch {
      response.writeHead(404, { "content-type": "text/plain" });
      response.end("not found\n");
    }
  });
  return new Promise((done, fail) => {
    server.once("error", fail);
    server.listen(port, "127.0.0.1", () => {
      const { port: bound } = server.address();
      done({
        url: `http://127.0.0.1:${bound}/`,
        close: () => new Promise((closed) => server.close(closed)),
      });
    });
  });
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const [dir, port] = process.argv.slice(2);
  if (!dir) {
    console.error("usage: node serve.mjs DIR [PORT]");
    process.exit(2);
  }
  const { url } = await serve(dir, Number(port ?? 0));
  console.log(`serving ${dir} at ${url}`);
}
