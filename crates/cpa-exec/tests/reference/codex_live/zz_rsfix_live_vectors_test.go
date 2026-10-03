package live

// Golden vectors for cliproxy-rs. Runs only with RSFIX_OUT set; writes JSON produced
// by the real Go functions of this package.

import (
	"encoding/base64"
	"encoding/json"
	"io"
	"mime/quotedprintable"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
	"unicode/utf8"
)

type goldenVector struct {
	Fn  string         `json:"fn"`
	In  map[string]any `json:"in"`
	Out map[string]any `json:"out"`
}

func gb(data []byte) string {
	if utf8.Valid(data) {
		return string(data)
	}
	return "b64:" + base64.StdEncoding.EncodeToString(data)
}

func gerr(err error) any {
	if err == nil {
		return nil
	}
	return err.Error()
}

func capture(run func() map[string]any) (out map[string]any) {
	defer func() {
		if recovered := recover(); recovered != nil {
			out = map[string]any{"panic": true}
		}
	}()
	return run()
}

func mp(boundary string, parts ...string) string {
	body := ""
	for index := 0; index+1 < len(parts); index += 2 {
		body += "--" + boundary + "\r\n" + parts[index] + "\r\n\r\n" + parts[index+1] + "\r\n"
	}
	return body + "--" + boundary + "--\r\n"
}

func disposition(name string) string {
	return "Content-Disposition: form-data; name=\"" + name + "\""
}

