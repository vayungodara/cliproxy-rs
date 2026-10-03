package live

// Golden vectors for cliproxy-rs's port of tcp_proxy.go and proxyutil.BuildDialer.
// Runs only with RSFIX_OUT set; every expected value comes from the real Go code.

import (
	"bufio"
	"context"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/netip"
	"os"
	"net/url"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
	"time"

	"github.com/pion/stun/v3"
	"github.com/router-for-me/CLIProxyAPI/v8/sdk/proxyutil"
)

type neverDialer struct{}

func (neverDialer) DialContext(context.Context, string, string) (net.Conn, error) {
	return nil, fmt.Errorf("never")
}

func tunnelSDP(sessionLines string, medias []struct{ mid, ufrag, pwd string; cands []string }) string {
	var b strings.Builder
	b.WriteString("v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n")
	b.WriteString(sessionLines)
	b.WriteString("a=group:BUNDLE 0 1\r\n")
	for _, m := range medias {
		line := "m=audio 9 UDP/TLS/RTP/SAVPF 111"
		if m.mid == "1" {
			line = "m=application 9 UDP/DTLS/SCTP webrtc-datachannel"
		}
		fmt.Fprintf(&b, "%s\r\nc=IN IP4 0.0.0.0\r\na=mid:%s\r\n", line, m.mid)
		if m.ufrag != "-" {
			fmt.Fprintf(&b, "a=ice-ufrag:%s\r\n", m.ufrag)
		}
		if m.pwd != "-" {
			fmt.Fprintf(&b, "a=ice-pwd:%s\r\n", m.pwd)
		}
		for _, c := range m.cands {
			fmt.Fprintf(&b, "a=candidate:%s\r\n", c)
		}
	}
	return b.String()
}

type media = struct{ mid, ufrag, pwd string; cands []string }

func std(cands ...string) string {
	return tunnelSDP("", []media{{"0", "remote-ufrag", "remote-password", cands}, {"1", "remote-ufrag", "remote-password", nil}})
}

func stdOffer() string {
	return tunnelSDP("", []media{{"0", "local-ufrag", "local-password", nil}, {"1", "local-ufrag", "local-password", nil}})
}

func tcpPassive(addr string, port int) string {
	return fmt.Sprintf("2 1 tcp 1671430143 %s %d typ host tcptype passive", addr, port)
}

