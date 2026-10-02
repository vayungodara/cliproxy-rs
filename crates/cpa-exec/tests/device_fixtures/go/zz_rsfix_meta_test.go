package executor

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"os"
	"path/filepath"
	"testing"
	"time"

	metaauth "github.com/router-for-me/CLIProxyAPI/v8/internal/auth/meta"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/buildinfo"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	sdkAuth "github.com/router-for-me/CLIProxyAPI/v8/sdk/auth"
	cliproxyauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
	cliproxyexecutor "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/executor"
	sdktranslator "github.com/router-for-me/CLIProxyAPI/v8/sdk/translator"
)

func TestRSFixMeta(t *testing.T) {
	rsfixOut(t)
	buildinfo.Version = "0.1.0"
	created := `data: {"type":"response.created","response":{"id":"resp_m","status":"in_progress"}}`
	itemDone := `data: {"type":"response.output_item.done","output_index":0,"item":{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"hi there"}]}}`
	completedEmpty := `data: {"type":"response.completed","response":{"id":"resp_m","status":"completed","model":"muse-spark-1.3","output":[],"usage":{"input_tokens":5,"output_tokens":2,"total_tokens":7}}}`
	stream := "event: response.created\n" + created + "\n\nevent: response.output_item.done\n" + itemDone + "\n\nevent: response.completed\n" + completedEmpty + "\n\n"
	apiMeta := map[string]any{"type": "meta", "auth_kind": "oauth", "access_token": "meta-key-fixture", "api_key": "meta-key-fixture", "dca_token": "dca:fixture"}
	oauthAttrs := map[string]string{"auth_kind": "oauth"}
	body := `{"model":"muse-spark-1.3(high)","input":"hello","instructions":null,"temperature":0.2,"max_output_tokens":99,"prompt_cache_retention":"24h","safety_identifier":"x","client_metadata":{"a":1},"user":"u1","service_tier":"fast","stream_options":{"include_obfuscation":false},"tools":[{"type":"web_search_preview","search_content_types":["text"]},{"type":"namespace","name":"n","tools":[{"type":"web_search","search_content_types":["image"]}]}],"reasoning":{"summary":"auto"}}`
	mintOK := `{"api_key":"minted-key","base_url":"","user_email":"u@example.com","user_full_name":"U Ser","subs_tier_name":"Plus","subs_tier_id":"","is_subs_active":true,"has_payment_method":false}`
	cases := []struct {
		name      string
		stream    bool
		count     bool
		refresh   bool
		alt       string
		body      string
		meta      map[string]any
		attrs     map[string]string
		noBase    bool
		headers   http.Header
		mint      bool
		responses []rsfixResponse
		dynamic   func(url string) []rsfixResponse
	}{
		{name: "responses-nonstream-collected", body: body, meta: apiMeta, responses: []rsfixResponse{sseResp(stream)}},
		{name: "responses-stream", stream: true, body: body, meta: apiMeta, responses: []rsfixResponse{sseResp(stream)}},
		{name: "responses-nonstream-json-body", body: `{"model":"muse-spark-1.2","input":[{"type":"message","role":"system","content":[{"type":"input_text","text":"sys"}]},{"type":"function_call","call_id":"c1","name":"f","arguments":" "}]}`, meta: apiMeta,
			responses: []rsfixResponse{jsonResp(200, `{"id":"resp_j","object":"response","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"ok"}]}],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2,"output_tokens_details":null}}`)}},
		{name: "error-429-subscription-quota", body: `{"model":"muse-spark-1.3","input":"hi"}`, meta: apiMeta,
			responses: []rsfixResponse{jsonResp(429, `{"error":{"message":"Subscription quota exhausted","code":"rate_limit_exceeded","resets_at":4102444800}}`)}},
		{name: "error-429-plain", body: `{"model":"muse-spark-1.3","input":"hi"}`, meta: apiMeta,
			responses: []rsfixResponse{jsonResp(429, `{"error":{"message":"slow down","code":"rate_limit_exceeded"}}`)}},
		{name: "error-404-model", body: `{"model":"muse-spark-1.3","input":"hi"}`, meta: apiMeta,
			responses: []rsfixResponse{jsonResp(404, `{"error":{"message":"model not found"}}`)}},
		{name: "error-404-resets", stream: true, body: `{"model":"muse-spark-1.3","input":"hi"}`, meta: apiMeta,
			responses: []rsfixResponse{jsonResp(404, `{"error":{"message":"not yet","resets_at":4102444800}}`)}},
		{name: "stream-error-event", stream: true, body: `{"model":"muse-spark-1.3","input":"hi"}`, meta: apiMeta,
			responses: []rsfixResponse{sseResp("event: response.created\n" + created + "\n\nevent: error\ndata: {\"type\":\"error\",\"error\":{\"code\":503,\"message\":\"overloaded\"}}\n\n")}},
		{name: "nonstream-response-failed", body: `{"model":"muse-spark-1.3","input":"hi"}`, meta: apiMeta,
			responses: []rsfixResponse{sseResp(created + "\n" + `data: {"type":"response.failed","response":{"id":"resp_m"},"error":{"code":"server_error","message":"boom"}}` + "\n")}},
		{name: "nonstream-incomplete-fallback-items", body: `{"model":"muse-spark-1.3","input":"hi"}`, meta: apiMeta,
			responses: []rsfixResponse{sseResp(`data: {"type":"response.output_item.done","item":{"type":"reasoning","summary":[]}}` + "\n" + `data: {"type":"response.output_item.done","output_index":1,"item":{"type":"message","content":[]}}` + "\n" + `data: {"type":"response.incomplete","response":{"id":"r","status":"incomplete","usage":{"input_tokens":1,"output_tokens_details":{"reasoning_tokens":null}}}}` + "\n")}},
		{name: "nonstream-disconnect-before-completed", body: `{"model":"muse-spark-1.3","input":"hi"}`, meta: apiMeta,
			responses: []rsfixResponse{sseResp("event: response.created\n" + created + "\n\n")}},
		{name: "stream-plain-json-lines", stream: true, body: `{"model":"muse-spark-1.3","input":"hi"}`, meta: apiMeta,
			responses: []rsfixResponse{jsonResp(200, "{\"type\":\"response.created\",\"response\":{}}\r\ndata:{\"type\":\"response.in_progress\",\"response\":{\"id\":\"x\"}}\ndata: [DONE]")}},
		{name: "compact-rejected", alt: "responses/compact", body: `{"model":"muse-spark-1.3","input":"hi"}`, meta: apiMeta},
		{name: "count-tokens", count: true, body: `{"model":"muse-spark-1.3","instructions":"Be brief.","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello world"}]},{"type":"function_call","name":"lookup","arguments":"{\"q\":1}"},{"type":"function_call_output","output":"result text"}],"tools":[{"type":"function","name":"lookup","description":"Find things","parameters":{"type":"object"}}],"text":{"format":{"type":"json_schema","name":"out","schema":{"type":"object"}}}}`, meta: apiMeta},
		{name: "config-apikey-headers", body: `{"model":"muse-spark-1.3","input":"hi"}`, noBase: true,
			attrs:   map[string]string{"api_key": "cfg-key", "source": "config:meta[abc]", "header:X-Team": "blue", "header:X-Fwd": "$X-Client-Req", "header:X-Missing": "$X-Absent"},
			headers: http.Header{"X-Client-Req": {"r1"}},
			meta:    map[string]any{}, responses: []rsfixResponse{sseResp(stream)}},
		{name: "config-apikey-dca-only", body: `{"model":"muse-spark-1.3","input":"hi"}`,
			attrs: map[string]string{"api_key": "dca:cfg", "source": "config:meta[abc]"}, meta: map[string]any{}},
		{name: "remint-from-dca", body: `{"model":"muse-spark-1.1","input":"hi","reasoning":{"effort":"minimal"}}`, mint: true,
			meta:      map[string]any{"type": "meta", "auth_kind": "oauth", "access_token": "dca:only", "dca_token": "dca:only", "expired": "2026-01-01T00:00:00Z", "subs_tier_id": "old"},
			responses: []rsfixResponse{jsonResp(200, mintOK), sseResp(stream)}},
		{name: "remint-access-token-dca-minted-base", body: `{"model":"muse-spark-1.3","input":"hi"}`, mint: true, noBase: true,
			meta: map[string]any{"type": "meta", "access_token": " dca:from-access "},
			dynamic: func(url string) []rsfixResponse {
				return []rsfixResponse{jsonResp(200, `{"api_key":"k2","base_url":" `+url+`/v2 ","user_email":"","subs_tier_name":"","subs_tier_id":"tid","is_subs_active":false,"has_payment_method":true}`), sseResp(stream)}
			}},
		{name: "remint-failure", body: `{"model":"muse-spark-1.3","input":"hi"}`, mint: true,
			meta:      map[string]any{"type": "meta", "dca_token": "dca:bad"},
			responses: []rsfixResponse{jsonResp(401, `{"error":"invalid dca"}`)}},
		{name: "refresh-direct", refresh: true, mint: true,
			meta:      map[string]any{"type": "meta", "api_key": "old-key", "access_token": "old-key", "dca_token": "dca:refresh", "email": "keep@x", "name": "Keep", "subs_tier_name": "Old", "custom": 1},
			responses: []rsfixResponse{jsonResp(200, mintOK)}},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			srv := newRSFixServer(t, false, tc.responses...)
			if tc.dynamic != nil {
				srv.responses = tc.dynamic(srv.URL())
			}
			responses := append([]rsfixResponse(nil), srv.responses...)
			t.Setenv("META_MINT_URL", srv.URL()+"/muse-code/key")
			meta := map[string]any{}
			for k, v := range tc.meta {
				meta[k] = v
			}
			attrs := map[string]string{}
			for k, v := range oauthAttrs {
				attrs[k] = v
			}
			if tc.attrs != nil {
				attrs = map[string]string{}
				for k, v := range tc.attrs {
					attrs[k] = v
				}
			}
			if !tc.noBase {
				meta["base_url"] = srv.URL() + "/v1"
			} else if tc.attrs != nil {
				attrs["base_url"] = srv.URL() + "/v1"
			}
			recorded := map[string]any{}
			for k, v := range meta {
				recorded[k] = v
			}
			recordedAttrs := map[string]string{}
			for k, v := range attrs {
				recordedAttrs[k] = v
			}
			auth := &cliproxyauth.Auth{ID: "meta-fixture.json", Provider: "meta", Attributes: attrs, Metadata: meta}
			req := cliproxyexecutor.Request{Model: modelOf(tc.body), Payload: []byte(tc.body)}
			opts := cliproxyexecutor.Options{SourceFormat: sdktranslator.FormatOpenAIResponse, Stream: tc.stream, Alt: tc.alt, OriginalRequest: []byte(tc.body), Headers: tc.headers}
			exec := NewMetaExecutor(&config.Config{})
			ctx := context.Background()
			var down rsfixDownstream
			var execErr error
			switch {
			case tc.refresh:
				_, execErr = exec.Refresh(ctx, auth)
				down.ErrStatus, down.ErrBody = rsfixStatus(execErr)
			case tc.count:
				resp, err := exec.CountTokens(ctx, auth, req, opts)
				execErr = err
				down.ErrStatus, down.ErrBody = rsfixStatus(err)
				down.Body = string(resp.Payload)
			case tc.stream:
				result, err := exec.ExecuteStream(ctx, auth, req, opts)
				execErr = err
				down.ErrStatus, down.ErrBody = rsfixStatus(err)
				if result != nil {
					for chunk := range result.Chunks {
						if chunk.Err != nil {
							status, msg := rsfixStatus(chunk.Err)
							down.StreamErr = msg
							down.ErrStatus = status
							execErr = chunk.Err
							continue
						}
						down.Chunks = append(down.Chunks, string(chunk.Payload))
					}
				}
			default:
				resp, err := exec.Execute(ctx, auth, req, opts)
				execErr = err
				down.ErrStatus, down.ErrBody = rsfixStatus(err)
				down.Body = string(resp.Payload)
			}
			extra := map[string]any{}
			if tc.mint {
				extra["metadata_after"] = auth.Metadata
			}
			if execErr != nil {
				type retryAfter interface{ RetryAfter() *time.Duration }
				type scoped interface{ IsCredentialScoped() bool }
				var ra retryAfter
				if errors.As(execErr, &ra) && ra.RetryAfter() != nil {
					extra["retry_after_secs"] = ra.RetryAfter().Seconds()
					extra["now_unix"] = time.Now().Unix()
				}
				var sc scoped
				extra["credential_scoped"] = errors.As(execErr, &sc) && sc.IsCredentialScoped()
			}
			request := map[string]any{"source": "openai-response", "model": modelOf(tc.body), "stream": tc.stream, "body": tc.body}
			if tc.alt != "" {
				request["alt"] = tc.alt
			}
			if tc.count {
				request["count"] = true
			}
			if tc.refresh {
				request["refresh"] = true
			}
			if tc.headers != nil {
				var hs [][2]string
				for k, vs := range tc.headers {
					hs = append(hs, [2]string{k, vs[0]})
				}
				request["headers"] = hs
			}
			rsfixWrite(t, "meta", rsfixFixture{Name: tc.name, Credential: recorded, Attributes: recordedAttrs, Request: request, Responses: responses, Upstream: srv.Captured(), Downstream: down, Extra: extra})
		})
	}
}

