// Durable server list (IndexedDB) and tab-scoped session (sessionStorage).
//
// The certificate-hash set is a public pin and the auth token is a bearer
// credential, so both live in IndexedDB and never in a cookie. The Quosh
// session id/token are tab-scoped by design: a new tab or a cold browser
// session starts a new session.

export interface ServerEntry {
  key: string;
  host: string;
  port: number;
  /** Display name `<unix_user>@<host>`. */
  user: string;
  /** Pinned certificate hashes, hex. */
  hashes: string[];
  /** Rotating session token, hex. */
  authToken?: string;
  /** WebAuthn credential id, base64url. */
  credentialId?: string;
}

export interface TabSession {
  sessionId: string; // hex
  sessionToken: string; // hex
}

const DB = "quosh";
const STORE = "servers";
const VERSION = 1;

function open(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(DB, VERSION);
    req.onupgradeneeded = () => {
      if (!req.result.objectStoreNames.contains(STORE)) {
        req.result.createObjectStore(STORE, { keyPath: "key" });
      }
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error ?? new Error("indexedDB open failed"));
  });
}

function request<T>(
  mode: IDBTransactionMode,
  fn: (store: IDBObjectStore) => IDBRequest,
): Promise<T> {
  return new Promise((resolve, reject) => {
    open().then((db) => {
      const t = db.transaction(STORE, mode);
      const req = fn(t.objectStore(STORE));
      req.onsuccess = () => resolve(req.result as T);
      req.onerror = () => reject(req.error ?? new Error("indexedDB request failed"));
    }, reject);
  });
}

export async function listServers(): Promise<ServerEntry[]> {
  const all = await request<ServerEntry[]>("readonly", (s) => s.getAll());
  return all.sort((a, b) => a.key.localeCompare(b.key));
}

export function getServer(key: string): Promise<ServerEntry | undefined> {
  return request<ServerEntry | undefined>("readonly", (s) => s.get(key));
}

export async function putServer(entry: ServerEntry): Promise<void> {
  await request("readwrite", (s) => s.put(entry));
}

export async function forgetServer(key: string): Promise<void> {
  await request("readwrite", (s) => s.delete(key));
  clearTabSession(key);
}

const tabKey = (key: string) => `quosh.session.${key}`;

export function getTabSession(key: string): TabSession | undefined {
  try {
    const raw = sessionStorage.getItem(tabKey(key));
    return raw ? (JSON.parse(raw) as TabSession) : undefined;
  } catch {
    return undefined;
  }
}

export function setTabSession(key: string, session: TabSession): void {
  try {
    sessionStorage.setItem(tabKey(key), JSON.stringify(session));
  } catch {
    // Private mode with storage disabled: reconnect will just create a session.
  }
}

export function clearTabSession(key: string): void {
  try {
    sessionStorage.removeItem(tabKey(key));
  } catch {
    // Ignore.
  }
}