func prepareVectors() []map[string]any {
	type pcase struct {
		name, answer, offer string
	}
	cases := []pcase{
		{"go_rewrite", std("1 1 udp 2130706431 20.42.0.10 3478 typ host", "2 1 tcp 1671430143 20.42.0.20 443 typ host tcptype passive"), stdOffer()},
		{"go_private", std("1 1 tcp 1671430143 10.0.0.1 443 typ host tcptype passive"), stdOffer()},
		{"go_zero_network", std("1 1 tcp 1671430143 0.0.0.1 443 typ host tcptype passive"), stdOffer()},
		{"go_carrier_nat", std("1 1 tcp 1671430143 100.64.0.1 443 typ host tcptype passive"), stdOffer()},
		{"go_reserved", std("1 1 tcp 1671430143 203.0.113.10 443 typ host tcptype passive"), stdOffer()},
		{"go_site_local_v6", std("1 1 tcp 1671430143 fec0::1 443 typ host tcptype passive"), stdOffer()},
		{"go_wrong_port", std("1 1 tcp 1671430143 20.42.0.10 8443 typ host tcptype passive"), stdOffer()},
		{"go_relay", std("1 1 tcp 1671430143 20.42.0.10 443 typ relay raddr 192.0.2.1 rport 5000 tcptype passive"), stdOffer()},
		{"go_active", std("1 1 tcp 1671430143 20.42.0.10 443 typ host tcptype active"), stdOffer()},
		{"only_udp", std("1 1 udp 2130706431 20.42.0.10 3478 typ host"), stdOffer()},
		{"no_candidates", std(), stdOffer()},
		{"ipv6_public", std(tcpPassive("2606:4700::1", 443)), stdOffer()},
		{"ipv4_mapped", std(tcpPassive("::ffff:20.42.0.20", 443)), stdOffer()},
		{"component_two", std("2 2 tcp 1671430143 20.42.0.20 443 typ host tcptype passive"), stdOffer()},
		{"component_two_and_valid", std("2 2 tcp 1671430143 20.42.0.21 443 typ host tcptype passive", tcpPassive("20.42.0.20", 443)), stdOffer()},
		{"upper_tcp", std("2 1 TCP 1671430143 20.42.0.20 443 typ host tcptype passive"), stdOffer()},
		{"upper_passive", std("2 1 tcp 1671430143 20.42.0.20 443 typ host tcptype PASSIVE"), stdOffer()},
		{"extensions", std("2 1 tcp 1671430143 20.42.0.20 443 typ host tcptype passive generation 0 network-id 1"), stdOffer()},
		{"extra_spaces", std("  2  1 tcp 1671430143   20.42.0.20 443 typ host tcptype passive  "), stdOffer()},
		{"mdns_name", std(tcpPassive("abcd-1234.local", 443)), stdOffer()},
		{"hostname", std(tcpPassive("example.com", 443)), stdOffer()},
		{"short", std("1 1 tcp"), stdOffer()},
		{"bad_port", std("2 1 tcp 1671430143 20.42.0.20 abc typ host tcptype passive"), stdOffer()},
		{"bad_priority", std("2 1 tcp x 20.42.0.20 443 typ host tcptype passive"), stdOffer()},
		{"bad_component", std("2 x tcp 1 20.42.0.20 443 typ host tcptype passive"), stdOffer()},
		{"unknown_type", std("2 1 tcp 1 20.42.0.20 443 typ foo tcptype passive"), stdOffer()},
		{"unknown_network", std("2 1 sctp 1 20.42.0.20 443 typ host"), stdOffer()},
		{"srflx", std("2 1 tcp 1 20.42.0.20 443 typ srflx raddr 0.0.0.0 rport 0 tcptype passive"), stdOffer()},
		{"simultaneous_open", std("2 1 tcp 1 20.42.0.20 443 typ host tcptype so"), stdOffer()},
		{"tcp_no_tcptype", std("2 1 tcp 1 20.42.0.20 443 typ host"), stdOffer()},
		{"unspecified", std(tcpPassive("0.0.0.0", 443)), stdOffer()},
		{"multicast", std(tcpPassive("224.0.0.1", 443)), stdOffer()},
		{"broadcast", std(tcpPassive("255.255.255.255", 443)), stdOffer()},
		{"loopback", std(tcpPassive("127.0.0.1", 443)), stdOffer()},
		{"private_192", std(tcpPassive("192.168.1.1", 443)), stdOffer()},
		{"nat64", std(tcpPassive("64:ff9b::1", 443)), stdOffer()},
		{"teredo", std(tcpPassive("2001::1", 443)), stdOffer()},
		{"google_v6", std(tcpPassive("2001:4860::1", 443)), stdOffer()},
		{"second_media", tunnelSDP("", []media{{"0", "remote-ufrag", "remote-password", nil}, {"1", "remote-ufrag", "remote-password", []string{tcpPassive("20.42.0.20", 443)}}}), stdOffer()},
		{"session_credentials", tunnelSDP("a=ice-ufrag:sess-ufrag\r\na=ice-pwd:sess-password\r\n", []media{{"0", "-", "-", []string{tcpPassive("20.42.0.20", 443)}}, {"1", "-", "-", nil}}), stdOffer()},
		{"media_overrides_session", tunnelSDP("a=ice-ufrag:sess-ufrag\r\na=ice-pwd:sess-password\r\n", []media{{"0", "media-ufrag", "-", []string{tcpPassive("20.42.0.20", 443)}}, {"1", "media-ufrag", "-", nil}}), stdOffer()},
		{"inconsistent", tunnelSDP("", []media{{"0", "a", "p", []string{tcpPassive("20.42.0.20", 443)}}, {"1", "b", "p", nil}}), stdOffer()},
		{"incomplete", tunnelSDP("", []media{{"0", "a", "-", []string{tcpPassive("20.42.0.20", 443)}}, {"1", "-", "-", nil}}), stdOffer()},
		{"missing", tunnelSDP("", []media{{"0", "-", "-", []string{tcpPassive("20.42.0.20", 443)}}, {"1", "-", "-", nil}}), stdOffer()},
		{"padded_credentials", tunnelSDP("", []media{{"0", " remote-ufrag ", " remote-password ", []string{tcpPassive("20.42.0.20", 443)}}, {"1", "remote-ufrag", "remote-password", nil}}), stdOffer()},
		{"offer_missing", std(tcpPassive("20.42.0.20", 443)), tunnelSDP("", []media{{"0", "-", "-", nil}, {"1", "-", "-", nil}})},
		{"offer_inconsistent", std(tcpPassive("20.42.0.20", 443)), tunnelSDP("", []media{{"0", "x", "p", nil}, {"1", "y", "p", nil}})},
		{"garbage_answer", "garbage", stdOffer()},
		{"v6_bad_digit", std(tcpPassive("2606:4700::zz", 443)), stdOffer()},
		{"v6_triple_colon", std(tcpPassive("2606:4700:::1", 443)), stdOffer()},
		{"v6_nine_fields", std(tcpPassive("1:2:3:4:5:6:7:8:9", 443)), stdOffer()},
		{"v6_long_group", std(tcpPassive("12345::1", 443)), stdOffer()},
		{"v6_trailing_colon", std(tcpPassive("1::2:", 443)), stdOffer()},
		{"v6_too_short", std(tcpPassive("1:2", 443)), stdOffer()},
		{"v6_double_ellipsis", std(tcpPassive("1::2::3", 443)), stdOffer()},
		{"v6_full_with_ellipsis", std(tcpPassive("1:2:3:4::5:6:7:8", 443)), stdOffer()},
		{"v6_embedded_v4", std(tcpPassive("2606:4700::1.2.3.4", 443)), stdOffer()},
		{"v6_embedded_v4_misplaced", std(tcpPassive("1:2:3:1.2.3.4", 443)), stdOffer()},
		{"v6_embedded_v4_too_many", std(tcpPassive("1:2:3:4:5:6:7:1.2.3.4", 443)), stdOffer()},
		{"v6_embedded_bad_v4", std(tcpPassive("::ffff:1.2.3.400", 443)), stdOffer()},
		{"v6_garbage_after_v4", std(tcpPassive("::ffff:1.2.3.4x", 443)), stdOffer()},
		{"v6_bad_separator", std(tcpPassive("2606:4700::1g", 443)), stdOffer()},
		{"v4_short", std(tcpPassive("1.2.3", 443)), stdOffer()},
		{"v4_long", std(tcpPassive("1.2.3.4.5", 443)), stdOffer()},
		{"v4_leading_zero", std(tcpPassive("01.2.3.4", 443)), stdOffer()},
		{"v4_big", std(tcpPassive("256.1.1.1", 443)), stdOffer()},
		{"v4_empty_field", std(tcpPassive("1..2.3", 443)), stdOffer()},
		{"v4_with_zone", std(tcpPassive("20.42.0.20%eth0", 443)), stdOffer()},
		{"percent_only", std(tcpPassive("%eth0", 443)), stdOffer()},
		{"no_separator", std(tcpPassive("abc", 443)), stdOffer()},
		{"garbage_offer", std(tcpPassive("20.42.0.20", 443)), "garbage"},
	}
	many := func(n int, f func(int) string) []string {
		out := make([]string, 0, n)
		for i := 0; i < n; i++ {
			out = append(out, f(i))
		}
		return out
	}
	cases = append(cases,
		pcase{"limit_65", std(many(65, func(i int) string { return fmt.Sprintf("%d 1 udp 2130706431 20.42.0.10 3478 typ host", i+1) })...), stdOffer()},
		pcase{"exactly_64", std(append(many(63, func(i int) string { return fmt.Sprintf("%d 1 udp 2130706431 20.42.0.10 3478 typ host", i+1) }), tcpPassive("20.42.0.20", 443))...), stdOffer()},
		pcase{"tcp_limit_17", std(many(17, func(i int) string { return tcpPassive(fmt.Sprintf("20.42.1.%d", i+1), 443) })...), stdOffer()},
		pcase{"tcp_16", std(many(16, func(i int) string { return tcpPassive(fmt.Sprintf("20.42.1.%d", i+1), 443) })...), stdOffer()},
		pcase{"limit_before_bad", std(append(many(65, func(i int) string { return fmt.Sprintf("%d 1 udp 2130706431 20.42.0.10 3478 typ host", i+1) }), "bad")...), stdOffer()},
		pcase{"bad_before_limit", std(append([]string{tcpPassive("10.0.0.1", 443)}, many(65, func(i int) string { return fmt.Sprintf("%d 1 udp 2130706431 20.42.0.10 3478 typ host", i+1) })...)...), stdOffer()},
	)
	out := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		rewritten, tunnels, err := prepareProxiedUpstreamAnswer(c.answer, c.offer, neverDialer{})
		result := map[string]any{"name": c.name, "answer": c.answer, "offer": c.offer}
		if err != nil {
			result["error"] = err.Error()
		} else {
			masked := rewritten
			targets := []string{}
			listeners := []string{}
			for i, tunnel := range tunnels {
				addr := tunnel.listener.Addr().(*net.TCPAddr)
				masked = strings.ReplaceAll(masked, addr.IP.String()+" "+strconv.Itoa(addr.Port)+" ", fmt.Sprintf("LISTEN%d ", i))
				targets = append(targets, tunnel.target.String())
				listeners = append(listeners, addr.IP.String())
				result["expected_user"] = tunnel.expectedUser
				result["password"] = tunnel.remotePassword
			}
			result["sdp"] = masked
			result["targets"] = targets
			result["listeners"] = listeners
		}
		_ = closeCandidateTunnels(tunnels)
		out = append(out, result)
	}
	return out
}

