// Records what Go's mime/multipart Reader.ReadForm returns for header, line and
// boundary edge cases, as the images paths call it (multipart.NewReader(bytes.NewReader(
// body), boundary).ReadForm(32 << 20); the boundary is "b" unless a case names one). Standard library only; it opens no files or sockets.
package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"mime/multipart"
	"os"
	"runtime"
	"strings"
)

type file struct {
	Filename string              `json:"filename"`
	Header   map[string][][]byte `json:"header"`
	Body     []byte              `json:"body"`
}

type result struct {
	Name     string              `json:"name"`
	Boundary string              `json:"boundary,omitempty"`
	Body     []byte              `json:"body"`
	Error    *string             `json:"error,omitempty"`
	Values   map[string][][]byte `json:"values,omitempty"`
	Files    map[string][]file   `json:"files,omitempty"`
}

const cd = "Content-Disposition: form-data; name=\"p\"\r\n"
const fileCD = "Content-Disposition: form-data; name=\"f\"; filename=\"a.png\"\r\n"

var cases = []struct{ name, body string }{
	// Continuation lines are joined before the key and value bytes are checked.
	{"continuation_joined_before_key_check", "--b\r\nBad(Key): x\r\n\tmore\r\n\r\nv\r\n--b--\r\n"},
	{"continuation_value_bytes_checked", "--b\r\n" + cd + "X-A: ok\r\n  bad\x01\r\n\r\nv\r\n--b--\r\n"},
	{"continuation_joined_with_one_space", "--b\r\n" + fileCD + "Content-Type: image/\r\n \t png  \r\n\r\nv\r\n--b--\r\n"},
	{"whitespace_only_line_continues", "--b\r\n" + cd + "  \r\nX-B: z\r\n\r\nv\r\n--b--\r\n"},
	{"lf_mode_continuation", "--b\nContent-Disposition: form-data;\n\tname=\"p\"\n\nv\n--b--\n"},
	// textproto trims SP and HTAB only.
	{"form_feed_is_not_trimmed", "--b\r\n" + cd + "X-A: a\x0c\r\n\r\nv\r\n--b--\r\n"},
	{"tabs_and_spaces_trimmed", "--b\r\n" + fileCD + "X-A: \t a \t\r\n\r\nv\r\n--b--\r\n"},
	{"missing_colon_quotes_the_untrimmed_line", "--b\r\n" + cd + "NoColon \t\r\n\r\nv\r\n--b--\r\n"},
	{"key_with_trailing_space_kept", "--b\r\n" + fileCD + "x-a : v\r\n\r\nv\r\n--b--\r\n"},
	{"carriage_return_inside_value", "--b\r\n" + cd + "X-A: a\rb\r\n\r\nv\r\n--b--\r\n"},
	// Header values are bytes, obs-text included.
	{"obs_text_value_kept", "--b\r\n" + fileCD + "X-Note: caf\xe9 \xff\r\n\r\nv\r\n--b--\r\n"},
	// ReadLine keeps a bare CR at EOF; EOF inside the headers ends the form.
	{"bare_cr_at_eof", "--b\r\n" + cd + "X-A: v\r"},
	{"header_at_eof_without_newline", "--b\r\n" + "Content-Disposition: form-data; name=\"p\""},
	{"header_then_eof", "--b\r\n" + cd},
	{"eof_after_first_part", "--b\r\n" + cd + "\r\nv\r\n--b\r\n" + "Content-Disposition: form-data; name=\"q\"\r\n"},
	{"initial_line_bare_cr_at_eof", "--b\r\n X\r"},
	// The initial-line diagnostic reads at most 80 bytes.
	{"initial_line_80_bytes", "--b\r\n " + strings.Repeat("x", 79) + "\r\n\r\nv\r\n--b--\r\n"},
	{"initial_line_81_bytes", "--b\r\n " + strings.Repeat("x", 80) + "\r\n\r\nv\r\n--b--\r\n"},
	{"initial_line_81_bytes_with_cr", "--b\r\n " + strings.Repeat("x", 79) + "\r"},
	// Lines outside parts are read with a 4096-byte bufio buffer.
	{"preamble_line_4096_bytes", strings.Repeat("x", 4094) + "\r\n--b\r\n" + cd + "\r\nv\r\n--b--\r\n"},
	{"preamble_line_4097_bytes", strings.Repeat("x", 4095) + "\r\n--b\r\n" + cd + "\r\nv\r\n--b--\r\n"},
	{"final_boundary_4096_bytes_at_eof", "--b\r\n" + cd + "\r\nv\r\n--b--" + strings.Repeat(" ", 4091)},
	{"final_boundary_4095_bytes_at_eof", "--b\r\n" + cd + "\r\nv\r\n--b--" + strings.Repeat(" ", 4090)},
	{"long_header_line", "--b\r\n" + fileCD + "X-Long: " + strings.Repeat("y", 5000) + "\r\n\r\nv\r\n--b--\r\n"},
	// Next() diagnostics quote the whole line, line ending included.
	{"expecting_a_new_part", "--b\r\n" + cd + "\r\nv\r\n--b x\r\n"},
	{"unexpected_line_in_next", "--b\r\n" + cd + "\r\n--b x\r\n"},
	// A nameless part's body is drained; a named part's is read.
	{"nameless_part_without_boundary", "--b\r\nX-A: y\r\n\r\nabc\r\n--"},
	{"named_part_without_boundary", "--b\r\n" + cd + "\r\nabc\r\n--"},
	// textproto drops a header line without a newline whose last ReadLine fragment
	// (4096 bytes, or 4095 when it would end in CR) ends the body: io.EOF ends the form.
	{"header_fragment_4095_bytes_at_eof", "--b\r\n" + strings.Repeat("x", 4095)},
	{"header_fragment_4096_bytes_at_eof", "--b\r\n" + strings.Repeat("x", 4096)},
	{"header_fragment_8192_bytes_at_eof", "--b\r\n" + strings.Repeat("x", 8192)},
	{"header_fragment_cr_rewind_kept", "--b\r\n" + strings.Repeat("x", 4095) + "\r"},
	{"header_fragment_cr_rewind_dropped", "--b\r\n" + strings.Repeat("x", 4095) + "\r" + strings.Repeat("x", 4095)},
	{"continuation_fragment_4096_bytes_at_eof", "--b\r\n" + cd + "\r\nv\r\n--b\r\nX-A: v\r\n " + strings.Repeat("y", 4096)},
	{"named_part_then_header_fragment_at_eof", "--b\r\n" + cd + "\r\nv\r\n--b\r\n" + strings.Repeat("x", 4096)},
	// The invalid-value diagnostic quotes the line with its key canonicalized.
	{"lowercase_key_continuation_value_checked", "--b\r\nx-a: ok\r\n \x01\r\n\r\nv\r\n--b--\r\n"},
	{"spaced_key_continuation_value_checked", "--b\r\nx a: ok\r\n \x01\r\n\r\nv\r\n--b--\r\n"},
}

