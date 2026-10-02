package helps

// ClientHello captured from Go's own chatgpt.com round tripper (newUtlsRoundTripper,
// uTLS HelloChrome_Auto). Copy into internal/runtime/executor/helps/ of CLIProxyAPI
// 6fecc6e and run with RSFIX_OUT=<dir>; it writes codex_chrome_hello.json. The listener
// is local and closes after reading the hello, so nothing leaves the machine.
//
// Chrome randomizes GREASE values, extension order and the ECH GREASE payload, so the
// fixture keeps what is stable: values with GREASE replaced by "GREASE", extension
// contents keyed by type, and lengths for random fields.

import (
	"context"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"io"
	"net"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"testing"
	"time"
)

func rsfixGrease(v uint16) bool { return v&0x0f0f == 0x0a0a && v>>8 == v&0xff }

func rsfixU16(v uint16) string {
	if rsfixGrease(v) {
		return "GREASE"
	}
	return hex.EncodeToString([]byte{byte(v >> 8), byte(v)})
}

type rsfixHello struct {
	Ciphers    []string          `json:"ciphers"`
	Extensions []string          `json:"extensions"`
	Contents   map[string]string `json:"contents"`
	KeyShares  []string          `json:"key_shares"`
	ECHPayload int               `json:"ech_payload_len"`
}

func rsfixParseHello(t *testing.T, record []byte) rsfixHello {
	h := rsfixHello{Contents: map[string]string{}}
	p := 4 + 2 + 32
	p += 1 + int(record[p])
	n := int(binary.BigEndian.Uint16(record[p:]))
	p += 2
	for i := 0; i < n; i += 2 {
		h.Ciphers = append(h.Ciphers, rsfixU16(binary.BigEndian.Uint16(record[p+i:])))
	}
	p += n
	p += 1 + int(record[p])
	end := p + 2 + int(binary.BigEndian.Uint16(record[p:]))
	p += 2
	for p < end {
		typ := binary.BigEndian.Uint16(record[p:])
		size := int(binary.BigEndian.Uint16(record[p+2:]))
		data := record[p+4 : p+4+size]
		p += 4 + size
		name := rsfixU16(typ)
		h.Extensions = append(h.Extensions, name)
		switch {
		case rsfixGrease(typ):
		case typ == 0x0033: // key_share: group and key length
			for q := 2; q < len(data); {
				group := binary.BigEndian.Uint16(data[q:])
				klen := int(binary.BigEndian.Uint16(data[q+2:]))
				h.KeyShares = append(h.KeyShares, rsfixU16(group)+":"+strconv.Itoa(klen))
				q += 4 + klen
			}
		case typ == 0xfe0d: // ECH GREASE: fixed suite, random config id, enc and payload
			h.Contents[name] = hex.EncodeToString(data[:5])
			encLen := int(binary.BigEndian.Uint16(data[6:]))
			h.ECHPayload = int(binary.BigEndian.Uint16(data[8+encLen:]))
		case typ == 0x000a || typ == 0x002b: // u16 lists after a 2- or 1-byte length
			start := 2
			if typ == 0x002b {
				start = 1
			}
			norm := hex.EncodeToString(data[:start])
			for q := start; q+1 < len(data); q += 2 {
				norm += "," + rsfixU16(binary.BigEndian.Uint16(data[q:]))
			}
			h.Contents[name] = norm
		default:
			h.Contents[name] = hex.EncodeToString(data)
		}
	}
	return h
}

func TestRSFixCodexChromeHello(t *testing.T) {
	dir := os.Getenv("RSFIX_OUT")
	if dir == "" {
		t.Skip("RSFIX_OUT not set")
	}
	var hellos []rsfixHello
	orders := map[string]bool{}
	for i := 0; i < 12; i++ {
		listener, err := net.Listen("tcp", "127.0.0.1:0")
		if err != nil {
			t.Fatal(err)
		}
		records := make(chan []byte, 1)
		go func() {
			conn, errAccept := listener.Accept()
			if errAccept != nil {
				records <- nil
				return
			}
			defer func() { _ = conn.Close() }()
			header := make([]byte, 5)
			if _, errRead := io.ReadFull(conn, header); errRead != nil {
				records <- nil
				return
			}
			record := make([]byte, binary.BigEndian.Uint16(header[3:]))
			_, _ = io.ReadFull(conn, record)
			records <- record
		}()
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		_, _ = newUtlsRoundTripper("").createConnection(ctx, "chatgpt.com", listener.Addr().String())
		cancel()
		record := <-records
		_ = listener.Close()
		if record == nil {
			t.Fatal("no ClientHello captured")
		}
		h := rsfixParseHello(t, record)
		order, _ := json.Marshal(h.Extensions)
		orders[string(order)] = true
		sorted := append([]string(nil), h.Extensions...)
		sort.Strings(sorted)
		h.Extensions = sorted
		hellos = append(hellos, h)
	}
	out := map[string]any{"hellos": hellos, "distinct_orders": len(orders)}
	data, err := json.MarshalIndent(out, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "codex_chrome_hello.json"), append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
