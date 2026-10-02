"""ClientHello inspection, not TLS emulation. No encrypted-byte comparison."""
import hashlib
import socket
import time


def grease(value):
    return value & 0x0F0F == 0x0A0A and value >> 8 == value & 255


def words(data):
    if len(data) % 2:
        raise ValueError("odd TLS uint16 vector")
    return [int.from_bytes(data[i:i + 2], "big") for i in range(0, len(data), 2)]


def fingerprint(hello):
    """Parse the handshake, including its four-byte header. Reject truncation."""
    if len(hello) < 4 or hello[0] != 1 or int.from_bytes(hello[1:4], "big") != len(hello) - 4:
        raise ValueError("not a complete ClientHello")
    pos = 4

    def take(count):
        nonlocal pos
        value = hello[pos:pos + count]
        if len(value) != count:
            raise ValueError("truncated ClientHello field")
        pos += count
        return value

    def vector(width):
        return take(int.from_bytes(take(width), "big"))

    version = int.from_bytes(take(2), "big")
    take(32)  # random, retained only in the raw capture
    session = vector(1)
    ciphers = words(vector(2))
    compression = list(vector(1))
    extensions = vector(2)
    if pos != len(hello):
        raise ValueError("trailing ClientHello bytes")
    ext = []
    data = {}
    while extensions:
        if len(extensions) < 4:
            raise ValueError("truncated TLS extension")
        kind, length = words(extensions[:4])
        value = extensions[4:4 + length]
        if len(value) != length or kind in data:
            raise ValueError("truncated or duplicate TLS extension")
        ext.append([kind, length])
        data[kind] = value
        extensions = extensions[4 + length:]
    groups = words(data.get(10, b"\0\0")[2:])
    points = list(data.get(11, b"\0")[1:])
    signatures = words(data.get(13, b"\0\0")[2:])
    versions = words(data.get(43, b"\0")[1:])
    shares = data.get(51, b"\0\0")[2:]
    key_shares = []
    while shares:
        if len(shares) < 4:
            raise ValueError("truncated key share")
        group, length = words(shares[:4])
        if len(shares) < length + 4:
            raise ValueError("truncated key share value")
        key_shares.append([group, length])
        shares = shares[4 + length:]
    identities = []
    binders = []
    if 41 in data:
        psk = data[41]
        if len(psk) < 2:
            raise ValueError("truncated PSK extension")
        size = int.from_bytes(psk[:2], "big")
        entries, tail = psk[2:2 + size], psk[2 + size:]
        if len(entries) != size or len(tail) < 2 or int.from_bytes(tail[:2], "big") != len(tail) - 2:
            raise ValueError("truncated PSK vectors")
        while entries:
            size = int.from_bytes(entries[:2], "big")
            if len(entries) < size + 6:
                raise ValueError("truncated PSK identity")
            identities.append(size)
            entries = entries[size + 6:]
        tail = tail[2:]
        while tail:
            size = tail[0]
            if len(tail) < size + 1:
                raise ValueError("truncated PSK binder")
            binders.append(size)
            tail = tail[size + 1:]
    alpn = []
    protocols = data.get(16, b"\0\0")[2:]
    while protocols:
        length = protocols[0]
        if len(protocols) < length + 1:
            raise ValueError("truncated ALPN")
        alpn.append(protocols[1:length + 1].decode("ascii"))
        protocols = protocols[1 + length:]
    names = data.get(0, b"\0\0")[2:]
    sni = ""
    while names:
        if len(names) < 3:
            raise ValueError("truncated SNI")
        kind, length = names[0], int.from_bytes(names[1:3], "big")
        if len(names) < length + 3:
            raise ValueError("truncated SNI value")
        if kind == 0:
            sni = names[3:3 + length].decode("ascii")
        names = names[3 + length:]
    clean_ciphers = [v for v in ciphers if not grease(v)]
    clean_ext = [k for k, _ in ext if not grease(k)]
    join = lambda values: "-".join(map(str, values))
    ja3 = ",".join([str(version), join(clean_ciphers), join(clean_ext),
                    join(v for v in groups if not grease(v)), join(points)])
    # FoxIO JA4 technical_details/JA4.md: sort hex ciphers/extensions, but NOT
    # signature algorithms; exclude GREASE and exclude SNI/ALPN from hash c.
    hexes = lambda values: ",".join(f"{v:04x}" for v in values)
    digest = lambda value: hashlib.sha256(value.encode()).hexdigest()[:12] if value else "000000000000"
    tls_version = max((v for v in versions if not grease(v)), default=version)
    version_code = {0x0304: "13", 0x0303: "12", 0x0302: "11", 0x0301: "10", 0x0300: "s3"}.get(tls_version, "00")
    protocol = alpn[0] if alpn else ""
    if not protocol:
        alpn_code = "00"
    elif protocol[0].isascii() and protocol[-1].isascii() and protocol[0].isalnum() and protocol[-1].isalnum():
        alpn_code = protocol[0] + protocol[-1]
    else:
        raw = protocol.encode().hex()
        alpn_code = raw[0] + raw[-1]
    ext_hash_input = hexes(sorted(v for v in clean_ext if v not in (0, 16)))
    if ext_hash_input and signatures:
        ext_hash_input += "_" + hexes(v for v in signatures if not grease(v))
    ja4 = (f"t{version_code}{'d' if 0 in data else 'i'}{min(len(clean_ciphers), 99):02}"
           f"{min(len(clean_ext), 99):02}{alpn_code}_"
           f"{digest(hexes(sorted(clean_ciphers)))}_{digest(ext_hash_input)}")
    return {"handshake_length": len(hello) - 4, "legacy_version": version,
            "session_id_length": len(session), "ciphers": ciphers, "compression": compression,
            "extensions": ext, "groups": groups, "points": points, "signatures": signatures,
            "versions": versions, "key_shares": key_shares, "alpn": alpn, "sni": sni,
            "psk_identity_lengths": identities, "psk_binder_lengths": binders,
            "ja3": ja3, "ja3_md5": hashlib.md5(ja3.encode()).hexdigest(), "ja4": ja4,
            # Keep other extension payloads: GREASE/padding length, ticket bytes,
            # etc. Random/session/key-share/PSK opaque values remain raw only.
            "extension_data": {str(k): v.hex() for k, v in data.items() if k not in (51, 41)}}


def peek_hello(conn):
    """Peek without consuming bytes, so Python's TLS stack sees the exact hello."""
    deadline = time.monotonic() + 10
    records = b""
    handshake = b""
    while time.monotonic() < deadline:
        available = conn.recv(131072, socket.MSG_PEEK)
        if not available:
            raise ValueError("EOF before ClientHello")
        pos = 0
        handshake = b""
        while len(available) >= pos + 5:
            kind = available[pos]
            size = int.from_bytes(available[pos + 3:pos + 5], "big")
            if kind != 22:
                raise ValueError("expected TLS handshake record")
            if len(available) < pos + 5 + size:
                break
            handshake += available[pos + 5:pos + 5 + size]
            pos += 5 + size
            if len(handshake) >= 4:
                needed = 4 + int.from_bytes(handshake[1:4], "big")
                if len(handshake) >= needed:
                    records = available[:pos]
                    return records, fingerprint(handshake[:needed])
        time.sleep(0.001)
    raise TimeoutError("incomplete ClientHello")
