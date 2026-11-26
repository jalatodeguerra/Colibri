// Unit tests of the pieces of colibri.js that the programs of the harness cannot reach.
//   node unit.mjs

import assert from "node:assert/strict";
import { decoderFor, defaultUtcOffset, MAX_TIMER_MS } from "../colibri.js";

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

// The page's time zone, in seconds east of UTC, asked for the instant (daylight saving changes it).
{
  const saved = process.env.TZ;
  const january = Date.UTC(2024, 0, 15) / 1000;
  const july = Date.UTC(2024, 6, 15) / 1000;
  process.env.TZ = "Asia/Kolkata";
  assert.equal(defaultUtcOffset(january), 19_800);
  process.env.TZ = "America/New_York";
  assert.equal(defaultUtcOffset(january), -18_000);
  assert.equal(defaultUtcOffset(july), -14_400);
  process.env.TZ = "UTC";
  assert.equal(defaultUtcOffset(january), 0);
  assert.ok(Number.isNaN(defaultUtcOffset(Number.NaN)));
  if (saved === undefined) delete process.env.TZ;
  else process.env.TZ = saved;
}

console.log("colibri.js unit tests passed");
