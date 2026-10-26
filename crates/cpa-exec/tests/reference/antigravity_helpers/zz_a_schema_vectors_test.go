package executor

import (
	"strings"
	"testing"
)

// TestAntigravityZZSchemaVectors records sanitizeAntigravityRequestSchemasForModel on
// shapes Go's own tests leave out (b8d4e14a's per-declaration splicing): repeated
// declaration-list keys, both spellings, elements that are not objects, odd whitespace
// and repeated keys inside one declaration. Its records are unnamed in the fixture.
func TestAntigravityZZSchemaVectors(t *testing.T) {
	dirty := `{"name":"t","parametersJsonSchema":{"type":"object","$id":"x","properties":{"q":{"type":["string","null"]}}}}`
	other := `{"name":"u","parameters":{"type":"object","additionalProperties":false,"properties":{"r":{"type":"integer"}}}}`
	clean := `{"name":"c"}`
	wrap := func(tools string) string { return `{"request":{"tools":[` + tools + `]}}` }
	cases := []string{
		// A tool repeating one declarations key, in every arrangement.
		wrap(`{"functionDeclarations":[` + dirty + `],"functionDeclarations":[` + other + `]}`),
		wrap(`{"functionDeclarations":[` + clean + `],"functionDeclarations":[` + dirty + `]}`),
		wrap(`{"function_declarations":[` + dirty + `],"function_declarations":[` + dirty + `]}`),
		wrap(`{"functionDeclarations":[` + dirty + `],"function_declarations":[` + other + `],"functionDeclarations":[` + dirty + `]}`),
		wrap(`{"function_declarations":[` + other + `],"functionDeclarations":[` + dirty + `],"function_declarations":[` + dirty + `]}`),
		wrap(`{"functionDeclarations":{"x":1},"functionDeclarations":[` + dirty + `]}`),
		// Both spellings in either order, beside tools without declarations.
		wrap(`{"googleSearch":{}},{"function_declarations":[` + dirty + `,` + other + `],"functionDeclarations":[` + other + `,` + dirty + `]}`),
		// Elements that are not objects, in both arrays.
		wrap(`1,"s",null,{"functionDeclarations":[1,` + dirty + `,"x",null,[` + dirty + `],` + other + `]},[]`),
		// Whitespace around elements and inside declarations.
		"{\"request\":{\"tools\":[ \n\t{ \"functionDeclarations\" : [ " + dirty + " ,\n " + clean + " , " + other + " ] } , {\"function_declarations\":[\r\n" + dirty + "\r\n]}\n ]}}",
		// Repeated keys inside one declaration.
		wrap(`{"functionDeclarations":[{"name":"d","parametersJsonSchema":{"type":"object","$id":"a"},"parametersJsonSchema":{"type":"object","$id":"b"}}]}`),
		wrap(`{"functionDeclarations":[{"name":"d","parameters":{"type":"object","$id":"a"},"parametersJsonSchema":{"type":"object","$id":"b"},"parameters":{"type":"object"}}]}`),
		wrap(`{"functionDeclarations":[{"name":"d","response":{"type":"object","$id":"a"},"response":{"type":"object","$id":"b"},"parameters":{"type":"object"}}]}`),
		// An escaped key spelling.
		wrap(`{"function\u0044eclarations":[` + dirty + `]}`),
		// Many declarations with one dirty in the middle.
		wrap(`{"functionDeclarations":[` + strings.Repeat(clean+",", 20) + dirty + strings.Repeat(","+clean, 20) + `]}`),
	}
	for _, payload := range cases {
		for _, flags := range [][2]bool{{true, false}, {false, false}, {true, true}} {
			sanitizeAntigravityRequestSchemasForModel(payload, flags[0], flags[1])
		}
	}
}
