// Unit tests of the pieces of colibri.js that the programs of the harness cannot reach.
//   node unit.mjs

import assert from "node:assert/strict";
import { decoderFor, MAX_TIMER_MS } from "../colibri.js";

const collect = () => {
  const parts = [];
  return { parts, write: decoderFor((text) => parts.push(text)) };
};

// A character split between two writes comes out whole, once.
{
  const { parts, write } = collect();
  write(new Uint8Array([0x61, 0xc3]));
  write(new Uint8Array([0xa9, 0x62]));
  write.flush();
  assert.equal(parts.join(""), "aéb");
}

// A character cut off at the end of the stream is not lost silently.
{
  const { parts, write } = collect();
  write(new Uint8Array([0x61, 0xe2, 0x82]));
  assert.equal(parts.join(""), "a");
  write.flush();
  assert.equal(parts.join(""), "a�");
}

assert.equal(MAX_TIMER_MS, 2147483647);
console.log("colibri.js unit tests passed");