func rawFrame(raw []byte) []byte {
	frame := make([]byte, 2+len(raw))
	binary.BigEndian.PutUint16(frame[:2], uint16(len(raw)))
	copy(frame[2:], raw)
	return frame
}

func buildFrame(t *testing.T, setters ...stun.Setter) []byte {
	m, err := stun.Build(setters...)
	if err != nil {
		t.Fatalf("build: %v", err)
	}
	return rawFrame(m.Raw)
}

func frameVectors(t *testing.T) []map[string]any {
	user, password := "remote:local", "remote-password"
	valid := buildFrame(t, stun.BindingRequest, stun.TransactionID, stun.NewUsername(user), stun.NewShortTermIntegrity(password), stun.Fingerprint)
	trailing := append([]byte(nil), valid[2:]...)
	trailing = append(trailing, 0, 0, 0, 0)
	badCookie := append([]byte(nil), valid...)
	badCookie[2+4] ^= 0xff
	badFingerprint := append([]byte(nil), valid...)
	badFingerprint[len(badFingerprint)-1] ^= 0xff
	afterFingerprint, _ := stun.Build(stun.BindingRequest, stun.TransactionID, stun.NewUsername(user), stun.NewShortTermIntegrity(password), stun.Fingerprint)
	afterFingerprint.Add(stun.AttrSoftware, []byte("late"))
	type fcase struct {
		name           string
		frame          []byte
		user, password string
	}
	cases := []fcase{
		{"valid", valid, user, password},
		{"wrong_username", valid, "local:remote", password},
		{"wrong_password", valid, user, "local-password"},
		{"missing_fingerprint", buildFrame(t, stun.BindingRequest, stun.TransactionID, stun.NewUsername(user), stun.NewShortTermIntegrity(password)), user, password},
		{"undersized_header", []byte{0, 1, 0}, user, password},
		{"size_19", append([]byte{0, 19}, make([]byte, 19)...), user, password},
		{"size_4097", append([]byte{0x10, 0x01}, make([]byte, 4097)...), user, password},
		{"size_4096_garbage", append([]byte{0x10, 0x00}, make([]byte, 4096)...), user, password},
		{"truncated", valid[:len(valid)-5], user, password},
		{"trailing", rawFrame(trailing), user, password},
		{"bad_cookie", badCookie, user, password},
		{"bad_fingerprint", badFingerprint, user, password},
		{"success_type", buildFrame(t, stun.BindingSuccess, stun.TransactionID, stun.NewUsername(user), stun.NewShortTermIntegrity(password), stun.Fingerprint), user, password},
		{"indication_type", buildFrame(t, stun.NewType(stun.MethodBinding, stun.ClassIndication), stun.TransactionID, stun.NewUsername(user), stun.NewShortTermIntegrity(password), stun.Fingerprint), user, password},
		{"username_after_integrity", buildFrame(t, stun.BindingRequest, stun.TransactionID, stun.NewShortTermIntegrity(password), stun.NewUsername(user), stun.Fingerprint), user, password},
		{"software_after_integrity", buildFrame(t, stun.BindingRequest, stun.TransactionID, stun.NewUsername(user), stun.NewShortTermIntegrity(password), stun.NewSoftware("x"), stun.Fingerprint), user, password},
		{"attribute_after_fingerprint", rawFrame(afterFingerprint.Raw), user, password},
		{"no_integrity", buildFrame(t, stun.BindingRequest, stun.TransactionID, stun.NewUsername(user), stun.Fingerprint), user, password},
		{"no_username", buildFrame(t, stun.BindingRequest, stun.TransactionID, stun.NewShortTermIntegrity(password), stun.Fingerprint), user, password},
		{"ice_check", buildFrame(t, stun.BindingRequest, stun.TransactionID, stun.NewUsername(user), stun.RawAttribute{Type: stun.AttrPriority, Value: []byte{0x6e, 0, 0x1e, 0xff}}, stun.RawAttribute{Type: stun.AttrICEControlling, Value: []byte{1, 2, 3, 4, 5, 6, 7, 8}}, stun.RawAttribute{Type: stun.AttrUseCandidate}, stun.NewShortTermIntegrity(password), stun.Fingerprint), user, password},
		{"empty_username_expected", buildFrame(t, stun.BindingRequest, stun.TransactionID, stun.NewUsername(""), stun.NewShortTermIntegrity(password), stun.Fingerprint), "", password},
	}
	out := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		got, err := readValidatedICEBindingFrame(&fragmentedReader{data: append([]byte(nil), c.frame...), maximum: 7}, c.user, c.password)
		result := map[string]any{"name": c.name, "frame": hex.EncodeToString(c.frame), "user": c.user, "password": c.password, "ok": err == nil}
		if err != nil {
			result["error"] = err.Error()
		} else {
			result["returned"] = hex.EncodeToString(got)
		}
		out = append(out, result)
	}
	return out
}

