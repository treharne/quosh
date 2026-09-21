// The session loop: authenticate, then drive the wasm client over one
// WebTransport connection. Reconnects reuse the pinned hashes and the rotating
// auth token, so a healthy path reconnects silently.

import {
  AuthRejected,
  NeedsPasskey,
  assertionCeremony,
  enrollmentCeremony,
  runAuth,
  type Ceremony,
  type TabTokens,
} from "./auth.js";
import { bytesToHex, hexToBytes } from "./enroll.js";
import {
  getTabSession,
  putServer,
  setTabSession,
  type ServerEntry,
} from "./store.js";
import { Connection, serverUrl, type Control } from "./transport.js";
import { client as wasmClient, type Client, type FrameView } from "./wasm.js";

export type Status = "connecting" | "online" | "offline" | "ended";

export interface SessionOptions {
  entry: ServerEntry;
  cols: number;
  rows: number;
  predictNever: boolean;
  /** Present only while completing an enrolment link. */
  enrollNonce?: Uint8Array;
  onFrame(frame: FrameView): void;
  onStatus(status: Status, detail?: string): void;
  onExit(code: number): void;
}

export const sleep = (ms: number): Promise<void> =>
  new Promise((resolve) => setTimeout(resolve, ms));

const now = (): number => performance.now();

interface Link {
  client: Client;
  control: Control;
  datagrams: ReadableStream<Uint8Array>;
  closed: Promise<void>;
}

export class Session {
  private client: Client | null = null;
  private clientSessionId: string | null = null;
  private control: Control | null = null;
  private conn: Connection | null = null;
  private running = false;
  /** Bumped to cancel the loops of the previous connection. */
  private gen = 0;

  constructor(private readonly opts: SessionOptions) {}

  /** Runs until the session ends or `stop()` is called. */
  async start(): Promise<void> {
    if (this.running) return;
    this.running = true;
    while (this.running) {
      let link: Link;
      try {
        this.opts.onStatus("connecting");
        link = await this.establish();
        this.opts.onStatus("online");
      } catch (e) {
        if (e instanceof AuthRejected || e instanceof NeedsPasskey) {
          this.opts.onStatus("ended", e.message);
          this.running = false;
          return;
        }
        this.opts.onStatus("offline", (e as Error).message);
        await sleep(1500);
        continue;
      }
      const result = await this.runLinked(link);
      if (result === "hungup") {
        this.opts.onStatus("ended");
        this.running = false;
        this.opts.onExit(link.client.exit_code);
        return;
      }
      this.opts.onStatus("offline");
      await sleep(1000);
    }
  }

  stop(): void {
    this.running = false;
    this.gen++;
    this.cancelLink();
  }

  input(bytes: Uint8Array): void {
    if (!this.client) return;
    this.client.queue_input(bytes, now());
    this.flush();
  }

  resize(cols: number, rows: number): void {
    this.opts.cols = cols;
    this.opts.rows = rows;
    this.client?.set_size(cols, rows);
    this.flush();
  }

  quit(): void {
    this.client?.request_hangup();
    this.flush();
  }

  get entry(): ServerEntry {
    return this.opts.entry;
  }

  private async establish(): Promise<Link> {
    const { entry, cols, rows } = this.opts;
    const hashes = entry.hashes.map((h) => hexToBytes(h));
    const conn = await Connection.connect(serverUrl(entry.host, entry.port), hashes);
    this.conn = conn;
    const control = await conn.openControl();
    this.control = control;

    const ceremony = this.ceremony();
    const auth = await runAuth(control, {
      authToken: entry.authToken ? hexToBytes(entry.authToken) : undefined,
      tab: this.tabTokens(),
      cols,
      rows,
      ceremony,
    });

    // Persist the rotated token and the refreshed pin set for this server.
    entry.authToken = bytesToHex(auth.authToken);
    entry.hashes = auth.hashes;
    await putServer(entry);
    const sessionId = bytesToHex(auth.sessionId);
    const sessionToken = bytesToHex(auth.sessionToken);
    setTabSession(entry.key, { sessionId, sessionToken });

    if (!this.client || this.clientSessionId !== sessionId) {
      this.client = new wasmClient.Client(
        auth.sessionId,
        auth.sessionToken,
        cols,
        rows,
        this.opts.predictNever,
      );
      this.clientSessionId = sessionId;
    }
    this.client.begin_connection(now());
    if (auth.remaining.length > 0) this.client.recv_control(auth.remaining, now());

    return {
      client: this.client,
      control,
      datagrams: conn.datagrams,
      closed: conn.closed(),
    };
  }

  private ceremony(): Ceremony | null {
    const { entry, enrollNonce } = this.opts;
    if (entry.credentialId) return assertionCeremony(entry.credentialId);
    if (enrollNonce) {
      return enrollmentCeremony(enrollNonce, entry.user, entry.user, (id) => {
        entry.credentialId = id;
      });
    }
    return null;
  }

  private tabTokens(): TabTokens {
    const t = getTabSession(this.opts.entry.key);
    if (!t) return {};
    try {
      return { sessionId: hexToBytes(t.sessionId), sessionToken: hexToBytes(t.sessionToken) };
    } catch {
      return {};
    }
  }

  private async runLinked(link: Link): Promise<"hungup" | "lost"> {
    const { client, control, datagrams, closed } = link;
    const gen = this.gen;
    const paint = () => {
      const frame = client.display();
      if (frame) this.opts.onFrame(frame);
    };

    this.flush();
    paint();

    const controlLoop = (async (): Promise<"lost" | null> => {
      for (;;) {
        if (gen !== this.gen) return null;
        const { value, done } = await control.recv.read();
        if (done || !value) return "lost";
        client.recv_control(value, now());
        this.flush();
        paint();
      }
    })().catch(() => "lost" as const);

    const datagramLoop = (async () => {
      const reader = datagrams.getReader();
      for (;;) {
        if (gen !== this.gen) return;
        const { value, done } = await reader.read();
        if (done) return;
        client.recv_datagram(value, now());
      }
    })().catch(() => undefined);

    const tickLoop = (async (): Promise<"hungup" | "lost" | null> => {
      for (;;) {
        if (gen !== this.gen) return null;
        const delay = client.tick_delay_ms ?? 1000;
        await sleep(Math.max(20, Math.min(1000, delay)));
        if (gen !== this.gen) return null;
        const alive = client.tick(now());
        this.flush();
        paint();
        if (!alive) return "lost";
        if (client.is_hungup) return "hungup";
      }
    })().catch(() => "lost" as const);

    const result = await Promise.race([
      controlLoop,
      tickLoop,
      closed.then(() => "lost" as const),
    ]);

    this.gen++;
    this.cancelLink();
    void datagramLoop;
    return result ?? "lost";
  }

  private cancelLink(): void {
    try {
      void this.control?.recv.cancel();
    } catch {
      // Already closed.
    }
    try {
      void this.control?.send.close();
    } catch {
      // Already closed.
    }
    this.conn?.close();
    this.conn = null;
    this.control = null;
  }

  /** Drain the client's outbound queue onto the control stream. */
  private flush(): void {
    const client = this.client;
    const control = this.control;
    if (!client || !control) return;
    client.pump();
    while (client.writing) {
      const out = client.outbound;
      if (out.length === 0) break;
      // `write` queues in call order; not awaiting keeps this synchronous so
      // `outbound`/`advance_outbound` stay atomic. The client's unacked-input
      // cap bounds how much can back up.
      control.send.write(out);
      client.advance_outbound(out.length);
    }
  }
}