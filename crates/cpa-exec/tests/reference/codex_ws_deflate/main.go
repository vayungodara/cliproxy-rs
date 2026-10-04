// Command codex_ws_deflate records how gorilla/websocket's dialer, with
// EnableCompression and write compression off as CLIProxyAPI's Codex executor sets them
// (internal/runtime/executor/codex_websockets_connection.go), negotiates
// permessage-deflate and what it reads from raw server frames. A loopback TCP server
// answers the upgrade itself so the response headers and frame bytes are exact.
//
// Usage: go run . <output.json>
package main

import (
	"bufio"
	"bytes"
	"compress/flate"
	"crypto/sha1"
	"encoding/base64"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"net"
	"net/http"
	"os"
	"strings"
	"time"

	"github.com/gorilla/websocket"
)

const negotiated = "permessage-deflate; server_no_context_takeover; client_no_context_takeover"

type result struct {
	// Offer is the Sec-WebSocket-Extensions request header the dialer sent.
	Offer string `json:"offer"`
	// DialError is set when Dial failed.
	DialError string `json:"dial_error,omitempty"`
	// Messages are the text messages read before ReadError.
	Messages  []string `json:"messages"`
	ReadError bool     `json:"read_error"`
}

type negotiation struct {
	Headers []string `json:"headers"`
	result
}

type frames struct {
	Name  string `json:"name"`
	Bytes string `json:"bytes"`
	result
}