func modelOf(body string) string {
	var v struct {
		Model string `json:"model"`
	}
	_ = json.Unmarshal([]byte(body), &v)
	return v.Model
}

func TestRSFixMetaLogin(t *testing.T) {
	rsfixOut(t)
	cases := []struct {
		name      string
		stale     string
		email     string
		responses []rsfixResponse
	}{
		{name: "login", email: "Some.User+tag@Example.com",
			stale: `{"type":"meta","api_key":"stale","dca_expires_at":5,"dca_expired":"old","custom_setting":"from-disk","prefix":"team","disabled":true,"api-key":"alias-stale","proxy-url":"socks5://p","expires_in":1,"note":"n"}`,
			responses: []rsfixResponse{
				jsonResp(200, `{"device_code":"meta-dev-1","user_code":"WXYZ-1234","verification_uri":"https://www.meta.ai/device","verification_uri_complete":"","expires_in":600,"interval":1}`),
				jsonResp(400, `{"error":"authorization_pending"}`),
				jsonResp(200, `{"access_token":"dca:login-token","token_type":"bearer","expires_in":7200}`),
				jsonResp(200, `{"api_key":"mk-login","base_url":"https://api.meta.ai/v1","user_email":"Some.User+tag@Example.com","user_full_name":"Some <User>","subs_tier_name":"Free","subs_tier_id":"t0","is_subs_active":false,"has_payment_method":true}`),
			}},
		{name: "login-no-mint",
			responses: []rsfixResponse{
				jsonResp(200, `{"device_code":"meta-dev-2","user_code":"AB-CD","verification_uri":"https://www.meta.ai/device","verification_uri_complete":"https://www.meta.ai/device?code=AB-CD","expires_in":0,"interval":0}`),
				jsonResp(200, `{"access_token":"dca:nomint","token_type":"","expires_in":3600}`),
				jsonResp(500, `upstream down`),
			}},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			srv := newRSFixServer(t, false, tc.responses...)
			t.Setenv("META_MINT_URL", srv.URL()+"/muse-code/key")
			previous := http.DefaultTransport
			http.DefaultTransport = rsfixRedirect{host: srv.ln.Addr().String()}
			defer func() { http.DefaultTransport = previous }()
			authDir := t.TempDir()
			if tc.stale != "" {
				_ = os.WriteFile(filepath.Join(authDir, metaauth.CredentialFileName(tc.email, "")), []byte(tc.stale), 0o600)
			}
			manager := sdkAuth.NewManager(sdkAuth.NewFileTokenStore(), sdkAuth.NewMetaAuthenticator())
			before := time.Now()
			record, saved, err := manager.Login(context.Background(), "meta", &config.Config{AuthDir: authDir}, &sdkAuth.LoginOptions{NoBrowser: true, Metadata: map[string]string{}})
			if err != nil {
				t.Fatal(err)
			}
			raw, _ := os.ReadFile(saved)
			rsfixWrite(t, "meta", rsfixFixture{Name: tc.name, Credential: map[string]any{}, Request: map[string]any{"login": "meta", "stale_file": tc.stale}, Responses: tc.responses, Upstream: srv.Captured(), Extra: map[string]any{
				"file_name": filepath.Base(saved), "file_raw": string(raw), "label": record.Label, "before_unix": before.Unix(), "disabled": record.Disabled,
			}})
		})
	}
}

