/**
 * Unit tests for `src/ipc/codec.ts`. Covers:
 *
 *  - Round-trip every {@link MessageType} through `encode` then `decode`.
 *  - Reject malformed frames: empty length, non-numeric length, oversized
 *    length prefix, oversized payload, missing trailing newline, partial
 *    frame, invalid UTF-8, invalid JSON, schema mismatch.
 *  - Decoder stitches frames split across chunk boundaries.
 *
 * The tests construct `ReadableStream<Uint8Array>` instances directly so
 * no socket I/O is needed.
 */

import { assertEquals, assertInstanceOf, assertRejects, assertThrows } from "@std/assert";

import { decode, encode, IpcCodecError } from "../../src/ipc/codec.ts";
import { type MessageEnvelope, parseEnvelope } from "../../src/ipc/protocol.ts";
import { MAX_IPC_FRAME_BYTES } from "../../src/constants.ts";
import { makeIssueNumber, makeRepoFullName } from "../../src/types.ts";

const textEncoder = new TextEncoder();

function streamFrom(...chunks: Uint8Array[]): ReadableStream<Uint8Array> {
  return new ReadableStream({
    start(controller) {
      for (const chunk of chunks) {
        controller.enqueue(chunk);
      }
      controller.close();
    },
  });
}

async function collect(
  source: ReadableStream<Uint8Array>,
): Promise<MessageEnvelope[]> {
  const out: MessageEnvelope[] = [];
  for await (const envelope of decode(source)) {
    out.push(envelope);
  }
  return out;
}

const sampleEnvelopes: readonly MessageEnvelope[] = [
  { id: "1", type: "ping", payload: {} },
  {
    id: "2",
    type: "subscribe",
    payload: { target: "*" },
  },
  {
    id: "3",
    type: "unsubscribe",
    payload: { subscriptionId: "2" },
  },
  {
    id: "4",
    type: "command",
    payload: { name: "issue", args: ["42"] },
  },
  {
    id: "4b",
    type: "command",
    payload: {
      name: "issue",
      args: ["42"],
      // Exercises the branded transform in `repoFullNameSchema` and
      // `issueNumberSchema`.
      repo: makeRepoFullName("owner/repo"),
      issueNumber: makeIssueNumber(42),
    },
  },
  {
    id: "5",
    type: "prompt",
    payload: { taskId: "task_abc", text: "please continue" },
  },
  {
    id: "6",
    type: "pong",
    payload: { daemonVersion: "0.0.0-dev" },
  },
  {
    id: "7",
    type: "ack",
    payload: { ok: true },
  },
  {
    id: "8",
    type: "ack",
    payload: { ok: false, error: "unimplemented" },
  },
  {
    id: "9",
    type: "event",
    payload: {
      taskId: "task_abc",
      atIso: "2026-04-26T12:00:00.000Z",
      kind: "state-changed",
      data: {
        fromState: "INIT",
        toState: "CLONING_WORKTREE",
        reason: "starting",
      },
    },
  },
  {
    id: "10",
    type: "event",
    payload: {
      taskId: "task_abc",
      atIso: "2026-04-26T12:00:01.000Z",
      kind: "log",
      data: { level: "info", message: "hello" },
    },
  },
  {
    id: "11",
    type: "event",
    payload: {
      taskId: "task_abc",
      atIso: "2026-04-26T12:00:02.000Z",
      kind: "agent-message",
      data: { role: "assistant", text: "working" },
    },
  },
  {
    id: "12",
    type: "event",
    payload: {
      taskId: "task_abc",
      atIso: "2026-04-26T12:00:03.000Z",
      kind: "github-call",
      data: {
        method: "GET",
        endpoint: "/repos/x/y/issues/1",
        statusCode: 200,
        durationMilliseconds: 123,
      },
    },
  },
  {
    // Regression: `github-call` events must round-trip the optional
    // `error` field that `GitHubCallPayload` carries (set by the
    // stabilize-CI fetcher and `dispatchCiAgent` when a single GitHub
    // call rejects). The schema is `.strict()`, so an unknown `error`
    // field used to be rejected at the IPC seam.
    id: "12b",
    type: "event",
    payload: {
      taskId: "task_abc",
      atIso: "2026-04-26T12:00:03.500Z",
      kind: "github-call",
      data: {
        method: "GET",
        endpoint: "/repos/x/y/check-runs/99/logs",
        error: "404 Not Found",
      },
    },
  },
  {
    id: "13",
    type: "event",
    payload: {
      taskId: "task_abc",
      atIso: "2026-04-26T12:00:04.000Z",
      kind: "error",
      data: { message: "boom", detail: "stack" },
    },
  },
];