func TestRSFixLiveVectors(t *testing.T) {
	dir := os.Getenv("RSFIX_OUT")
	if dir == "" {
		t.Skip("RSFIX_OUT not set")
	}
	var vectors []goldenVector
	add := func(fn string, in map[string]any, run func() map[string]any) {
		vectors = append(vectors, goldenVector{Fn: fn, In: in, Out: capture(run)})
	}

	const sdp = "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\n"
	const b = "rust-golden-boundary"
	multipartType := "multipart/form-data; boundary=" + b
	type bodyCase struct {
		body        string
		contentType string
	}
	bodies := []bodyCase{
		{`{"sdp":"v=0","model":"gpt-realtime"}`, "application/json"},
		{`{"sdp":"v=0","session":{"model":" m1 "},"model":"m2"}`, "application/json"},
		{`{"sdp":"v=0","session":{"model":""},"model":"m2"}`, "application/json"},
		{`{"MODEL":"upper","Session":{"Model":"folded"}}`, "application/json; charset=utf-8"},
		{`{"model":5,"session":{"model":"x"}}`, "application/json"},
		{`{"session":"str","model":"m"}`, "application/json"},
		{`{"session":null,"model":"m"}`, "application/json"},
		{`{"model":"a","model":"b"}`, "application/json"},
		{`{"sdp":"<v>&","z":1.50,"a":[1, 2],"session":{"b":1e3,"model":"gpt-realtime-mini","a":"\u00e9"}}`, "application/json"},
		{`{"sdp":"v=0","other":true}`, "application/json"},
		{`{"sdp":5,"model":"m"}`, "application/json"},
		{`{"sdp":null,"SDP":"case-folded"}`, "application/json"},
		{`{"sdp":["x"],"session":{"model":["y"]}}`, "application/json"},
		{`{"sdp":"a","sdp":"  "}`, "application/json"},
		{`{"sdp":"\u003c\ud800x","session":{}}`, "application/json"},
		{`{"sdp":"x","session":{"model":"m","model":"n"}}`, "application/json"},
		{`{"sdp":"x","session":[1]}`, "application/json"},
		{`{"sdp":"x","session":true}`, "application/json"},
		{`{"sdp":"x","session":1}`, "application/json"},
		{`true`, "application/json"},
		{`12`, "application/json"},
		{`{"a":1}x`, "application/json"},
		{`{"a":tru}`, "application/json"},
		{"{\"sdp\":\"\xff\"}", "application/json"},
		{` {"model":"gpt-4o-realtime-preview"} `, "application/json"},
		{`[1,2]`, "application/json"},
		{`"text"`, "application/json"},
		{`null`, "application/json"},
		{`{"model":"x"`, "application/json"},
		{``, "application/json"},
		{`   `, "application/json"},
		{sdp, "application/sdp"},
		{sdp, "Application/SDP; charset=x"},
		{sdp, "text/plain"},
		{sdp, ""},
		{sdp, "foo"},
		{sdp, "application/octet-stream"},
		{`{"model":"m"}`, ""},
		{mp(b, disposition("sdp")+"\r\nContent-Type: application/sdp", sdp, disposition("session")+"\r\nContent-Type: application/json", `{"model":"future-live","instructions":"<hi>"}`), multipartType},
		{mp(b, disposition("sdp"), sdp), multipartType},
		{mp(b, disposition("sdp"), sdp), "MULTIPART/FORM-DATA; boundary=" + b},
		{mp(b, disposition("session"), `{"model":"m"}`), multipartType},
		{mp(b, disposition("sdp"), sdp, disposition("session"), `{"model":`), multipartType},
		{mp(b, disposition("sdp"), sdp, disposition("session"), ``), multipartType},
		{mp(b, disposition("sdp"), sdp, disposition("session"), `null`), multipartType},
		{mp(b, disposition("sdp"), sdp, disposition("session"), `"str"`), multipartType},
		{mp(b, disposition("sdp"), sdp, disposition("session"), `{"type":"realtime","model":"gpt-realtime"}`), multipartType},
		{mp(b, disposition("sdp"), "first", disposition("sdp"), "second", disposition("other"), "x"), multipartType},
		{mp(b, disposition("sdp")+"; filename=\"offer.sdp\"", sdp), multipartType},
		{mp(b, disposition("sdp"), "", disposition("session"), `{ "model" : "spaced" , "x" : [ 1 ] }`), multipartType},
		{mp(b, disposition("sdp"), "v=0\xff\xfe"), multipartType},
		{mp(b, "Content-Disposition: attachment; name=\"sdp\"", sdp), multipartType},
		{"preamble line\r\n" + mp(b, disposition("sdp"), sdp) + "epilogue", multipartType},
		{"--" + b + "\n" + disposition("sdp") + "\n\n" + "lf-only\n--" + b + "--\n", multipartType},
		{"--" + b + "\r\n" + disposition("sdp") + "\r\n\r\n" + sdp, multipartType},
		{"--" + b + "\r\n" + disposition("sdp") + "\r\n", multipartType},
		{"--" + b + "\r\nbroken header line\r\n\r\nx\r\n--" + b + "--\r\n", multipartType},
		{"no boundary at all", multipartType},
		{"", multipartType},
		{mp(b, disposition("sdp")+"\r\nContent-Transfer-Encoding: quoted-printable", "v=3D0=0D=0Ao=3d- 1=\r\n 2"), multipartType},
		{mp(b, disposition("sdp")+"\r\nCONTENT-TRANSFER-ENCODING: Quoted-Printable", "a=ZZb =\r\nc\td  \r\ne=\t\r\n"), multipartType},
		{mp(b, disposition("sdp"), "x", disposition("session")+"\r\nContent-Transfer-Encoding: quoted-printable", "{=22model=22:=22qp-model=22}"), multipartType},
		{mp(b, disposition("sdp")+"\r\nContent-Transfer-Encoding: quoted-printable", "bad \x01 byte"), multipartType},
		{mp(b, disposition("sdp")+"\r\nContent-Transfer-Encoding: quoted-printable", "soft= junk"), multipartType},
		{mp(b, disposition("sdp")+"\r\nContent-Transfer-Encoding: quoted-printable", "hex=4"), multipartType},
		{mp(b, disposition("sdp")+"\r\nContent-Transfer-Encoding: quoted-printable", "eq=\rx"), multipartType},
		{mp(b, disposition("sdp")+"\r\nContent-Transfer-Encoding: base64", "djA="), multipartType},
		{"--" + b + "\r\n" + disposition("sdp") + "\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\nv=3D0 no closing boundary", multipartType},
		{"--" + b + "\r\n" + disposition("sdp") + "\r\n" + strings.Repeat("X: y\r\n", 9999) + "\r\nok\r\n--" + b + "--\r\n", multipartType},
		{"--" + b + "\r\n" + disposition("sdp") + "\r\n" + strings.Repeat("X: y\r\n", 10000) + "\r\ntoo many\r\n--" + b + "--\r\n", multipartType},
		{"--" + b + "\r\n " + strings.Repeat("x", 90) + "\r\n\r\nv\r\n--" + b + "--\r\n", multipartType},
		{mp(b, disposition("sdp"), sdp), "multipart/form-data"},
		{mp(b, disposition("sdp"), sdp), "multipart/form-data; boundary=\"\""},
		{mp(b, disposition("sdp"), sdp), "multipart/mixed; boundary=" + b},
		{mp(b, disposition("sdp"), sdp), "multipart/form-data; boundary=" + b + "; boundary=other"},
	}
	for _, item := range bodies {
		body, contentType := item.body, item.contentType
		add("prepare", map[string]any{"body": gb([]byte(body)), "content_type": contentType}, func() map[string]any {
			encoded, ct, model, err := prepareCallRequest([]byte(body), contentType)
			return map[string]any{"body": gb(encoded), "content_type": ct, "model": model, "err": gerr(err)}
		})
		add("model_from_json", map[string]any{"body": gb([]byte(body))}, func() map[string]any {
			return map[string]any{"model": modelFromJSON([]byte(body))}
		})
		add("call_request_sdp", map[string]any{"body": gb([]byte(body)), "content_type": contentType}, func() map[string]any {
			offer, err := callRequestSDP([]byte(body), contentType)
			return map[string]any{"sdp": offer, "err": gerr(err)}
		})
		add("replace_sdp", map[string]any{"body": gb([]byte(body)), "content_type": contentType, "sdp": "v=0\r\no=gw <&>\r\n"}, func() map[string]any {
			encoded, ct, err := replaceCallRequestSDP([]byte(body), contentType, "v=0\r\no=gw <&>\r\n")
			return map[string]any{"body": gb(encoded), "content_type": ct, "err": gerr(err)}
		})
		add("response_sdp", map[string]any{"body": gb([]byte(body)), "content_type": contentType}, func() map[string]any {
			answer, err := callResponseSDP([]byte(body), contentType)
			return map[string]any{"sdp": answer, "err": gerr(err)}
		})
		add("rewrite_model", map[string]any{"body": gb([]byte(body)), "content_type": contentType, "model": "gpt-realtime"}, func() map[string]any {
			encoded, model, err := rewriteCallRequestModel([]byte(body), contentType, "gpt-realtime")
			return map[string]any{"body": gb(encoded), "model": model, "err": gerr(err)}
		})
		for _, session := range []string{"", `{"type":"realtime","model":"gpt-live-1-codex","instructions":"<help>"}`, `{"model":"gpt-realtime-2025","a":{"z":1,"b":2}}`} {
			session := session
			add("pipeline", map[string]any{"body": gb([]byte(body)), "content_type": contentType, "session": session}, func() map[string]any {
				var raw json.RawMessage
				if session != "" {
					raw = json.RawMessage(session)
				}
				encoded, ct, model, err := prepareCallRequest([]byte(body), contentType)
				if err == nil {
					encoded, ct, model, err = applyClientSecretCallSession(encoded, ct, model, raw)
				}
				if err == nil {
					encoded, model, err = rewriteCallRequestModel(encoded, ct, model)
				}
				return map[string]any{"body": gb(encoded), "content_type": ct, "model": model, "err": gerr(err)}
			})
		}
	}

	for _, encoded := range []string{"", "plain", "a=3Db", "a=3db", "=", "a=", "a=\r\nb", "a=\nb", "a= \t\r\nb", "a=  x", "a=Z", "a=ZZ", "a=4", "a=4G", "a=\r", "a=\rb\n",
		"trail  \t\r\nnext", "keep\tTab\n", "\x01", "\x7f", "\xc3\xa9", "=E2=82=AC", "line1\r\nline2\n", "a==3D", "x=\n"} {
		encoded := encoded
		add("quoted_printable", map[string]any{"body": gb([]byte(encoded))}, func() map[string]any {
			decoded, err := io.ReadAll(quotedprintable.NewReader(strings.NewReader(encoded)))
			if err != nil {
				return map[string]any{"err": err.Error()}
			}
			return map[string]any{"body": gb(decoded), "err": nil}
		})
	}

	for _, model := range []string{"", " ", "gpt-realtime", "GPT-Realtime", "gpt-realtime-mini", "gpt-realtime-2025-08-28", "gpt-realtimex", "gpt-4o-realtime-preview", "gpt-4o-mini-REALTIME-PREVIEW-2024", " custom-live ", "gpt-live-1-codex"} {
		model := model
		add("codex_model", map[string]any{"model": model}, func() map[string]any {
			return map[string]any{"model": codexRealtimeModel(model)}
		})
	}

	for _, location := range []string{"", "call-123", " call-123 ", "/v1/live/rtc_1", "/v1/realtime/calls/rtc_2", "/v1/realtime?intent=quicksilver&call_id=rtc_3", "https://api.openai.com/v1/realtime/calls/rtc_4?x=1", "/v1/realtime/calls/rtc_5/", "/foo/bar", "/live/", "rtc_6", "/v1/live/a b", "/v1/live/a%20b", "?call_id=bad id", "?call_id=", "/calls/x?call_id=q", "live/x", "/x/live/y/z", "://bad", "/v1/live/" + string(make([]byte, 0)) + "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "/v1/live/%2F"} {
		location := location
		add("call_id_from_location", map[string]any{"location": location}, func() map[string]any {
			return map[string]any{"call_id": callIDFromLocation(location)}
		})
	}

	for _, session := range []string{"", "  null \n", "null", "{}", `{"type":"realtime"}`, `{"type":""}`, `{"type":"  "}`, `{"type":"transcription","model":"gpt-4o-transcribe"}`, `{"type":5}`, `{"model":"  "}`, `{"model":1}`, `{"model":"gpt-realtime-mini","instructions":"<b>&</b>"}`, `{"model":"custom","n":12345678901234567890,"f":1.0,"e":1e3,"s":"\u00e9\ud83d\ude00"}`, `[]`, `"x"`, `{"a":`, `{"z":1,"a":{"y":2,"b":null}}`, `{"type":"realtime","model":"gpt-realtime","dup":1,"dup":2}`} {
		session := session
		add("normalize_secret", map[string]any{"session": session}, func() map[string]any {
			client, upstream, err := normalizeClientSecretSession(json.RawMessage(session))
			return map[string]any{"client": gb(client), "upstream": gb(upstream), "err": gerr(err)}
		})
		add("session_response", map[string]any{"session": session}, func() map[string]any {
			client, _, err := normalizeClientSecretSession(json.RawMessage(session))
			if err != nil {
				return map[string]any{"skip": true}
			}
			response, errResponse := realtimeSessionResponse(client, "sess_fixed", time.Unix(1700000000, 0))
			return map[string]any{"body": gb(response), "err": gerr(errResponse)}
		})
		add("session_update", map[string]any{"session": session}, func() map[string]any {
			update, err := realtimeSessionUpdate(json.RawMessage(session))
			return map[string]any{"body": gb(update), "err": gerr(err)}
		})
	}

	for _, style := range []sidebandStyle{sidebandFrameless, sidebandRealtimeCalls, sidebandRealtimeQuery} {
		for _, base := range []string{defaultSidebandAPIBaseURL, "ws://127.0.0.1:9/v1/", "wss://api.openai.com/v1//"} {
			style, base := style, base
			add("sideband_url", map[string]any{"style": int(style), "base": base, "call_id": "rtc-_1"}, func() map[string]any {
				url := buildSidebandURL(base, style, "rtc-_1")
				return map[string]any{"url": url, "http_url": websocketHTTPURL(url)}
			})
		}
	}
	for _, model := range []string{"gpt-realtime", " a b&c ", ""} {
		model := model
		add("direct_url", map[string]any{"model": model}, func() map[string]any {
			handler := &Handler{sidebandAPIBaseURL: defaultSidebandAPIBaseURL}
			return map[string]any{"url": handler.directRealtimeURL(model), "hangup_base": handler.realtimeHTTPBaseURL()}
		})
	}

	type lifetimeCase struct {
		anchor  string
		seconds int64
		set     bool
	}
	for _, item := range []lifetimeCase{{set: false}, {"", 60, true}, {"created_at", 9, true}, {"created_at", 10, true}, {"created_at", 7200, true}, {"created_at", 7201, true}, {"other", 60, true}, {"", 0, true}} {
		item := item
		add("lifetime", map[string]any{"set": item.set, "anchor": item.anchor, "seconds": item.seconds}, func() map[string]any {
			var expires *struct {
				Anchor  string `json:"anchor"`
				Seconds int64  `json:"seconds"`
			}
			if item.set {
				expires = &struct {
					Anchor  string `json:"anchor"`
					Seconds int64  `json:"seconds"`
				}{item.anchor, item.seconds}
			}
			lifetime, err := clientSecretLifetime(expires)
			return map[string]any{"seconds": int64(lifetime / time.Second), "err": gerr(err)}
		})
	}

	for _, body := range []string{"", "  ", "null", "{}", "[]", `"x"`, `{"session":{"a":1}}`, `{"session":null}`, `{"SESSION":5,"Expires_After":{"Anchor":"created_at","SECONDS":60}}`,
		`{"expires_after":null}`, `{"expires_after":{}}`, `{"expires_after":{"seconds":"60"}}`, `{"expires_after":{"seconds":60.0}}`, `{"expires_after":{"seconds":6e1}}`,
		`{"expires_after":{"seconds":99999999999999999999}}`, `{"expires_after":{"anchor":5}}`, `{"expires_after":[1]}`, `{"expires_after":{"seconds":-3,"anchor":null}}`,
		`{"expires_after":{"seconds":30},"expires_after":{"anchor":"created_at"}}`, `{"session":{}, "x":[1,2,{"y":true}]}`, `{"session":{}`, `{"a":1}x`} {
		body := body
		add("secret_request", map[string]any{"body": body}, func() map[string]any {
			var request clientSecretCreateRequest
			if len(strings.TrimSpace(body)) > 0 {
				if errUnmarshal := json.Unmarshal([]byte(body), &request); errUnmarshal != nil {
					return map[string]any{"ok": false}
				}
			}
			out := map[string]any{"ok": true, "session": gb(request.Session)}
			if request.ExpiresAfter != nil {
				out["anchor"] = request.ExpiresAfter.Anchor
				out["seconds"] = request.ExpiresAfter.Seconds
			}
			return out
		})
	}

	for _, value := range []string{"", "Bearer ek_abc", "bearer  ek_abc ", "BEARER ek_x", "Bearer", "Bearer ", "Basic ek_abc", "ek_abc", " Bearer ek_pad"} {
		value := value
		add("bearer", map[string]any{"authorization": value}, func() map[string]any {
			request, _ := http.NewRequest(http.MethodGet, "http://x/", nil)
			if value != "" {
				request.Header.Set("Authorization", value)
			}
			return map[string]any{"token": bearerToken(request)}
		})
	}

	for _, callID := range []string{"", "a", "rtc_ABC-1", "a b", "a/b", "é", string(make([]byte, 0)) + "0123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789"} {
		callID := callID
		add("call_id_valid", map[string]any{"call_id": callID}, func() map[string]any {
			return map[string]any{"valid": callIDPattern.MatchString(callID)}
		})
	}

	encoded, errMarshal := json.MarshalIndent(vectors, "", " ")
	if errMarshal != nil {
		t.Fatal(errMarshal)
	}
	if errWrite := os.WriteFile(filepath.Join(dir, "codex_live_go.json"), encoded, 0o644); errWrite != nil {
		t.Fatal(errWrite)
	}
}