func TestRSFixMetaWriter(t *testing.T) {
	out := rsfixOut(t)
	_ = out
	dir := t.TempDir()
	type writerCase struct {
		Name     string         `json:"name"`
		Disk     string         `json:"disk"`
		Storage  map[string]any `json:"storage"`
		Snapshot map[string]any `json:"snapshot"`
		Out      string         `json:"out"`
	}
	var cases []writerCase
	for _, tc := range []writerCase{
		{Name: "no-snapshot-inherits-settings", Disk: `{"type":"meta","api_key":"stale","expires_in":10,"expired":"x","custom":"disk","email":"old@x","prefix":"p"}`,
			Storage: map[string]any{"access_token": "new", "token_type": "bearer", "expires_in": 0, "dca_expires_at": 0, "email": "new@x"}},
		{Name: "snapshot-wins-and-deletes", Disk: `{"type":"meta","custom":"disk","prefix":"p"}`,
			Storage:  map[string]any{"access_token": "new", "api_key": "k", "expires_in": 3600, "dca_expires_at": 1790000000, "base_url": "https://b", "name": "N <&>"},
			Snapshot: map[string]any{"type": "other", "api_key": "snap-stale", "email": "snap@x", "disabled": false, "headers": map[string]any{"X": "1"}}},
	} {
		path := filepath.Join(dir, tc.Name+".json")
		_ = os.WriteFile(path, []byte(tc.Disk), 0o600)
		raw, _ := json.Marshal(tc.Storage)
		var storage metaauth.MetaTokenStorage
		_ = json.Unmarshal(raw, &storage)
		if tc.Snapshot != nil {
			storage.SetMetadata(tc.Snapshot)
		}
		if err := storage.SaveTokenToFile(path); err != nil {
			t.Fatal(err)
		}
		written, _ := os.ReadFile(path)
		tc.Out = string(written)
		cases = append(cases, tc)
	}
	var names []map[string]string
	for _, in := range [][2]string{{"Some.User+tag@Example.com", ""}, {"", "dca:sub"}, {"", ""}, {"ü@x", ""}} {
		names = append(names, map[string]string{"email": in[0], "sub": in[1], "out": metaauth.CredentialFileName(in[0], in[1])})
	}
	rsfixWrite(t, "meta", rsfixFixture{Name: "writer", Credential: map[string]any{}, Request: map[string]any{}, Extra: map[string]any{"cases": cases, "file_names": names}})
}