// Long boundaries: partReader decides a delimiter from at most the 4096 bytes its
// bufio buffer holds from the delimiter on.
var longBoundaries = []struct{ name, boundary, body string }{}

func init() {
	for _, n := range []int{4089, 4090, 4091, 4092} {
		b := strings.Repeat("a", n)
		named := "Content-Disposition: form-data; name=\"p\"\r\n\r\nx\r\n--" + b + "--"
		longBoundaries = append(longBoundaries,
			struct{ name, boundary, body string }{fmt.Sprintf("boundary_%d_final_at_eof", n), b, "--" + b + "\r\n" + named},
			struct{ name, boundary, body string }{fmt.Sprintf("boundary_%d_final_crlf", n), b, "--" + b + "\r\n" + named + "\r\n"},
			struct{ name, boundary, body string }{fmt.Sprintf("boundary_%d_file_then_part", n), b, "--" + b + "\r\nContent-Disposition: form-data; name=\"f\"; filename=\"a\"\r\n\r\nx\r\n--" + b + "\r\n" + named + "\r\n"},
			struct{ name, boundary, body string }{fmt.Sprintf("boundary_%d_nameless_then_part", n), b, "--" + b + "\r\nX-A: y\r\n\r\nx\r\n--" + b + "\r\n" + named + "\r\n"},
			struct{ name, boundary, body string }{fmt.Sprintf("boundary_%d_nameless_closed_by_final", n), b, "--" + b + "\r\nX-A: y\r\n\r\nx\r\n--" + b + "--\r\n"},
			struct{ name, boundary, body string }{fmt.Sprintf("boundary_%d_nameless_closed_by_final_at_eof", n), b, "--" + b + "\r\nX-A: y\r\n\r\nx\r\n--" + b + "--"},
			struct{ name, boundary, body string }{fmt.Sprintf("boundary_%d_lf", n), b, "--" + b + "\nContent-Disposition: form-data; name=\"p\"\n\nx\n--" + b + "--\n"},
		)
	}
}

func main() {
	var out []result
	all := longBoundaries[:0:0]
	for _, c := range cases {
		all = append(all, struct{ name, boundary, body string }{c.name, "b", c.body})
	}
	all = append(all, longBoundaries...)
	for _, c := range all {
		r := result{Name: c.name, Body: []byte(c.body)}
		if c.boundary != "b" {
			r.Boundary = c.boundary
		}
		form, err := multipart.NewReader(bytes.NewReader([]byte(c.body)), c.boundary).ReadForm(32 << 20)
		if err != nil {
			msg := err.Error()
			r.Error = &msg
		} else {
			r.Values = map[string][][]byte{}
			for k, vs := range form.Value {
				for _, v := range vs {
					r.Values[k] = append(r.Values[k], []byte(v))
				}
			}
			r.Files = map[string][]file{}
			for k, fhs := range form.File {
				for _, fh := range fhs {
					f, _ := fh.Open()
					body, _ := io.ReadAll(f)
					f.Close()
					h := map[string][][]byte{}
					for hk, hv := range fh.Header {
						for _, v := range hv {
							h[hk] = append(h[hk], []byte(v))
						}
					}
					r.Files[k] = append(r.Files[k], file{fh.Filename, h, body})
				}
			}
			form.RemoveAll()
		}
		out = append(out, r)
	}
	doc := map[string]any{"go": runtime.Version(), "cases": out}
	b, _ := json.MarshalIndent(doc, "", "  ")
	if err := os.WriteFile(os.Args[1], append(b, '\n'), 0o644); err != nil {
		panic(err)
	}
}
