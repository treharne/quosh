// Wire constants mirrored from `quosh-proto`. Kept in one place so the
// TypeScript and Rust sides are easy to compare.

export const PROTOCOL_VERSION = 2;
export const WT_PATH = "/quosh";

export const MSG_HELLO = 1;
export const MSG_HELLO_OK = 2;
export const MSG_INPUT = 3;
export const MSG_RESIZE = 4;
export const MSG_HANGUP = 5;
export const MSG_ACK_STATE = 6;
export const MSG_INPUT_ACK = 7;
export const MSG_SCREEN = 8;
export const MSG_EXIT = 9;
export const MSG_ERROR = 10;
export const MSG_PING = 11;
export const MSG_PONG = 12;

export const MSG_AUTH_HELLO = 13;
export const MSG_CHALLENGE = 14;
export const MSG_ENROLL = 15;
export const MSG_ASSERT = 16;
export const MSG_AUTH_OK = 17;
export const MSG_AUTH_FAIL = 18;

export const MODE_CURSOR_VISIBLE = 1;
export const MODE_BRACKETED_PASTE = 1 << 3;

/** Seconds of silence before the outage banner shows. */
export const OUTAGE_BANNER_SECS = 3;