func acceptKey(key string) string {
	h := sha1.New()
	h.Write([]byte(key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"))
	return base64.StdEncoding.EncodeToString(h.Sum(nil))
}

// run answers one upgrade with extensions, writes raw, then closes the TCP connection.
func run(extensions []string, raw []byte) result {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		panic(err)
	}
	defer func() { _ = ln.Close() }()
	offer := make(chan string, 1)
	go func() {
		conn, err := ln.Accept()
		if err != nil {
			return
		}
		defer func() { _ = conn.Close() }()
		req, err := http.ReadRequest(bufio.NewReader(conn))
		if err != nil {
			return
		}
		offer <- req.Header.Get("Sec-WebSocket-Extensions")
		var resp strings.Builder
		resp.WriteString("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n")
		resp.WriteString("Sec-WebSocket-Accept: " + acceptKey(req.Header.Get("Sec-WebSocket-Key")) + "\r\n")
		for _, ext := range extensions {
			resp.WriteString("Sec-WebSocket-Extensions: " + ext + "\r\n")
		}
		resp.WriteString("\r\n")
		_, _ = conn.Write([]byte(resp.String()))
		_, _ = conn.Write(raw)
		time.Sleep(100 * time.Millisecond)
	}()
	dialer := websocket.Dialer{EnableCompression: true, HandshakeTimeout: 5 * time.Second}
	conn, _, err := dialer.Dial("ws://"+ln.Addr().String()+"/responses", nil)
	out := result{Messages: []string{}}
	select {
	case out.Offer = <-offer:
	case <-time.After(5 * time.Second):
	}
	if err != nil {
		out.DialError = err.Error()
		return out
	}
	defer func() { _ = conn.Close() }()
	conn.EnableWriteCompression(false)
	_ = conn.SetReadDeadline(time.Now().Add(5 * time.Second))
	for {
		_, msg, err := conn.ReadMessage()
		if err != nil {
			out.ReadError = true
			return out
		}
		out.Messages = append(out.Messages, string(msg))
	}
}

func mustHex(s string) []byte {
	b, err := hex.DecodeString(strings.ReplaceAll(s, " ", ""))
	if err != nil {
		panic(err)
	}
	return b
}

// compressed is one final text frame carrying text compressed like gorilla's writer
// (flush, then the 00 00 ff ff tail removed).
func compressed(text string) []byte {
	var buf bytes.Buffer
	w, _ := flate.NewWriter(&buf, flate.BestSpeed)
	_, _ = w.Write([]byte(text))
	_ = w.Flush()
	payload := bytes.TrimSuffix(buf.Bytes(), []byte{0x00, 0x00, 0xff, 0xff})
	frame := []byte{0xc1}
	switch n := len(payload); {
	case n < 126:
		frame = append(frame, byte(n))
	case n <= 0xffff:
		frame = append(frame, 126, byte(n>>8), byte(n))
	default:
		var l [8]byte
		binary.BigEndian.PutUint64(l[:], uint64(n))
		frame = append(append(frame, 127), l[:]...)
	}
	return append(frame, payload...)
}

func main() {
	hello := mustHex("c1 07 f2 48 cd c9 c9 07 00") // RFC 7692 7.2.3.1
	var negotiations []negotiation
	for _, headers := range [][]string{
		nil,
		{negotiated},
		{"permessage-deflate"},
		{"permessage-deflate; server_no_context_takeover"},
		{"permessage-deflate; client_no_context_takeover"},
		{"permessage-deflate; client_no_context_takeover; server_no_context_takeover"},
		{"permessage-deflate;server_no_context_takeover;client_no_context_takeover"},
		{"x-webkit-deflate-frame, " + negotiated},
		{negotiated + "; server_max_window_bits=10"},
		{"permessage-deflate; server_no_context_takeover=\"x\"; client_no_context_takeover"},
		{"PERMESSAGE-DEFLATE; server_no_context_takeover; client_no_context_takeover"},
		{negotiated + ", permessage-deflate"},
		{"permessage-deflate; client_no_context_takeover, " + negotiated},
		{negotiated + "; bad@"},
		{"foo", negotiated},
		{"foo; bar=\"unterminated", negotiated},
	} {
		negotiations = append(negotiations, negotiation{Headers: headers, result: run(headers, hello)})
	}
	big := `{"type":"response.output_text.delta","delta":"` + strings.Repeat("compressible ", 40) + `"}`
	var frameCases []frames
	for _, c := range []struct{ name, hex string }{
		{"single", "c1 07 f2 48 cd c9 c9 07 00"},
		{"fragmented_with_ping", "41 03 f2 48 cd 89 00 80 04 c9 c9 07 00"},
		{"stored_block", "c1 0b 00 05 00 fa ff 48 65 6c 6c 6f 00"},
		{"uncompressed", "81 05 48 65 6c 6c 6f"},
		{"two_messages", "c1 07 f2 48 cd c9 c9 07 00 c1 07 f2 48 cd c9 c9 07 00"},
		{"uncompressed_fragments", "01 02 48 65 80 03 6c 6c 6f"},
		{"rsv1_on_control", "c9 00 81 05 48 65 6c 6c 6f"},
		{"rsv1_on_continuation", "41 03 f2 48 cd c0 04 c9 c9 07 00"},
		{"rsv1_on_uncompressed_continuation", "01 02 48 65 c0 03 6c 6c 6f"},
		{"rsv2_set", "a1 05 48 65 6c 6c 6f"},
		{"masked_server_frame", "81 85 00 00 00 00 48 65 6c 6c 6f"},
		{"data_inside_compressed_message", "41 03 f2 48 cd 81 05 48 65 6c 6c 6f"},
		{"corrupt_deflate", "c1 03 ff ff ff"},
		{"rsv2_on_compressed", "e1 07 f2 48 cd c9 c9 07 00"},
		{"rsv3_on_compressed_continuation", "41 03 f2 48 cd 90 04 c9 c9 07 00"},
		{"valid_then_corrupt", "c1 07 f2 48 cd c9 c9 07 00 c1 03 ff ff ff"},
		{"big_json", hex.EncodeToString(compressed(big))},
	} {
		raw := mustHex(c.hex)
		frameCases = append(frameCases, frames{Name: c.name, Bytes: hex.EncodeToString(raw), result: run([]string{negotiated}, raw)})
	}
	data, err := json.MarshalIndent(map[string]any{"negotiations": negotiations, "frames": frameCases}, "", "  ")
	if err != nil {
		panic(err)
	}
	if err := os.WriteFile(os.Args[1], append(data, '\n'), 0o644); err != nil {
		panic(err)
	}
	fmt.Println("wrote", os.Args[1])
}
