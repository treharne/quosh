// Node smoke test for the @quosh/* wasm packages.
//
// Loads the CommonJS glue produced by `tools/build-wasm.sh` and exercises the
// same behaviour the unit tests cover natively, proving the generated
// bindings (not just the Rust) work.
const assert = require("node:assert");
const proto = require("../web/pkg/proto/node/quosh_proto_wasm.js");
const pred = require("../web/pkg/predict/node/quosh_predict_wasm.js");
const client = require("../web/pkg/client/node/quosh_client_wasm.js");

// -- @quosh/proto -------------------------------------------------------------
// Auth-hello framing: partial input yields nothing, then a complete frame.
const hello = proto.encodeAuthHello(
  new Uint8Array(32),
  new Uint8Array(16),
  new Uint8Array(32),
  80,
  24,
);
const fb = new proto.FrameBuffer();
fb.push(hello.subarray(0, 3));
assert.strictEqual(fb.take(), undefined, "incomplete frame must not be yielded");
fb.push(hello.subarray(3));
const frame = fb.take();
assert.strictEqual(frame.typ, 13, "MSG_AUTH_HELLO");
assert.strictEqual(frame.payload.length, 84, "auth hello payload size");

// Auth-fail decode (u16 length + utf8).
const reason = "nope";
const payload = new Uint8Array(2 + reason.length);
payload[0] = reason.length;
payload[1] = 0;
for (let i = 0; i < reason.length; i++) payload[2 + i] = reason.charCodeAt(i);
assert.strictEqual(proto.decodeAuthFail(payload), "nope");

// -- @quosh/predict -----------------------------------------------------------
const CELL = 12;
const enc = new TextEncoder();
function line(text, cursorCol) {
  const cols = 24;
  const cells = new Uint8Array(cols * CELL);
  [...text].forEach((ch, i) => {
    const bytes = enc.encode(ch);
    cells[i * CELL + 1] = bytes.length << 3;
    cells.set(bytes, i * CELL + 8);
  });
  return new pred.Frame(1, cols, 0, cursorCol, 1, "", cells);
}
const p = new pred.Predictor();
p.set_display_preference(true);
p.set_send_interval(250);
const confirmed = line("prompt> ", 8);
p.set_local_frame_sent(0);
const display = confirmed.clone_frame();
p.new_user_byte(0x61, display, 0);
p.apply(display);
assert.strictEqual(display.cells[8 * CELL + 8], 0, "first prediction stays hidden");
const echoed = line("prompt> a", 9);
p.set_local_frame_late_acked(1);
p.cull(echoed, 1);
p.set_local_frame_sent(1);
const shown = echoed.clone_frame();
p.new_user_byte(0x62, shown, 2);
p.apply(shown);
assert.strictEqual(shown.cells[9 * CELL + 8], 0x62, "next byte is predicted");

// -- @quosh/client ------------------------------------------------------------
const cl = new client.Client(new Uint8Array(16), new Uint8Array(32), 80, 24, false);
cl.begin_connection(0);
cl.pump();
const out = cl.outbound;
assert.ok(out.length > 0, "outbound must not be empty");
assert.strictEqual(out[4], 1, "first frame is MSG_HELLO");
assert.strictEqual(cl.hello_ok, false);
assert.strictEqual(cl.rows, 24);

console.log("wasm smoke: OK");