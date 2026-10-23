package helps

import (
	"encoding/base64"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// TestRSFixDevinView writes v8.0.35's FinalizeDevinPayload view (no rules) of each
// fixture's last recorded GetChatMessage frame.
func TestRSFixDevinView(t *testing.T) {
	files, _ := filepath.Glob(filepath.Join(os.Getenv("RSFIX_IN"), "*.json"))
	out := map[string]string{}
	for _, f := range files {
		raw, _ := os.ReadFile(f)
		var fx struct {
			Upstream []struct {
				BodyB64 string `json:"body_b64"`
			} `json:"upstream"`
		}
		if json.Unmarshal(raw, &fx) != nil || len(fx.Upstream) == 0 || fx.Upstream[len(fx.Upstream)-1].BodyB64 == "" {
			continue
		}
		frame, _ := base64.StdEncoding.DecodeString(fx.Upstream[len(fx.Upstream)-1].BodyB64)
		if len(frame) < 5 || frame[0] != 0 {
			continue // not a GetChatMessage frame
		}
		_, view, err := FinalizeDevinPayload(frame[5:], func(b []byte) []byte { return b })
		if err != nil {
			t.Fatal(f, err)
		}
		out[strings.TrimSuffix(filepath.Base(f), ".json")] = string(view)
	}
	data, _ := json.MarshalIndent(out, "", "  ")
	_ = os.WriteFile(os.Getenv("RSFIX_OUT"), data, 0o644)
}
