/**
 * ipc/codec.ts — length-prefixed framing for IPC envelopes on the
 * Unix-domain socket.
 *
 * **Frame format.** Each frame is the ASCII decimal byte length of the JSON
 * payload, then `'\n'` (`0x0A`), then exactly that many bytes of UTF-8
 * JSON, then a terminating `'\n'`. Example:
 *
 * ```text
 * 28\n{"id":"1","type":"ping",...}\n
 * ```
 *
 * The trailing newline is **not** counted in the length; it makes the
 * stream readable to a human inspecting it with `socat`/`cat` and gives
 * the decoder a cheap recovery boundary if a length is corrupt.
 *
 * **Why both length-prefix and NDJSON-style newline.** Length-prefix
 * eliminates the need to scan for delimiters inside JSON strings; the
 * trailing newline makes byte streams self-delimiting for ad-hoc tooling
 * and gives a sanity check (a missing terminator is treated as a
 * malformed frame). The cost is one extra byte per frame.
 *
 * **Bounds.** A frame is rejected if the length prefix:
 *  - exceeds {@link MAX_IPC_LENGTH_PREFIX_DIGITS} digits;
 *  - is not parseable as a non-negative decimal integer;
 *  - claims more than {@link MAX_IPC_FRAME_BYTES} bytes of payload;
 *  - is not followed by `'\n'`;
 *  - or is not followed by a `'\n'` after the declared payload length.
 *
 * Schema-level validation happens **after** the codec hands a parsed JSON
 * value to {@link parseEnvelope}; this module knows nothing about
 * envelopes other than their byte size.
 *
 * @module
 */

import { MAX_IPC_FRAME_BYTES, MAX_IPC_LENGTH_PREFIX_DIGITS } from "../constants.ts";
import { type MessageEnvelope, parseEnvelope } from "./protocol.ts";

/**
 * Error thrown by the decoder for any malformed frame. Carries a category
 * tag so callers can distinguish recoverable framing problems from
 * unrecoverable schema problems.
 */
export class IpcCodecError extends Error {
  /**
   * Construct a codec error.
   *
   * @param message Human-readable description of the failure.
   * @param cause Underlying error or zod issue, when applicable.
   */
  constructor(
    message: string,
    /** Underlying cause, when applicable. */
    public override readonly cause?: unknown,
  ) {
    super(message);
    this.name = "IpcCodecError";
  }
}

const NEWLINE_CODE_POINT = 0x0A;
const ZERO_CODE_POINT = 0x30;
const NINE_CODE_POINT = 0x39;
const RADIX_DECIMAL = 10;

const textEncoder = new TextEncoder();
const textDecoder = new TextDecoder("utf-8", { fatal: true });

/**
 * Encode a {@link MessageEnvelope} into a length-prefixed UTF-8 frame.
 *
 * The envelope is validated through {@link parseEnvelope} before
 * encoding so the encoder cannot emit a frame the decoder would refuse
 * (the round-trip property is symmetric by construction).
 *
 * @param envelope The envelope to encode.
 * @returns A `Uint8Array` containing the framed bytes.
 * @throws {IpcCodecError} If `envelope` fails schema validation, or if the
 *   serialized payload exceeds {@link MAX_IPC_FRAME_BYTES}.
 *
 * @example
 * ```ts
 * import { encode } from "./codec.ts";
 *
 * const bytes = encode({ id: "1", type: "ping", payload: {} });
 * // → Uint8Array(...)  "27\n{\"id\":\"1\",\"type\":\"ping\",\"payload\":{}}\n"
 * ```
 */
export function encode(envelope: MessageEnvelope): Uint8Array {
  const validation = parseEnvelope(envelope);
  if (!validation.success) {
    throw new IpcCodecError("envelope failed schema validation", validation.issues);
  }
  const json = JSON.stringify(validation.data);
  const payload = textEncoder.encode(json);
  if (payload.byteLength > MAX_IPC_FRAME_BYTES) {
    throw new IpcCodecError(
      `frame exceeds maximum size: ${payload.byteLength} > ${MAX_IPC_FRAME_BYTES}`,
    );
  }
  const prefix = textEncoder.encode(`${payload.byteLength}\n`);
  const trailer = textEncoder.encode("\n");
  const frame = new Uint8Array(prefix.byteLength + payload.byteLength + trailer.byteLength);
  frame.set(prefix, 0);
  frame.set(payload, prefix.byteLength);
  frame.set(trailer, prefix.byteLength + payload.byteLength);
  return frame;
}

/**
 * State machine for the streaming decoder. Holds the unconsumed tail of
 * the byte stream between calls so frames split across `chunk` boundaries
 * are stitched together correctly.
 */
class DecoderState {
  /**
   * Pending bytes that have not been fully consumed into a frame yet.
   * Initially empty; grows when a chunk supplies less than a complete
   * frame, shrinks when whole frames are extracted.
   */
  private buffer: Uint8Array = new Uint8Array(0);

  /**
   * Append `chunk` to the pending buffer.
   *
   * @param chunk Bytes pulled from the underlying transport.
   */
  push(chunk: Uint8Array): void {
    if (this.buffer.byteLength === 0) {
      this.buffer = chunk;
      return;
    }
    const merged = new Uint8Array(this.buffer.byteLength + chunk.byteLength);
    merged.set(this.buffer, 0);
    merged.set(chunk, this.buffer.byteLength);
    this.buffer = merged;
  }

