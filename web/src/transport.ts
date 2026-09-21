// Browser WebTransport adapter.
//
// The trust model is `serverCertificateHashes`: we pin the SHA-256 of the
// server's current (and forward) certificates, so no public CA or DNS name is
// needed. Any change to the transport should stay behind this module: the
// session logic only sees byte streams.

import { WT_PATH } from "./constants.js";

export interface Control {
  send: WritableStreamDefaultWriter<Uint8Array>;
  recv: ReadableStreamDefaultReader<Uint8Array>;
}

export class Connection {
  private constructor(private readonly wt: WebTransport) {}

  static async connect(url: string, hashes: Uint8Array[]): Promise<Connection> {
    if (hashes.length === 0) throw new Error("no pinned certificate hashes");
    const wt = new WebTransport(url, {
      serverCertificateHashes: hashes.map((value) => ({
        algorithm: "sha-256",
        value: value.buffer.slice(value.byteOffset, value.byteOffset + value.byteLength) as ArrayBuffer,
      })),
    });
    await wt.ready;
    return new Connection(wt);
  }

  async openControl(): Promise<Control> {
    const stream = await this.wt.createBidirectionalStream();
    return {
      send: stream.writable.getWriter(),
      recv: stream.readable.getReader(),
    };
  }

  get datagrams(): ReadableStream<Uint8Array> {
    return this.wt.datagrams.readable;
  }

  close(): void {
    try {
      this.wt.close();
    } catch {
      // Already closing.
    }
  }

  closed(): Promise<void> {
    return this.wt.closed.then(
      () => undefined,
      () => undefined,
    );
  }
}

export function serverUrl(host: string, port: number): string {
  return `https://${host}:${port}${WT_PATH}`;
}