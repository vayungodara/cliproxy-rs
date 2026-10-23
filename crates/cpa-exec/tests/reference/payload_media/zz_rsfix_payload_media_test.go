package helps

import (
	"bytes"
	"encoding/base64"
	"encoding/json"
	"io"
	"mime"
	"mime/multipart"
	"net/http"
	"os"
	"sort"
	"testing"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	cliproxyexecutor "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/executor"
)

// TestRSFixPayloadMedia records ApplyMediaPayloadConfig (868e137) for cliproxy-rs
// (crates/cpa-exec/tests/reference/payload_media). Go writes multipart parts in random
// order under a random boundary, so a rebuilt form is recorded as its parts.
func TestRSFixPayloadMedia(t *testing.T) {
	const b = "b0undary"
	ct := "multipart/form-data; boundary=" + b
	form := func(parts ...string) string {
		var out string
		for _, p := range parts {
			out += "--" + b + "\r\n" + p + "\r\n"
		}
		return out + "--" + b + "--\r\n"
	}
	field := func(name, value string) string {
		return "Content-Disposition: form-data; name=\"" + name + "\"\r\n\r\n" + value
	}
	file := func(name, filename, contentType, data string) string {
		h := "Content-Disposition: form-data; name=\"" + name + "\"; filename=\"" + filename + "\"\r\n"
		if contentType != "" {
			h += "Content-Type: " + contentType + "\r\n"
		}
		return h + "\r\n" + data
	}
	rule := func(kind, params string) string {
		return "payload:\n  " + kind + ":\n    - models: [{name: gpt-image-2, protocol: openai}]\n      params: " + params + "\n"
	}
	png := "\x00\xff\r\n\x89PNG"
	basic := form(field("model", "gpt-image-2"), field("prompt", "a <cat> & \xff"), file("image", "input.png", "image/png", png))
	cases := []struct {
		name, cfg, body, contentType, original string
	}{
		{"json_override", rule("override", `{"size": "1024x1024"}`), `{"model":"gpt-image-2","prompt":"x"}`, "application/json", ""},
		{"json_default_kept", rule("default", `{"prompt": "default"}`), `{"model":"gpt-image-2","prompt":"x"}`, "application/json", ""},
		{"form_unmatched", "payload:\n  override:\n    - models: [{name: other, protocol: openai}]\n      params: {\"size\": \"1\"}\n", basic, ct, ""},
		{"form_no_change", rule("default", `{"prompt": "default"}`), basic, ct, ""},
		{"form_override", rule("override", `{"model": "configured", "size": "auto"}`), basic, ct, ""},
		{"form_filter", "payload:\n  filter:\n    - models: [{name: gpt-image-2, protocol: openai}]\n      params: [\"prompt\"]\n", basic, ct, ""},
		{"form_repeated", rule("override", `{"n": 2}`),
			form(field("model", "gpt-image-2"), file("image[]", "a.png", "image/png", "A"), file("image[]", "b.jpg", "", "B"), field("tag", "1"), field("tag", "2")), ct, ""},
		{"form_names", rule("override", `{"quality": "high"}`),
			form(field("model", "gpt-image-2"), file("my image", "caf\xc3\xa9 \\\"1\\\".png", "image/png", "C"), file("plain", "x.png", "image/png", "D"), field("", "unnamed")), ct, ""},
		// Unescaped quotes break the disposition: the part has no name or filename.
		{"form_broken_disposition", rule("override", `{"quality": "high"}`),
			form(field("model", "gpt-image-2"), file("my image", "caf\xc3\xa9 \"1\".png", "image/png", "C")), ct, ""},
		{"form_default_from_original", rule("default", `{"size": "256x256", "quality": "low"}`),
			form(field("model", "gpt-image-2"), field("prompt", "p")), ct,
			form(field("model", "gpt-image-2"), field("size", "512x512"))},
		{"form_bad_base64", rule("override", `{"image.data": "%%"}`), basic, ct, ""},
		{"form_file_to_field", rule("override", `{"image": "replaced"}`), basic, ct, ""},
		{"form_raw_number", rule("default-raw", `{"n": "3"}`), basic, ct, ""},
		{"form_truncated", rule("override", `{"n": 2}`), "--" + b + "\r\n" + field("model", "gpt-image-2"), ct, ""},
		{"form_truncated_unnamed", rule("override", `{"n": 2}`), "--" + b + "\r\n" + field("", "gpt-image-2"), ct, ""},
		{"form_empty_boundary", rule("override", `{"n": 2}`), basic, "multipart/form-data", ""},
		{"form_empty_boundary_param", rule("override", `{"n": 2}`), basic, "multipart/form-data; boundary=\"\"", ""},
		// Go 1.26's quoteEscaper writes CR and LF as %0D and %0A, in names a client sent
		// (RFC 2231) and names a rule added.
		{"form_crlf_names", rule("override", `{"c\r\nd": "rule"}`),
			form(field("model", "gpt-image-2"),
				"Content-Disposition: form-data; name*=UTF-8''a%0D%0Ab\r\n\r\nclient",
				"Content-Disposition: form-data; name*=UTF-8''f%0D%0Ag; filename*=UTF-8''x%0Ay%0D.png\r\nContent-Type: image/png\r\n\r\nE"), ct, ""},
	}
	var out []map[string]any
	for _, tc := range cases {
		cfg, err := config.ParseConfigBytes([]byte(tc.cfg))
		if err != nil {
			t.Fatal(tc.name, err)
		}
		original := tc.original
		if original == "" {
			original = tc.body
		}
		req := cliproxyexecutor.Request{Model: "gpt-image-2", Payload: []byte(original)}
		opts := cliproxyexecutor.Options{Headers: http.Header{"Content-Type": {tc.contentType}}}
		rec := map[string]any{"name": tc.name, "config": tc.cfg, "content_type": tc.contentType,
			"body_b64": base64.StdEncoding.EncodeToString([]byte(tc.body)), "original_b64": base64.StdEncoding.EncodeToString([]byte(original))}
		view, _, errView := mediaPayloadJSON([]byte(tc.body), tc.contentType)
		if errView == nil {
			rec["view"] = string(view)
		}
		body, contentType, errApply := ApplyMediaPayloadConfig(cfg, "openai", "gpt-image-2", "openai", []byte(tc.body), tc.contentType, req, opts)
		switch {
		case errApply != nil:
			rec["error"] = errApply.Error()
		case bytes.Equal(body, []byte(tc.body)):
			rec["unchanged"] = true
			rec["out_content_type"] = contentType
		default:
			media, params, _ := mime.ParseMediaType(contentType)
			if media != "multipart/form-data" {
				rec["out_b64"] = base64.StdEncoding.EncodeToString(body)
				rec["out_content_type"] = contentType
				break
			}
			rec["out_media"] = media
			reader := multipart.NewReader(bytes.NewReader(body), params["boundary"])
			var parts []string
			// Each name's bodies in the order Go wrote them; the parts list is sorted.
			order := map[string][]string{}
			for {
				part, errNext := reader.NextRawPart()
				if errNext == io.EOF {
					break
				}
				if errNext != nil {
					t.Fatal(tc.name, errNext)
				}
				data, _ := io.ReadAll(part)
				var keys []string
				for k := range part.Header {
					keys = append(keys, k)
				}
				sort.Strings(keys)
				desc := ""
				for _, k := range keys {
					desc += k + ": " + part.Header.Get(k) + "\n"
				}
				parts = append(parts, desc+"\n"+base64.StdEncoding.EncodeToString(data))
				order[part.FormName()] = append(order[part.FormName()], base64.StdEncoding.EncodeToString(data))
			}
			sort.Strings(parts)
			rec["out_parts"] = parts
			for name, bodies := range order {
				if len(bodies) < 2 {
					delete(order, name)
				}
			}
			rec["out_order"] = order
		}
		out = append(out, rec)
	}
	data, _ := json.MarshalIndent(map[string]any{"cases": out}, "", "  ")
	if err := os.WriteFile(os.Getenv("RSFIX_OUT"), append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