Deno.test("encode/decode round-trips every message type", async () => {
  for (const envelope of sampleEnvelopes) {
    const bytes = encode(envelope);
    const decoded = await collect(streamFrom(bytes));
    assertEquals(decoded.length, 1);
    assertEquals(decoded[0], envelope);
  }
});

Deno.test("decoder concatenates multiple framed envelopes in one stream", async () => {
  const merged = mergeBytes(sampleEnvelopes.map(encode));
  const decoded = await collect(streamFrom(merged));
  assertEquals(decoded.length, sampleEnvelopes.length);
  for (let index = 0; index < sampleEnvelopes.length; index += 1) {
    assertEquals(decoded[index], sampleEnvelopes[index]);
  }
});

Deno.test("decoder stitches frames split across chunk boundaries", async () => {
  const envelope = sampleEnvelopes[0];
  if (envelope === undefined) {
    throw new Error("test fixture sampleEnvelopes is empty");
  }
  const bytes = encode(envelope);
  // Split at every byte boundary to make the smallest interesting case.
  const chunks: Uint8Array[] = [];
  for (let index = 0; index < bytes.byteLength; index += 1) {
    chunks.push(bytes.slice(index, index + 1));
  }
  const decoded = await collect(streamFrom(...chunks));
  assertEquals(decoded.length, 1);
  assertEquals(decoded[0], envelope);
});

Deno.test("encode rejects an envelope that fails schema validation", () => {
  const invalid = { id: "", type: "ping", payload: {} } as unknown as MessageEnvelope;
  const error = assertThrows(() => encode(invalid));
  assertInstanceOf(error, IpcCodecError);
});

Deno.test("encode rejects payloads larger than MAX_IPC_FRAME_BYTES", () => {
  // Construct a syntactically valid envelope with an oversize payload by
  // hand-encoding past the encoder's check; we pad the `text` field of a
  // prompt up to the limit + 1.
  const padding = "x".repeat(MAX_IPC_FRAME_BYTES);
  const envelope = {
    id: "1",
    type: "prompt",
    payload: { taskId: "t", text: padding },
  } as const;
  const error = assertThrows(() => encode(envelope satisfies MessageEnvelope));
  assertInstanceOf(error, IpcCodecError);
});

Deno.test("decoder rejects a frame whose length prefix is empty", async () => {
  // Stream begins with a leading newline.
  const error = await assertRejects(() => collect(streamFrom(textEncoder.encode("\n"))));
  assertInstanceOf(error, IpcCodecError);
});

Deno.test("decoder rejects a non-numeric length prefix", async () => {
  const bytes = textEncoder.encode("abc\n{}\n");
  const error = await assertRejects(() => collect(streamFrom(bytes)));
  assertInstanceOf(error, IpcCodecError);
});

Deno.test("decoder rejects a length prefix exceeding MAX_IPC_LENGTH_PREFIX_DIGITS", async () => {
  // 9 digits.
  const bytes = textEncoder.encode("123456789\n{}\n");
  const error = await assertRejects(() => collect(streamFrom(bytes)));
  assertInstanceOf(error, IpcCodecError);
});

Deno.test("decoder rejects a frame that claims more than MAX_IPC_FRAME_BYTES", async () => {
  // Length prefix 9 999 999 (well above MAX_IPC_FRAME_BYTES) but the codec
  // only consumes the prefix to make this judgement.
  const bytes = textEncoder.encode("9999999\n");
  const error = await assertRejects(() => collect(streamFrom(bytes)));
  assertInstanceOf(error, IpcCodecError);
});