func targetVectors() []map[string]any {
	addrs := []string{
		"20.42.0.20", "1.1.1.1", "8.8.8.8", "0.0.0.0", "0.255.255.255", "1.0.0.0", "9.255.255.255", "10.0.0.0", "10.255.255.255", "11.0.0.0",
		"100.63.255.255", "100.64.0.0", "100.127.255.255", "100.128.0.0", "126.255.255.255", "127.0.0.1", "128.0.0.0",
		"169.253.255.255", "169.254.0.1", "169.255.0.0", "172.15.255.255", "172.16.0.0", "172.31.255.255", "172.32.0.0",
		"192.0.0.1", "192.0.1.0", "192.0.2.1", "192.0.3.0", "192.88.99.1", "192.88.100.0", "192.167.255.255", "192.168.0.1", "192.169.0.0",
		"198.17.255.255", "198.18.0.0", "198.19.255.255", "198.20.0.0", "198.51.100.1", "198.51.101.0", "203.0.112.255", "203.0.113.1", "203.0.114.0",
		"223.255.255.255", "224.0.0.1", "239.255.255.255", "240.0.0.1", "255.255.255.255",
		"::", "::1", "::2", "::ffff:8.8.8.8", "::ffff:10.0.0.1", "::8.8.8.8", "64:ff9b::808:808", "64:ff9b:1::1", "64:ff9b:2::1", "100::1", "100:0:0:1::1",
		"2001::1", "2001:1ff::1", "2001:200::1", "2001:db8::1", "2001:4860::8888", "2002::1", "2003::1", "3fff::1", "3fff:1000::1", "4000::1",
		"5f00::1", "5f01::1", "2606:4700::1", "fc00::1", "fdff::1", "fe00::1", "fe80::1", "febf::1", "fec0::1", "feff::1", "ff02::1", "fe7f::1",
		"e000::1", "1::1",
	}
	out := make([]map[string]any, 0, len(addrs))
	for _, a := range addrs {
		addr := netip.MustParseAddr(a)
		out = append(out, map[string]any{"addr": a, "public": isPublicProxyTarget(addr), "public_unmapped": isPublicProxyTarget(addr.Unmap())})
	}
	return out
}

