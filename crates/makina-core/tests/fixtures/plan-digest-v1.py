#!/usr/bin/env python3
"""Inert Plan digest v1 conformance fixture.

This test-only reference accepts typed records, not JSON bytes. Callers must
decide canonical field order before passing records here.
"""
from __future__ import annotations

import hashlib
import struct
from collections.abc import Iterable

SOURCE_DOMAIN = b"makina.source-digest.v1\0"
EXECUTABLE_DOMAIN = b"makina.executable-digest.v1\0"


def normalize_markdown(value: str) -> bytes:
    return value.replace("\r\n", "\n").encode("utf-8")


def frame(tag: str, value: bytes | str) -> bytes:
    tag_bytes = tag.encode("utf-8")
    value_bytes = value.encode("utf-8") if isinstance(value, str) else value
    return struct.pack(">I", len(tag_bytes)) + tag_bytes + struct.pack(">Q", len(value_bytes)) + value_bytes


def frame_list(tag: str, values: Iterable[bytes | str]) -> bytes:
    items = list(values)
    return frame(tag, struct.pack(">Q", len(items)) + b"".join(frame("item", item) for item in items))


def digest(domain: bytes, records: Iterable[tuple[str, bytes | str]]) -> str:
    if domain not in (SOURCE_DOMAIN, EXECUTABLE_DOMAIN):
        raise ValueError("unknown plan digest domain")
    h = hashlib.sha256(domain)
    for tag, value in records:
        h.update(frame(tag, value))
    return h.hexdigest()


def source_digest(records: Iterable[tuple[str, bytes | str]]) -> str:
    return digest(SOURCE_DOMAIN, records)


def executable_digest(records: Iterable[tuple[str, bytes | str]]) -> str:
    return digest(EXECUTABLE_DOMAIN, records)