Deno.test("decoder rejects a frame missing its trailing newline", async () => {
  // Encode a valid 27-byte payload but omit the trailing newline.
  const payload = JSON.stringify({ id: "1", type: "ping", payload: {} });
  const payloadBytes = textEncoder.encode(payload);
  const bytes = mergeBytes([
    textEncoder.encode(`${payloadBytes.byteLength}\n`),
    payloadBytes,
    // Wrong trailer byte.
    new Uint8Array([0x20]),
  ]);
  const error = await assertRejects(() => collect(streamFrom(bytes)));
  assertInstanceOf(error, IpcCodecError);
});

Deno.test("decoder rejects a partial frame at end-of-stream", async () => {
  // Promise an 80-byte payload but supply only ten bytes.
  const bytes = textEncoder.encode("80\n0123456789");
  const error = await assertRejects(() => collect(streamFrom(bytes)));
  assertInstanceOf(error, IpcCodecError);
});

Deno.test("decoder rejects a frame whose payload is invalid JSON", async () => {
  const payload = textEncoder.encode("not-json");
  const bytes = mergeBytes([
    textEncoder.encode(`${payload.byteLength}\n`),
    payload,
    textEncoder.encode("\n"),
  ]);
  const error = await assertRejects(() => collect(streamFrom(bytes)));
  assertInstanceOf(error, IpcCodecError);
});

Deno.test("decoder rejects a frame whose JSON does not match the envelope schema", async () => {
  // Valid JSON, wrong shape (missing `payload`).
  const payload = textEncoder.encode(JSON.stringify({ id: "1", type: "ping" }));
  const bytes = mergeBytes([
    textEncoder.encode(`${payload.byteLength}\n`),
    payload,
    textEncoder.encode("\n"),
  ]);
  const error = await assertRejects(() => collect(streamFrom(bytes)));
  assertInstanceOf(error, IpcCodecError);
});

Deno.test("decoder rejects a frame with invalid UTF-8 in the payload", async () => {
  // 0xFF is never a valid UTF-8 continuation byte and never a leading byte.
  const payload = new Uint8Array([0xFF, 0xFE]);
  const bytes = mergeBytes([
    textEncoder.encode(`${payload.byteLength}\n`),
    payload,
    textEncoder.encode("\n"),
  ]);
  const error = await assertRejects(() => collect(streamFrom(bytes)));
  assertInstanceOf(error, IpcCodecError);
});

Deno.test("decoder rejects a length prefix that is zero or negative", async () => {
  const error = await assertRejects(() => collect(streamFrom(textEncoder.encode("0\n\n"))));
  assertInstanceOf(error, IpcCodecError);
});

Deno.test("decoder rejects an oversize length prefix when split across chunks", async () => {
  // The framer must enforce the digit cap even before a newline arrives,
  // otherwise a peer could exhaust memory by streaming digits indefinitely.
  const tooMany = textEncoder.encode("123456789");
  const error = await assertRejects(() => collect(streamFrom(tooMany)));
  assertInstanceOf(error, IpcCodecError);
});

Deno.test("decoder yields nothing for an empty stream", async () => {
  const decoded = await collect(streamFrom());
  assertEquals(decoded.length, 0);
});

Deno.test("parseEnvelope rejects a command payload with malformed repo", () => {
  const result = parseEnvelope({
    id: "1",
    type: "command",
    payload: { name: "issue", args: [], repo: "no-slash" },
  });
  assertEquals(result.success, false);
});

Deno.test("parseEnvelope rejects a command payload with non-positive issueNumber", () => {
  const result = parseEnvelope({
    id: "1",
    type: "command",
    payload: { name: "issue", args: [], issueNumber: 0 },
  });
  assertEquals(result.success, false);
});

function mergeBytes(parts: readonly Uint8Array[]): Uint8Array {
  const total = parts.reduce((sum, part) => sum + part.byteLength, 0);
  const out = new Uint8Array(total);
  let offset = 0;
  for (const part of parts) {
    out.set(part, offset);
    offset += part.byteLength;
  }
  return out;
}