  /**
   * Try to extract one frame from the head of the buffer. Returns
   * `undefined` when the buffer does not yet hold a complete frame.
   *
   * @returns A parsed {@link MessageEnvelope} or `undefined`.
   * @throws {IpcCodecError} On any framing or schema problem.
   */
  next(): MessageEnvelope | undefined {
    const view = this.buffer;
    const newlineIndex = indexOfNewline(view);
    if (newlineIndex === -1) {
      if (view.byteLength > MAX_IPC_LENGTH_PREFIX_DIGITS) {
        throw new IpcCodecError(
          `length prefix exceeds ${MAX_IPC_LENGTH_PREFIX_DIGITS} digits`,
        );
      }
      return undefined;
    }
    if (newlineIndex === 0) {
      throw new IpcCodecError("empty length prefix");
    }
    if (newlineIndex > MAX_IPC_LENGTH_PREFIX_DIGITS) {
      throw new IpcCodecError(
        `length prefix exceeds ${MAX_IPC_LENGTH_PREFIX_DIGITS} digits`,
      );
    }

    const prefix = view.subarray(0, newlineIndex);
    if (!isAllDigits(prefix)) {
      throw new IpcCodecError(
        `length prefix is not a non-negative integer: ${decodeAsciiSafe(prefix)}`,
      );
    }
    const declaredLength = Number.parseInt(decodeAsciiSafe(prefix), RADIX_DECIMAL);
    if (declaredLength > MAX_IPC_FRAME_BYTES) {
      throw new IpcCodecError(
        `declared length ${declaredLength} > ${MAX_IPC_FRAME_BYTES}`,
      );
    }
    if (declaredLength <= 0) {
      throw new IpcCodecError(`declared length must be positive, got ${declaredLength}`);
    }

    const payloadStart = newlineIndex + 1;
    const payloadEnd = payloadStart + declaredLength;
    const trailerEnd = payloadEnd + 1;
    if (view.byteLength < trailerEnd) {
      // Not enough bytes yet — wait for more.
      return undefined;
    }
    if (view[payloadEnd] !== NEWLINE_CODE_POINT) {
      throw new IpcCodecError(
        `frame missing trailing newline at byte ${payloadEnd}`,
      );
    }

    const payloadBytes = view.subarray(payloadStart, payloadEnd);
    let json: string;
    try {
      json = textDecoder.decode(payloadBytes);
    } catch (error) {
      throw new IpcCodecError("frame payload is not valid UTF-8", error);
    }
    let raw: unknown;
    try {
      raw = JSON.parse(json);
    } catch (error) {
      throw new IpcCodecError("frame payload is not valid JSON", error);
    }
    const validation = parseEnvelope(raw);
    if (!validation.success) {
      throw new IpcCodecError("frame payload failed schema validation", validation.issues);
    }

    // Drop the consumed bytes (prefix + newline + payload + trailing newline).
    this.buffer = view.subarray(trailerEnd);
    return validation.data;
  }

  /**
   * Whether the buffer holds any bytes that have not been consumed into
   * a frame yet.
   *
   * @returns `true` iff there is at least one buffered byte.
   */
  hasPendingBytes(): boolean {
    return this.buffer.byteLength > 0;
  }
}

/**
 * Decode a `ReadableStream<Uint8Array>` into an async iterable of
 * {@link MessageEnvelope} values.
 *
 * The decoder tolerates arbitrary chunk boundaries: a single frame can
 * arrive split across many chunks. When the stream ends mid-frame the
 * decoder throws {@link IpcCodecError} instead of swallowing the partial
 * frame silently — partial frames are evidence of either a buggy peer or
 * a transport disconnect, and either case must be visible to the caller.
 *
 * @param stream The raw byte stream from the underlying transport (a
 *   `Deno.Conn.readable` in production; a synthetic stream in tests).
 * @returns An async iterable that yields validated envelopes.
 *
 * @example
 * ```ts
 * import { decode } from "./codec.ts";
 *
 * for await (const envelope of decode(connection.readable)) {
 *   handle(envelope);
 * }
 * ```
 */
export async function* decode(
  stream: ReadableStream<Uint8Array>,
): AsyncIterable<MessageEnvelope> {
  const state = new DecoderState();
  const reader = stream.getReader();
  try {
    while (true) {
      const { value, done } = await reader.read();
      if (value !== undefined && value.byteLength > 0) {
        state.push(value);
        while (true) {
          const envelope = state.next();
          if (envelope === undefined) {
            break;
          }
          yield envelope;
        }
      }
      if (done) {
        // Any bytes still buffered at end-of-stream are a partial frame.
        // Either the peer disconnected mid-write or it sent a malformed
        // length prefix — both cases must be visible to the caller, so
        // surface them rather than silently swallow.
        if (state.hasPendingBytes()) {
          throw new IpcCodecError("stream ended mid-frame");
        }
        return;
      }
    }
  } finally {
    reader.releaseLock();
  }
}

/**
 * Find the byte offset of the first `'\n'` in `view`, or `-1` if absent.
 */
function indexOfNewline(view: Uint8Array): number {
  for (let index = 0; index < view.byteLength; index += 1) {
    if (view[index] === NEWLINE_CODE_POINT) {
      return index;
    }
  }
  return -1;
}

/**
 * Return `true` iff every byte in `view` is an ASCII decimal digit.
 */
function isAllDigits(view: Uint8Array): boolean {
  if (view.byteLength === 0) {
    return false;
  }
  for (let index = 0; index < view.byteLength; index += 1) {
    const byte = view[index];
    if (byte === undefined || byte < ZERO_CODE_POINT || byte > NINE_CODE_POINT) {
      return false;
    }
  }
  return true;
}

/**
 * Decode an ASCII-only byte slice to a string for diagnostic messages.
 * Falls back to the byte length if the slice contains non-ASCII data, so
 * exception messages do not themselves throw.
 */
function decodeAsciiSafe(view: Uint8Array): string {
  try {
    return new TextDecoder("ascii", { fatal: true }).decode(view);
  } catch {
    return `<${view.byteLength} non-ASCII bytes>`;
  }
}