// TestRSFixMetaDecode records Go's json.Unmarshal results for the Meta auth wire records:
// field folding, duplicates, nulls, type errors (first error wins, decoding continues) and
// syntax errors.
func TestRSFixMetaDecode(t *testing.T) {
	rsfixOut(t)
	inputs := []string{
		`{"DEVICE_CODE":"d","User_Code":"u","interval":2}`,
		`{"device_code":"a","device_code":"b","user_code":"u"}`,
		`{"device_code":"a","Device_Code":"b","DEVICE_CODE":null,"user_code":"u"}`,
		`{"device_code":null,"user_code":"u","expires_in":null}`,
		`{"interval":1.5}`, `{"interval":"5"}`, `{"interval":1e3}`, `{"interval":-0}`,
		`{"interval":99999999999999999999}`, `{"interval":true}`, `{"interval":{}}`, `{"interval":[]}`,
		`{"device_code":5}`, `{"device_code":true}`, `{"device_code":{"a":1},"user_code":[1],"interval":7}`,
		`{"TokenEndpoint":"x","-":"y","device_code":"\u00e9\ud83d\ude00"}`,
		`[1]`, `null`, `"x"`, `5`, `true`, ``, ` `, `{`, `{"a":tru}`, `{"a" 1}`, `nul`, `{"a":1}x`, "{\"a\":\"\x01\"}",
		`{"a":01}`, `{"a":-}`, `{"a":1.}`, `{"a":1e}`, `{'a':1}`, `{"a":"\q"}`, `{"a":"\u12g4"}`, `{"a":1,}`, `[1,]`,
		`{"a":[1 2]}`, `{"a":fals}`, `{"a":nulx}`, "{\"a\":\xc3\xa9}", "{\"a\":\x7f}", "{\"a\":\xc2\xa0}", `{"a":1}}`,
		`{"access_token":"t","ACCESS_TOKEN":"T","Expires_In":5,"expires_at":"x","Error":"slow_down"}`,
		`{"error":5,"ERROR":"access_denied","error_description":null}`,
		`{"api_key":"first","api_key":"second"}`, `{"API_KEY":"k","Is_Subs_Active":true,"has_payment_method":null}`,
		`{"api_key":"k","is_subs_active":"yes"}`, `{"api_key":"k","can_subscribe":1,"require_payment":false}`,
		`{"ſtatus":1,"api_kKey":"x"}`, "{\"api_\u212aey\":\"kelvin\",\"\u017fubs_tier_name\":\"longs\"}",
	}
	type result struct {
		Value any    `json:"value"`
		Err   string `json:"err"`
	}
	var out []map[string]any
	for _, in := range inputs {
		var d metaauth.DeviceCodeResponse
		errD := json.Unmarshal([]byte(in), &d)
		var tk metaauth.TokenData
		errT := json.Unmarshal([]byte(in), &tk)
		var m metaauth.MintedKeyResponse
		errM := json.Unmarshal([]byte(in), &m)
		msg := func(err error) string {
			if err == nil {
				return ""
			}
			return err.Error()
		}
		out = append(out, map[string]any{
			"input":  in,
			"device": result{d, msg(errD)},
			"token":  result{tk, msg(errT)},
			"mint":   result{m, msg(errM)},
		})
	}
	// quoteChar for every byte: a lone byte where a value must begin.
	var bytesOut []map[string]any
	for b := 0; b < 256; b++ {
		var v map[string]any
		err := json.Unmarshal([]byte{byte(b)}, &v)
		msg := ""
		if err != nil {
			msg = err.Error()
		}
		bytesOut = append(bytesOut, map[string]any{"byte": b, "err": msg})
	}
	rsfixWrite(t, "meta", rsfixFixture{Name: "decode", Credential: map[string]any{}, Request: map[string]any{}, Extra: map[string]any{"cases": out, "bytes": bytesOut}})
}
