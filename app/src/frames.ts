// Language server messages, as they arrive on a stream: frames.test.ts checks it.

/**
 * Split complete frames off `buf`: a `Content-Length: N` header, a blank
 * line, then N bytes of JSON. Anything before a header is login-file noise.
 */
export function takeFrames(buf: Uint8Array): { bodies: string[]; rest: Uint8Array } {
  const dec = new TextDecoder();
  const bodies: string[] = [];
  for (;;) {
    // latin1 keeps one character per byte, so indexes are byte offsets.
    const head = new TextDecoder("latin1").decode(buf.subarray(0, Math.min(buf.length, 4096)));
    const at = head.indexOf("Content-Length:");
    const end = head.indexOf("\r\n\r\n", at);
    if (at < 0 || end < 0) return { bodies, rest: buf };
    const len = Number(/Content-Length:\s*(\d+)/.exec(head.slice(at))![1]);
    const start = end + 4;
    if (buf.length < start + len) return { bodies, rest: buf };
    bodies.push(dec.decode(buf.subarray(start, start + len)));
    buf = buf.slice(start + len);
  }
}
