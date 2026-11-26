// Checks `package.json` of the extension without VS Code or npm: it parses, contributes the
// `colibri` debugger with the launch attributes the adapter reads, and `extension.js` is
// valid JavaScript. CI runs it: `node editors/vscode/check-package.mjs`.
import { execFileSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const problems = [];
const check = (ok, what) => {
  if (!ok) problems.push(what);
};

const pkg = JSON.parse(readFileSync(join(here, "package.json"), "utf8"));
for (const key of ["name", "version", "publisher", "engines", "main", "contributes", "activationEvents"]) {
  check(pkg[key] !== undefined, `package.json has no \`${key}\``);
}
check(existsSync(join(here, pkg.main ?? "extension.js")), "`main` names a file that does not exist");
check(pkg.activationEvents?.includes("onDebug"), "activationEvents lacks `onDebug`");

const contributes = pkg.contributes ?? {};
check(
  (contributes.breakpoints ?? []).some((b) => b.language === "colibri"),
  "breakpoints are not enabled for the colibri language (the editor would refuse them)",
);
const debuggers = contributes.debuggers ?? [];
check(debuggers.length === 1, "exactly one debugger must be contributed");
const dbg = debuggers[0] ?? {};
check(dbg.type === "colibri", "the debugger type is `colibri`");
check(typeof dbg.label === "string" && dbg.label.length > 0, "the debugger has a label");
check((dbg.languages ?? []).includes("colibri"), "the debugger lists the colibri language");

// The launch attributes, against what the adapter reads (`crates/colibri-dap/src/server.rs`).
const launch = dbg.configurationAttributes?.launch ?? {};
const props = launch.properties ?? {};
const wanted = {
  program: "string",
  args: "array",
  cwd: "string",
  features: "array",
  stopOnEntry: "boolean",
  noDebug: "boolean",
  watch: "boolean",
};
for (const [name, type] of Object.entries(wanted)) {
  check(props[name]?.type === type, `launch attribute \`${name}\` must be of type ${type}`);
  check(
    typeof props[name]?.description === "string" && props[name].description.length > 0,
    `launch attribute \`${name}\` needs a description`,
  );
}
check(
  JSON.stringify(Object.keys(props).sort()) === JSON.stringify(Object.keys(wanted).sort()),
  `the launch attributes are ${Object.keys(props).sort()}; expected ${Object.keys(wanted).sort()}`,
);
check(JSON.stringify(launch.required) === JSON.stringify(["program"]), "`program` is the one required attribute");
const adapter = readFileSync(join(here, "../../crates/colibri-dap/src/server.rs"), "utf8");
for (const name of Object.keys(props)) {
  check(adapter.includes(`"${name}"`), `the adapter never reads \`${name}\``);
}

for (const c of dbg.initialConfigurations ?? []) {
  check(c.type === "colibri" && c.request === "launch" && c.program, "an initial configuration is a colibri launch with `program`");
}
check((dbg.initialConfigurations ?? []).length > 0, "there is an initial configuration");
const snippets = dbg.configurationSnippets ?? [];
check(snippets.length >= 3, "there are launch configuration snippets");
for (const s of snippets) {
  check(typeof s.label === "string" && typeof s.description === "string", "a snippet has a label and a description");
  check(s.body?.type === "colibri" && s.body?.request === "launch", `snippet ${s.label}: not a colibri launch`);
  for (const key of Object.keys(s.body ?? {})) {
    check(["type", "request", "name", ...Object.keys(wanted)].includes(key), `snippet ${s.label}: unknown attribute ${key}`);
  }
}

// The setting for the executable serves the language server and the debugger.
check(contributes.configuration?.properties?.["colibri.server.path"] !== undefined, "no `colibri.server.path` setting");
const code = readFileSync(join(here, "extension.js"), "utf8");
check(code.includes("server.path") && code.includes('["debug"]'), "extension.js must start `colibri debug` from `colibri.server.path`");
try {
  execFileSync(process.execPath, ["--check", join(here, "extension.js")], { stdio: "pipe" });
} catch (e) {
  problems.push(`extension.js does not parse: ${e.stderr}`);
}

if (problems.length > 0) {
  console.error(problems.map((p) => `- ${p}`).join("\n"));
  process.exit(1);
}
console.log("editors/vscode/package.json: ok");