func parseVectors() []map[string]any {
	inputs := []string{
		"", "  ", "direct", "DIRECT", "none", "socks5://127.0.0.1:1080", "socks5h://u:p@proxy:1", "http://proxy", "https://u@proxy:8443",
		"ftp://proxy:21", "proxy:8080", "://bad", "http://", "http://%zz", "HTTP://proxy:1", "socks4://proxy:1", " socks5://p:1 ", "http://[::1]:3128",
		"http://proxy:99999", "/just/a/path", "http//proxy",
		// Go net/url corner cases the url crate reads differently.
		"http:proxy", "http://proxy/%zz", "ftp://proxy?x=%zz", "http://proxy?x=%zz", "http://proxy#%zz", "http://proxy#ok%41",
		"socks5://u:%FF@proxy:1080", "socks5://us%zzer@p:1", "http://a b@p:1", "http://u@x@p:1", "socks5://@p:1", "socks5://:@p:1",
		"socks5://u:@p:1", "socks5://u@p:1", "http://p:", "http://[::1]", "http://[::1]:3128", "http://[::1", "http://x[::1]:1",
		"http://[1.2.3.4]:1", "http://[fe80::1%25eth0]:1", "http://pro{xy:1", "http://%41:1", "http://%c3%a9:1", "http://p:12a",
		"http://a:b:1", "socks5://a:b:1", "HTTP://P:1", "http://p:1/a/%zz", "http://p:1/a%2Fb", "*", "http://p\x01:1", "socks5h://p",
		"https://p", "http://p", "socks5://p:0", "http://p:65536", "http://p:1/?", "socks5:///p", "socks5:p:1",
	}
	out := make([]map[string]any, 0, len(inputs))
	for _, in := range inputs {
		_, mode, err := proxyutil.BuildDialer(in)
		r := map[string]any{"in": in, "mode": int(mode), "scheme": proxyScheme(in)}
		if err != nil {
			r["error"] = err.Error()
		}
		if mode == proxyutil.ModeProxy {
			u, _ := url.Parse(strings.TrimSpace(in))
			r["hostname"] = u.Hostname()
			r["port"] = u.Port()
			if u.User != nil {
				password, has := u.User.Password()
				r["username"] = hex.EncodeToString([]byte(u.User.Username()))
				r["password"] = hex.EncodeToString([]byte(password))
				r["has_password"] = has
			}
		}
		out = append(out, r)
	}
	return out
}

