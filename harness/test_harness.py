"""Small regression checks for the evidence collector and fail-closed boundary."""
import http.client
import socket
import threading
import unittest
from unittest.mock import patch

from differential import RecordingReader, assert_isolated, comparable, differences
from tls_capture import fingerprint, grease, peek_hello


def vector(data, width=2):
    return len(data).to_bytes(width, "big") + data


def hello(ciphers, extensions):
    data = b"\x03\x03" + b"r" * 32 + vector(b"s" * 32, 1) + vector(bytes.fromhex(ciphers)) + b"\x01\0"
    data += vector(b"".join(bytes.fromhex(kind) + vector(value) for kind, value in extensions))
    return b"\x01" + len(data).to_bytes(3, "big") + data


class HarnessTests(unittest.TestCase):
    def test_reference_ja3_and_fragmented_records(self):
        # The independently asserted fingerprint is Go utls_client_test.go's
        # native 2.1.220 capture, not an expected value from this parser.
        extensions = [("0000", vector(b"\0" + vector(b"api.anthropic.com"))), ("0017", b""),
                      ("ff01", b"\0"), ("000a", vector(bytes.fromhex("001d00170018"))),
                      ("000b", b"\x01\0"), ("0023", b""), ("0010", vector(vector(b"http/1.1", 1))),
                      ("0005", b"\x01\0\0\0\0"), ("000d", vector(bytes.fromhex("040308040401050308050501080606010201"))),
                      ("0012", b""), ("0033", vector(bytes.fromhex("001d") + vector(b"k" * 32))),
                      ("002d", b"\x01\x01"), ("002b", b"\x04\x03\x04\x03\x03"), ("0015", b"\0" * 231)]
        packet = hello("130113021303c02bc02fc02cc030cca9cca8c009c013c00ac014009c009d002f0035", extensions)
        profile = fingerprint(packet)
        self.assertEqual(profile["ja3_md5"], "d871d02cecbde59abbf8f4806134addf")
        self.assertEqual(profile["handshake_length"], 508)
        self.assertEqual(profile["alpn"], ["http/1.1"])
        self.assertEqual(profile["sni"], "api.anthropic.com")
        # Hashes independently checked with openssl dgst -sha256 on the sorted
        # hex vectors required by FoxIO's algorithm (signature order retained).
        self.assertEqual(profile["ja4"], "t13d1714h1_5b57614c22b0_43ade6aba3df")
        receiver, sender = socket.socketpair()
        records = b"\x16\x03\x01" + vector(packet[:31]) + b"\x16\x03\x01" + vector(packet[31:])
        thread = threading.Thread(target=lambda: sender.sendall(records))
        thread.start()
        try:
            raw, seen = peek_hello(receiver)
            self.assertEqual(raw, records)
            self.assertEqual(seen, profile)
            self.assertEqual(receiver.recv(len(records)), records, "peek must not consume TLS bytes")
        finally:
            thread.join()
            receiver.close()
            sender.close()

    def test_grease_order_and_ja4(self):
        # JA4 sorts ciphers/extensions; JA3 preserves their wire order. A broad
        # normalizer must not erase the observable order change.
        first = fingerprint(hello("0a0a13021301", [("2a2a", b""), ("002b", b"\x04\x03\x04\x1a\x1a")]))
        second = fingerprint(hello("130113020a0a", [("002b", b"\x04\x03\x04\x1a\x1a"), ("2a2a", b"")]))
        self.assertEqual(first["ja4"], second["ja4"])
        self.assertNotEqual(first["ja3_md5"], second["ja3_md5"])
        self.assertTrue(first["ja4"].startswith("t13i020100_"))
        self.assertTrue(grease(0xfafa))
        self.assertFalse(grease(0x1a2a))
        with self.assertRaises(ValueError):
            fingerprint(hello("1301", [("002b", b"\x02\x03\x04")])[:-1])
        psk = vector(vector(b"ticket") + b"\0" * 4) + vector(vector(b"binder", 1))
        resumed = fingerprint(hello("1301", [("0029", psk)]))
        self.assertEqual(resumed["psk_identity_lengths"], [6])
        self.assertEqual(resumed["psk_binder_lengths"], [6])
        with self.assertRaises(ValueError):
            fingerprint(hello("1301", [("0029", psk[:-1])]))

    def test_diff_retains_header_order_bodies_and_unknown_fields(self):
        go = {"headers": [["Accept", "json"], ["Host", "api.anthropic.com"]], "body": "{\"x\":1}", "extra": 0}
        rust = {"headers": [["host", "api.anthropic.com"], ["accept", "json"]], "body": "{ \"x\":1}", "extra": None}
        paths = {entry["path"] for entry in differences(go, rust)}
        self.assertIn("/headers/0/0", paths)
        self.assertIn("/body", paths)
        self.assertIn("/extra", paths)
        self.assertEqual(differences({"x": None}, {})[0]["missing"], True)
        self.assertEqual(differences([], ["extra"])[0]["path"], "/length")
        self.assertEqual(differences([], ["extra"])[1]["rust"], "extra")
        raw = {"upstream": {"tls": [{"client_hello_hex": "random", "profile": {"ciphers": [1, 2]}}]},
               "downstream": [{"headers": [["Date", "today"], ["x-client-request-id", "keep-me"]],
                               "raw_response_hex": b"HTTP/1.1 200 OK\r\nDate: today\r\n\r\nDate: body".hex()}]}
        normalized = comparable(raw)
        self.assertEqual(normalized["downstream"][0]["headers"][1][1], "keep-me")
        self.assertEqual(bytes.fromhex(normalized["downstream"][0]["raw_response_hex"]),
                         b"HTTP/1.1 200 OK\r\nDate: <wall-clock>\r\n\r\nDate: body")
        self.assertEqual(raw["downstream"][0]["headers"][0][1], "today")
        self.assertIn("client_hello_hex", raw["upstream"]["tls"][0])

    def test_raw_downstream_chunking_and_early_close(self):
        receiver, sender = socket.socketpair()
        raw = b"HTTP/1.1 200 OK\r\nX-Mixed: yes\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n"
        sender.sendall(raw)
        sender.close()
        wire = bytearray()
        response = http.client.HTTPResponse(receiver)
        response.fp = RecordingReader(response.fp, wire)
        try:
            response.begin()
            self.assertEqual(response.read(1), b"a")
            self.assertEqual(response.read(), b"bc")
            response.close()
            self.assertEqual(bytes(wire), raw)
        finally:
            response.close()
            receiver.close()
        receiver, sender = socket.socketpair()
        sender.sendall(raw)
        sender.close()
        response = http.client.HTTPResponse(receiver)
        response.fp = RecordingReader(response.fp, bytearray())
        try:
            response.begin()
            self.assertEqual(response.read(1), b"a")
            response.close()  # Exercises HTTPResponse's flush before close.
        finally:
            receiver.close()

    def test_refuse_nonisolated_host_before_any_connect(self):
        with patch("differential.subprocess.check_output", return_value=b'[{"ifname":"lo"},{"ifname":"eth0"}]'), \
                patch("differential.socket.socket") as socket_mock:
            with self.assertRaisesRegex(RuntimeError, "only loopback"):
                assert_isolated()
            socket_mock.assert_not_called()


if __name__ == "__main__":
    unittest.main()
