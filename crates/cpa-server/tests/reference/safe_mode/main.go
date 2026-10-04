// Run inside the pinned reference module; the generated HTML is the production
// template as well as a byte-for-byte Go golden, with no new visual design.
package main

import (
	"encoding/json"
	"fmt"
	"os"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/safemode"
)

func main() {
	const management = "/management.html?safe-mode=configure"
	if len(os.Args) > 1 && os.Args[1] == "html" {
		fmt.Print(safemode.ExampleAPIKeyWarningPageHTML(nil, management))
		return
	}
	cases := []map[string]any{}
	for _, keys := range [][]string{
		{" real-key ", " your-api-key-1 ", "your-api-key", "change-me", "your-api-key-2", "your-api-key-2", "your-api-key-3"},
		{"your-api-key", "change-me", "changeme", "your-api-key-4", "my-your-api-key-1"},
		{"\u2003your-api-key-3\u2003", "YOUR-API-KEY-1", "your-api-key-2"},
	} {
		matches := safemode.ExampleAPIKeys(keys)
		cases = append(cases, map[string]any{"keys": keys, "matches": matches, "html": safemode.ExampleAPIKeyWarningPageHTML(matches, management)})
	}
	if err := json.NewEncoder(os.Stdout).Encode(cases); err != nil {
		panic(err)
	}
}
