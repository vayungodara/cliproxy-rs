package main

// A raw HTTP/1.1 upstream for host.http.* scenarios. The Rust test runs the same server
// (cpa_plugin::testing::raw_upstream), so responses are identical byte for byte:
//
//	/echo    200, body = the exact request bytes received (head and body), with
//	         this server's host:port written as UPSTREAM
//	/stream  200 chunked: "one", "two", "three", 50 ms apart
//	/status  404, empty body
//	/slow    200 chunked: "one", then "two" 3 s later
//	/truncated  200 with Content-Length 10 and only "abc"
//	/http10  an HTTP/1.0 answer with Connection: close and a Trailer header
//	/trailer 200 chunked with a declared and sent trailer
//	/hangup  reads the request and closes without an answer
//	/redirect  302 to http://127.0.0.1:1/after (nothing listens there)
//
// Every response closes the connection and carries no Date header.

import (
	"bufio"
	"bytes"
	"fmt"
	"io"
	"net"
	"strconv"
	"strings"
	"time"
)

func startUpstream() string {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	check(err)
	go func() {
		for {
			conn, err := ln.Accept()
			if err != nil {
				return
			}
			go serveUpstream(conn, ln.Addr().String())
		}
	}()
	return ln.Addr().String()
}

func serveUpstream(conn net.Conn, addr string) {
	defer conn.Close()
	reader := bufio.NewReader(conn)
	var head bytes.Buffer
	contentLength := 0
	chunked := false
	for {
		line, err := reader.ReadString('\n')
		if err != nil {
			return
		}
		head.WriteString(line)
		trimmed := strings.TrimRight(line, "\r\n")
		if trimmed == "" {
			break
		}
		name, value, ok := strings.Cut(trimmed, ":")
		if !ok {
			continue
		}
		switch strings.ToLower(strings.TrimSpace(name)) {
		case "content-length":
			contentLength, _ = strconv.Atoi(strings.TrimSpace(value))
		case "transfer-encoding":
			chunked = strings.Contains(strings.ToLower(value), "chunked")
		}
	}
	var body []byte
	if chunked {
		var raw bytes.Buffer
		for {
			line, err := reader.ReadString('\n')
			if err != nil {
				return
			}
			raw.WriteString(line)
			if strings.TrimRight(line, "\r\n") == "0" {
				end, _ := reader.ReadString('\n')
				raw.WriteString(end)
				break
			}
		}
		body = raw.Bytes()
	} else if contentLength > 0 {
		body = make([]byte, contentLength)
		if _, err := io.ReadFull(reader, body); err != nil {
			return
		}
	}
	requestLine := strings.SplitN(head.String(), " ", 3)
	path := ""
	if len(requestLine) > 1 {
		path = requestLine[1]
	}
	switch {
	case strings.HasPrefix(path, "/echo"):
		// The echo names this server UPSTREAM so it compares across runs.
		echo := bytes.ReplaceAll(append(head.Bytes(), body...), []byte(addr), []byte("UPSTREAM"))
		fmt.Fprintf(conn, "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nX-Multi: a\r\nX-Multi: b\r\nConnection: close\r\nContent-Length: %d\r\n\r\n", len(echo))
		_, _ = conn.Write(echo)
	case strings.HasPrefix(path, "/stream"):
		_, _ = io.WriteString(conn, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n")
		for _, chunk := range []string{"one", "two", "three"} {
			fmt.Fprintf(conn, "%x\r\n%s\r\n", len(chunk), chunk)
			time.Sleep(50 * time.Millisecond)
		}
		_, _ = io.WriteString(conn, "0\r\n\r\n")
	case strings.HasPrefix(path, "/slow"):
		_, _ = io.WriteString(conn, "HTTP/1.1 200 OK\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n3\r\none\r\n")
		time.Sleep(3 * time.Second)
		_, _ = io.WriteString(conn, "3\r\ntwo\r\n0\r\n\r\n")
	case strings.HasPrefix(path, "/truncated"):
		_, _ = io.WriteString(conn, "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 10\r\n\r\nabc")
	case strings.HasPrefix(path, "/http10"):
		_, _ = io.WriteString(conn, "HTTP/1.0 200 OK\r\nConnection: close\r\nTrailer: X-T\r\nContent-Length: 2\r\n\r\nok")
	case strings.HasPrefix(path, "/trailer"):
		_, _ = io.WriteString(conn, "HTTP/1.1 200 OK\r\nConnection: close\r\nTrailer: X-T\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nok\r\n0\r\nX-T: v\r\n\r\n")
	case strings.HasPrefix(path, "/redirect"):
		_, _ = io.WriteString(conn, "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/after\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")
	case strings.HasPrefix(path, "/hangup"):
		// Reads the request and closes without answering.
	default:
		_, _ = io.WriteString(conn, "HTTP/1.1 404 Not Found\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")
	}
}