// socksScript drives one SOCKS5 exchange and records every byte the client sent.
type socksScript struct {
	method   byte
	authOK   bool
	reply    []byte
	greeting []byte
}

func runSocks(t *testing.T, ln net.Listener, s socksScript, record *[]byte, payload string) {
	conn, err := ln.Accept()
	if err != nil {
		return
	}
	defer conn.Close()
	r := bufio.NewReader(conn)
	read := func(n int) []byte {
		b := make([]byte, n)
		if _, err := io.ReadFull(r, b); err != nil {
			return nil
		}
		*record = append(*record, b...)
		return b
	}
	head := read(2)
	if head == nil {
		return
	}
	read(int(head[1]))
	conn.Write([]byte{5, s.method})
	if s.method == 0xff {
		return
	}
	if s.method == 2 {
		v := read(2)
		if v == nil {
			return
		}
		read(int(v[1]))
		pl := read(1)
		if pl == nil {
			return
		}
		read(int(pl[0]))
		if s.authOK {
			conn.Write([]byte{1, 0})
		} else {
			conn.Write([]byte{1, 1})
			return
		}
	}
	req := read(4)
	if req == nil {
		return
	}
	switch req[3] {
	case 1:
		read(4)
	case 4:
		read(16)
	case 3:
		l := read(1)
		read(int(l[0]))
	}
	read(2)
	conn.Write(s.reply)
	if s.reply[1] == 0 {
		conn.Write([]byte(payload))
		time.Sleep(50 * time.Millisecond)
	}
}

func runConnect(ln net.Listener, response string, record *[]byte) {
	conn, err := ln.Accept()
	if err != nil {
		return
	}
	defer conn.Close()
	r := bufio.NewReader(conn)
	for {
		line, err := r.ReadString('\n')
		*record = append(*record, line...)
		if err != nil || line == "\r\n" {
			break
		}
	}
	conn.Write([]byte(response))
	time.Sleep(50 * time.Millisecond)
}

