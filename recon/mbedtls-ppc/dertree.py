#!/usr/bin/env python3
"""Minimal DER tree: parse, mutate, re-serialize with correct enclosing lengths.

The first corpus pass spliced bytes in place and every case failed at the outer SEQUENCE
because the parent lengths were never recomputed. This module keeps the tree so a mutation
anywhere re-encodes every ancestor.

A node may carry `len_override` (bytes) to declare a deliberately wrong length, which is how
the length-mismatch family is built.
"""
import copy


class Node:
    __slots__ = ("tag", "value", "len_override")

    def __init__(self, tag, value, len_override=None):
        self.tag = tag
        self.value = value          # bytes for primitive, list[Node] for constructed
        self.len_override = len_override

    @property
    def constructed(self):
        return isinstance(self.value, list)

    def clone(self):
        n = Node(self.tag, None)
        n.value = [c.clone() for c in self.value] if self.constructed else self.value
        n.len_override = self.len_override
        return n

    def walk(self, depth=0):
        yield self, depth
        if self.constructed:
            for c in self.value:
                yield from c.walk(depth + 1)

    def body(self):
        if self.constructed:
            return b"".join(c.encode() for c in self.value)
        return self.value

    def encode(self):
        body = self.body()
        tag = self.tag if isinstance(self.tag, bytes) else bytes([self.tag])
        if self.len_override is not None:
            return tag + self.len_override + body
        n = len(body)
        if n < 0x80:
            lb = bytes([n])
        elif n <= 0xFF:
            lb = bytes([0x81, n])
        elif n <= 0xFFFF:
            lb = bytes([0x82]) + n.to_bytes(2, "big")
        else:
            lb = bytes([0x83]) + n.to_bytes(3, "big")
        return tag + lb + body


def parse(buf, off=0, end=None, depth=0):
    """Parse one TLV; constructed tags recurse. Returns (Node, next_offset)."""
    if end is None:
        end = len(buf)
    tag = buf[off]
    p = off + 1
    if tag & 0x1F == 0x1F:
        while p < end and buf[p] & 0x80:
            p += 1
        p += 1
    first = buf[p]
    if first < 0x80:
        ln, p = first, p + 1
    elif first == 0x80:
        ln, p = end - p - 1, p + 1
    else:
        k = first & 0x7F
        ln = int.from_bytes(buf[p + 1:p + 1 + k], "big")
        p += 1 + k
    stop = min(p + ln, end)
    if tag & 0x20:
        kids = []
        q = p
        while q < stop:
            kid, q = parse(buf, q, stop, depth + 1)
            kids.append(kid)
        return Node(tag, kids), stop
    return Node(tag, buf[p:stop]), stop


def parse_one(buf):
    node, _ = parse(buf, 0, len(buf))
    return node


def find(node, tag, depth=None):
    """All nodes with `tag`, optionally only at `depth`."""
    return [n for n, d in node.walk() if n.tag == tag and (depth is None or d == depth)]


def at_depth(node, depth):
    return [n for n, d in node.walk() if d == depth]
