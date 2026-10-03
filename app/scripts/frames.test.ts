// node --experimental-strip-types scripts/frames.test.ts
import assert from "node:assert/strict";
import { takeFrames } from "../src/frames.ts";

const enc = (s: string) => new TextEncoder().encode(s);
const frame = (body: string) => `Content-Length: ${enc(body).length}\r\n\r\n${body}`;

// Login-file noise (with non-ASCII), then two frames, one with multi-byte text.
const a = '{"id":1,"result":"héllo ✓"}';
const b = '{"id":2}';
const all = enc(`Welcome — module loaded\n${frame(a)}${frame(b)}`);

// Every split point: bodies come out whole and in order.
for (let cut = 0; cut <= all.length; cut++) {
  const first = takeFrames(all.slice(0, cut));
  const rest = new Uint8Array([...first.rest, ...all.slice(cut)]);
  const second = takeFrames(rest);
  assert.deepEqual([...first.bodies, ...second.bodies], [a, b], `cut at ${cut}`);
  assert.equal(second.rest.length, 0);
}
console.log("frames ok");