func dialerVectors(t *testing.T) []map[string]any {
	ok4 := []byte{5, 0, 0, 1, 20, 42, 0, 20, 1, 187}
	type dcase struct {
		name, kind, url, target string
		socks                   socksScript
		response                string
	}
	cases := []dcase{
		{name: "socks5_no_auth", kind: "socks", url: "socks5://HOST", target: "20.42.0.20:443", socks: socksScript{method: 0, reply: ok4}},
		{name: "socks5h_auth", kind: "socks", url: "socks5h://user:pa%3Ass@HOST", target: "20.42.0.20:443", socks: socksScript{method: 2, authOK: true, reply: ok4}},
		{name: "socks5_auth_server_skips", kind: "socks", url: "socks5://user:pass@HOST", target: "20.42.0.20:443", socks: socksScript{method: 0, reply: ok4}},
		{name: "socks5_user_only", kind: "socks", url: "socks5://user@HOST", target: "20.42.0.20:443", socks: socksScript{method: 2, authOK: true, reply: ok4}},
		{name: "socks5_auth_rejected", kind: "socks", url: "socks5://user:bad@HOST", target: "20.42.0.20:443", socks: socksScript{method: 2, authOK: false, reply: ok4}},
		{name: "socks5_no_acceptable", kind: "socks", url: "socks5://HOST", target: "20.42.0.20:443", socks: socksScript{method: 0xff, reply: ok4}},
		{name: "socks5_ipv6_target", kind: "socks", url: "socks5://HOST", target: "[2606:4700::1]:443", socks: socksScript{method: 0, reply: []byte{5, 0, 0, 4, 0x26, 0x06, 0x47, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 187}}},
		{name: "socks5_fqdn_bound", kind: "socks", url: "socks5://HOST", target: "20.42.0.20:443", socks: socksScript{method: 0, reply: []byte{5, 0, 0, 3, 4, 'h', 'o', 's', 't', 1, 187}}},
		{name: "socks5_refused", kind: "socks", url: "socks5://HOST", target: "20.42.0.20:443", socks: socksScript{method: 0, reply: []byte{5, 5, 0, 1, 0, 0, 0, 0, 0, 0}}},
		{name: "http_connect", kind: "connect", url: "http://HOST", target: "20.42.0.20:443", response: "HTTP/1.1 200 Connection established\r\n\r\n"},
		{name: "http_connect_auth", kind: "connect", url: "http://us%40er:p%3Ass@HOST", target: "20.42.0.20:443", response: "HTTP/1.1 200 OK\r\nProxy-Agent: x\r\n\r\nearly"},
		{name: "http_connect_ipv6", kind: "connect", url: "http://HOST", target: "[2606:4700::1]:443", response: "HTTP/1.0 200 OK\r\n\r\n"},
		{name: "http_connect_407", kind: "connect", url: "http://HOST", target: "20.42.0.20:443", response: "HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n"},
		{name: "http_connect_garbage", kind: "connect", url: "http://HOST", target: "20.42.0.20:443", response: "nonsense\r\n\r\n"},
		{name: "socks5_ff_password", kind: "socks", url: "socks5://u:%FF@HOST", target: "20.42.0.20:443", socks: socksScript{method: 2, authOK: true, reply: ok4}},
		{name: "http_connect_ff_password", kind: "connect", url: "http://u:%FF@HOST", target: "20.42.0.20:443", response: "HTTP/1.1 200 OK\r\n\r\n"},
		{name: "http_connect_user_only", kind: "connect", url: "http://u@HOST", target: "20.42.0.20:443", response: "HTTP/1.1 200 OK\r\n\r\n"},
		{name: "lf_only", kind: "connect", url: "http://HOST", target: "20.42.0.20:443", response: "HTTP/1.1 200 OK\n\nafter"},
		{name: "continuation", kind: "connect", url: "http://HOST", target: "20.42.0.20:443", response: "HTTP/1.1 200 OK\r\nX-Test: a\r\n b\r\n\r\n"},
		{name: "bad_content_length", kind: "connect", url: "http://HOST", target: "20.42.0.20:443", response: "HTTP/1.1 200 OK\r\nContent-Length: nope\r\n\r\n"},
		{name: "empty_content_length", kind: "connect", url: "http://HOST", target: "20.42.0.20:443", response: "HTTP/1.1 200 OK\r\nContent-Length: \r\n\r\n"},
		{name: "two_content_lengths", kind: "connect", url: "http://HOST", target: "20.42.0.20:443", response: "HTTP/1.1 200 OK\r\nContent-Length: 1\r\ncontent-length: 2\r\n\r\n"},
		{name: "same_content_lengths", kind: "connect", url: "http://HOST", target: "20.42.0.20:443", response: "HTTP/1.1 200 OK\r\nContent-Length: 3\r\nContent-Length:  3\r\n\r\nabcdef"},
		{name: "te_gzip", kind: "connect", url: "http://HOST", target: "20.42.0.20:443", response: "HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\n\r\n"},
		{name: "te_chunked", kind: "connect", url: "http://HOST", target: "20.42.0.20:443", response: "HTTP/1.1 200 OK\r\nTransfer-Encoding: Chunked\r\n\r\nearly"},
		{name: "te_two", kind: "connect", url: "http://HOST", target: "20.42.0.20:443", response: "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n"},
		{name: "te_gzip_http10", kind: "connect", url: "http://HOST", target: "20.42.0.20:443", response: "HTTP/1.0 200 OK\r\nTransfer-Encoding: gzip\r\n\r\n"},
		{name: "chunked_bad_trailer", kind: "connect", url: "http://HOST", target: "20.42.0.20:443", response: "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTrailer: x, content-length\r\n\r\n"},
		{name: "space_in_key", kind: "connect", url: "http://HOST", target: "20.42.0.20:443", response: "HTTP/1.1 200 OK\r\nBad Key: x\r\n\r\n"},
		{name: "bad_key_byte", kind: "connect", url: "http://HOST", target: "20.42.0.20:443", response: "HTTP/1.1 200 OK\r\nBad(Key): x\r\n\r\n"},
		{name: "missing_colon", kind: "connect", url: "http://HOST", target: "20.42.0.20:443", response: "HTTP/1.1 200 OK\r\nNoColon\r\n\r\n"},
		{name: "leading_space_header", kind: "connect", url: "http://HOST", target: "20.42.0.20:443", response: "HTTP/1.1 200 OK\r\n X: y\r\n\r\n"},
		{name: "ctl_in_value", kind: "connect", url: "http://HOST", target: "20.42.0.20:443", response: "HTTP/1.1 200 OK\r\nX: a\x01b\r\n\r\n"},
		{name: "status_201", kind: "connect", url: "http://HOST", target: "20.42.0.20:443", response: "HTTP/1.1 201 Created\r\n\r\n"},
		{name: "status_plus", kind: "connect", url: "http://HOST", target: "20.42.0.20:443", response: "HTTP/1.1 +20 Odd\r\n\r\n"},
		{name: "status_only_code", kind: "connect", url: "http://HOST", target: "20.42.0.20:443", response: "HTTP/1.1 200\r\n\r\n"},
		{name: "http2_version", kind: "connect", url: "http://HOST", target: "20.42.0.20:443", response: "HTTP/2 200 OK\r\n\r\n"},
		{name: "spaces_before_status", kind: "connect", url: "http://HOST", target: "20.42.0.20:443", response: "HTTP/1.1   200 OK\r\n\r\n"},
		{name: "eof_in_headers", kind: "connect", url: "http://HOST", target: "20.42.0.20:443", response: "HTTP/1.1 200 OK\r\nX: y\r\n"},
		{name: "eof_in_status", kind: "connect", url: "http://HOST", target: "20.42.0.20:443", response: "HTTP/1.1 200 OK"},
		{name: "socks5_empty_host", kind: "socks", url: "socks5://:PORT", target: "20.42.0.20:443", socks: socksScript{method: 0, reply: ok4}},
		{name: "http_empty_host", kind: "connect", url: "http://:PORT", target: "20.42.0.20:443", response: "HTTP/1.1 200 OK\r\n\r\nempty"},
	}
	out := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		ln, err := net.Listen("tcp", "127.0.0.1:0")
		if err != nil {
			t.Fatal(err)
		}
		var record []byte
		done := make(chan struct{})
		go func() {
			defer close(done)
			if c.kind == "socks" {
				runSocks(t, ln, c.socks, &record, "hello")
			} else {
				runConnect(ln, c.response, &record)
			}
		}()
		raw := strings.Replace(c.url, "HOST", ln.Addr().String(), 1)
		raw = strings.Replace(raw, "PORT", strconv.Itoa(ln.Addr().(*net.TCPAddr).Port), 1)
		dialer, _, errBuild := proxyutil.BuildDialer(raw)
		result := map[string]any{"name": c.name, "kind": c.kind, "url": c.url, "target": c.target}
		if c.kind == "socks" {
			result["method"] = int(c.socks.method)
			result["auth_ok"] = c.socks.authOK
			result["reply"] = hex.EncodeToString(c.socks.reply)
		} else {
			result["response"] = c.response
		}
		if errBuild != nil {
			result["error"] = errBuild.Error()
		} else {
			ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
			conn, errDial := dialer.(interface {
				DialContext(context.Context, string, string) (net.Conn, error)
			}).DialContext(ctx, "tcp", c.target)
			cancel()
			if errDial != nil {
				result["error"] = strings.ReplaceAll(errDial.Error(), ln.Addr().String(), "HOST")
			} else {
				conn.SetReadDeadline(time.Now().Add(time.Second))
				buf := make([]byte, 64)
				n, _ := conn.Read(buf)
				result["first_read"] = string(buf[:n])
				conn.Close()
			}
		}
		ln.Close()
		<-done
		sent := string(record)
		sent = strings.ReplaceAll(sent, ln.Addr().String(), "HOST")
		if c.kind == "socks" {
			result["sent"] = hex.EncodeToString(record)
		} else {
			result["sent"] = sent
		}
		out = append(out, result)
	}
	return out
}

func TestRSFixLiveTunnelVectors(t *testing.T) {
	dir := os.Getenv("RSFIX_OUT")
	if dir == "" {
		t.Skip("RSFIX_OUT not set")
	}
	data := map[string]any{
		"prepare": prepareVectors(),
		"frames":  frameVectors(t),
		"targets": targetVectors(),
		"proxies": parseVectors(),
		"dialers": dialerVectors(t),
	}
	raw, err := json.MarshalIndent(data, "", " ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "live_tunnel_vectors.json"), raw, 0o644); err != nil {
		t.Fatal(err)
	}
}
