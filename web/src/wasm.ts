// Loads and re-exports the three @quosh wasm modules. Everything that touches
// wasm goes through here so initialisation happens once.

import initProto from "../pkg/proto/quosh_proto_wasm.js";
import * as proto from "../pkg/proto/quosh_proto_wasm.js";
import initPredict from "../pkg/predict/quosh_predict_wasm.js";
import * as predict from "../pkg/predict/quosh_predict_wasm.js";
import initClient from "../pkg/client/quosh_client_wasm.js";
import * as client from "../pkg/client/quosh_client_wasm.js";

let ready: Promise<void> | null = null;

export function initWasm(): Promise<void> {
  ready ??= (async () => {
    await Promise.all([initProto(), initPredict(), initClient()]);
  })();
  return ready;
}

export { proto, predict, client };
export type Client = client.Client;
export type FrameView = client.FrameView;
export type ScreenView = proto.ScreenView;