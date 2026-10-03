package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	applypatch "github.com/router-for-me/CLIProxyAPI/v8/internal/client/codex/apply-patch"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/runtime/executor/helps"
	codexresponses "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/codex/openai/responses"
	translatorcommon "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/common"
	sdk "github.com/router-for-me/CLIProxyAPI/v8/sdk/translator"
)

// apply_patch Responses bridge vectors (tests/fixtures/apply_patch_responses.json, replayed
// by tests/apply_patch_responses.rs): translator/common's NormalizeApplyPatchResponsesRequest
// and ApplyPatchResponsesBridge, helps' NormalizeApplyPatchResponsesRequest and
// ApplyPatchResponsesState, and the openai-response:codex pair with an executor-owned
// bridge. Scenarios are Go's own test cases (apply_patch_responses_test.go,
// apply_patch_identity_test.go, helps/apply_patch_responses_test.go,
// codex_openai-responses_response_test.go) plus edge cases; every op's output and error
// text is recorded. All strings are stored one byte per rune.

type apOp struct {
	Op        string `json:"op"`
	In        string `json:"in,omitempty"`
	Name      string `json:"name,omitempty"`
	Namespace string `json:"namespace,omitempty"`
}

type apResult struct {
	Out []string `json:"out"`
	Err *string  `json:"err"`
}

type apScenario struct {
	Name         string     `json:"name"`
	Kind         string     `json:"kind"`
	Request      string     `json:"request,omitempty"`
	Source       string     `json:"source,omitempty"`
	Original     string     `json:"original,omitempty"`
	Declarations string     `json:"declarations,omitempty"`
	Model        string     `json:"model,omitempty"`
	Bridged      bool       `json:"bridged,omitempty"`
	Ops          []apOp     `json:"ops"`
	Results      []apResult `json:"results"`
	ToolError    *string    `json:"tool_error"`
}

func apJSON(value any) string { raw, _ := json.Marshal(value); return string(raw) }
func apItem(kind, id, call, name, args string) string {
	return fmt.Sprintf(`{"type":%s,"id":%s,"call_id":%s,"name":%s,"arguments":%s}`, apJSON(kind), apJSON(id), apJSON(call), apJSON(name), apJSON(args))
}
func apEvent(kind string, index int, item string) string {
	return fmt.Sprintf(`{"type":%s,"output_index":%d,"item":%s}`, apJSON(kind), index, item)
}
func apDelta(index int, delta string) string {
	return fmt.Sprintf(`{"type":"response.function_call_arguments.delta","output_index":%d,"delta":%s}`, index, apJSON(delta))
}
func apErr(err error) *string {
	if err == nil {
		return nil
	}
	s := latin1(err.Error())
	return &s
}
func apOut(items [][]byte) []string {
	out := []string{}
	for _, item := range items {
		out = append(out, latin1(string(item)))
	}
	return out
}
func tx(events ...string) []apOp {
	var ops []apOp
	for _, e := range events {
		ops = append(ops, apOp{Op: "transform", In: e})
	}
	return ops
}

const apReq = `{"tools":[{"type":"custom","name":"apply_patch"}]}`
const apNSReq = `{"tools":[{"type":"namespace","name":"n","tools":[{"type":"custom","name":"apply_patch"},{"type":"function","name":"lookup"}]}]}`
const apNSOnly = `{"tools":[{"type":"namespace","name":"n","tools":[{"type":"custom","name":"apply_patch"}]}]}`
const apCompleted = `{"type":"response.completed","response":{"output":[]}}`

func runAP(s *apScenario) {
	ctx := context.Background()
	record := func(out [][]byte, err error) {
		s.Results = append(s.Results, apResult{Out: apOut(out), Err: apErr(err)})
	}
	switch s.Kind {
	case "bridge":
		b := translatorcommon.NewApplyPatchResponsesBridge([]byte(s.Request))
		for _, op := range s.Ops {
			switch op.Op {
			case "transform":
				record(b.Transform([]byte(op.In)))
			case "non_stream":
				out, err := b.TransformNonStream([]byte(op.In))
				if out == nil {
					record(nil, err)
				} else {
					record([][]byte{out}, err)
				}
			case "finish":
				record(nil, b.Finish())
			case "check_identity":
				record(nil, b.CheckIdentity([]byte(op.In)))
			case "fail":
				record(b.Fail(errors.New(op.In)))
			default:
				panic(op.Op)
			}
		}
		s.ToolError = apErr(b.ToolInputError())
	case "state":
		st := helps.NewApplyPatchResponsesState(sdk.FromString(s.Source), []byte(s.Original), []byte(s.Declarations))
		for _, op := range s.Ops {
			switch op.Op {
			case "add_dispatcher":
				st.AddDispatcher(op.Name, op.Namespace)
				record(nil, nil)
			case "transform":
				record(st.Transform([]byte(op.In)))
			case "stream":
				record(st.Stream([]byte(op.In)))
			case "finish":
				record(nil, st.Finish())
			case "bridge_finish":
				record(nil, st.Bridge.Finish())
			case "finish_stream":
				record(st.FinishStream())
			case "remember_event":
				st.RememberDispatcherEvent([]byte(op.In))
				record(nil, nil)
			case "remember_args":
				st.RememberDispatcherArguments([]byte(op.In))
				record(nil, nil)
			case "active":
				record([][]byte{[]byte(fmt.Sprint(st.Active()))}, nil)
			default:
				panic(op.Op)
			}
		}
		s.ToolError = apErr(st.Bridge.ToolInputError())
	case "normalize", "normalize_executor":
		for _, op := range s.Ops {
			var out []byte
			var err error
			if s.Kind == "normalize" {
				out, err = translatorcommon.NormalizeApplyPatchResponsesRequest([]byte(op.In))
			} else if s.Original != "" {
				out, err = helps.NormalizeApplyPatchResponsesRequest([]byte(op.In), []byte(s.Original))
			} else {
				out, err = helps.NormalizeApplyPatchResponsesRequest([]byte(op.In))
			}
			if err != nil {
				out = nil
			}
			if out == nil {
				record(nil, err)
			} else {
				record([][]byte{out}, err)
			}
		}
	case "codex_stream", "codex_non_stream":
		var param any
		if s.Bridged {
			param = translatorcommon.NewApplyPatchResponsesBridge([]byte(s.Request))
		}
		for _, op := range s.Ops {
			if s.Kind == "codex_stream" {
				record(codexresponses.ConvertCodexResponseToOpenAIResponses(ctx, s.Model, []byte(s.Original), []byte(s.Declarations), []byte(op.In), &param), nil)
			} else {
				out := codexresponses.ConvertCodexResponseToOpenAIResponsesNonStream(ctx, s.Model, []byte(s.Original), []byte(s.Declarations), []byte(op.In), &param)
				if out == nil {
					record(nil, nil)
				} else {
					record([][]byte{out}, nil)
				}
			}
		}
		if state, ok := param.(interface{ ToolInputError() error }); ok {
			s.ToolError = apErr(state.ToolInputError())
		}
	default:
		panic(s.Kind)
	}
	s.Request, s.Original, s.Declarations = latin1(s.Request), latin1(s.Original), latin1(s.Declarations)
	for i := range s.Ops {
		s.Ops[i].In = latin1(s.Ops[i].In)
	}
}

func bridgeScenarios() []apScenario {
	var out []apScenario
	add := func(name, req string, ops ...apOp) {
		out = append(out, apScenario{Name: name, Kind: "bridge", Request: req, Ops: ops})
	}
	// TestApplyPatchResponsesBridgeDeltaAndFourCompletions.
	args := `{"input":"*** Begin Patch\n+中文\uD83D\uDE00 \"\\\n*** End Patch\n"}`
	for _, late := range []bool{false, true} {
		name := "apply_patch"
		if late {
			name = ""
		}
		events := []string{apEvent("response.output_item.added", 0, apItem("function_call", "", "", name, ""))}
		for _, part := range []string{args[:18], args[18:35], args[35:41], args[41:]} {
			events = append(events, apDelta(0, part))
		}
		item := apItem("function_call", "fc1", "c1", "apply_patch", args)
		events = append(events, apEvent("response.output_item.done", 0, item), `{"type":"response.completed","response":{"id":"r1","output":[`+item+`]}}`)
		ops := append(tx(events...), apOp{Op: "finish"}, apOp{Op: "transform", In: apCompleted})
		add(fmt.Sprintf("DeltaAndFourCompletions/late=%v", late), apReq, ops...)
	}
	// TestApplyPatchResponsesBridgePassthrough.
	for i, req := range []string{apReq, `{"tools":[{"type":"function","name":"apply_patch"}]}`, `{}`} {
		add(fmt.Sprint("Passthrough/", i), req, tx(
			`{ "type":"response.output_item.added", "output_index":0,"item":{"type":"custom_tool_call","id":"native","name":"apply_patch","input":""}}`,
			`{ "type":"response.custom_tool_call_input.delta", "item_id":"native","delta":"raw patch"}`,
			apEvent("response.output_item.done", 1, apItem("function_call", "ordinary", "ordinary", "lookup", `{"x":1}`)),
		)...)
	}
	// TestApplyPatchResponsesBridgeIdentityAndSnapshotEvidence.
	for _, tc := range []struct {
		name   string
		before []string
		after  string
	}{
		{"index-item", []string{apEvent("response.output_item.added", 0, apItem("function_call", "a", "ca", "apply_patch", "")), apEvent("response.output_item.added", 1, apItem("function_call", "b", "cb", "lookup", ""))}, `{"type":"response.function_call_arguments.delta","output_index":1,"item_id":"a","delta":"{}"}`},
		{"call-item", []string{apEvent("response.output_item.added", 0, apItem("function_call", "a", "ca", "apply_patch", ""))}, `{"type":"response.function_call_arguments.done","output_index":0,"item_id":"a","call_id":"other","arguments":"{\"input\":\"p\"}"}`},
		{"type-before-name", []string{apEvent("response.output_item.added", 0, apItem("message", "a", "ca", "", ""))}, apEvent("response.output_item.done", 0, apItem("function_call", "a", "ca", "apply_patch", `{"input":"p"}`))},
		{"invalid-before-name", []string{apEvent("response.output_item.added", 0, apItem("function_call", "a", "ca", "", `{"input":"p","extra":1}`))}, apEvent("response.output_item.done", 0, apItem("function_call", "a", "ca", "apply_patch", `{"input":"p"}`))},
		{"partial-snapshot", nil, apEvent("response.output_item.added", 0, apItem("function_call", "a", "ca", "apply_patch", `{"input":"p`))},
		{"invalid-final-only", nil, `{"type":"response.completed","response":{"output":[{"type":"function_call","name":"apply_patch","arguments":"{}"}]}}`},
		{"old-patch-new-type", []string{apEvent("response.output_item.added", 0, apItem("function_call", "a", "ca", "apply_patch", ""))}, `{"type":"response.completed","response":{"output":[{"type":"message","id":"a","content":[]}]}}`},
		{"pending-id-evidence", []string{apEvent("response.output_item.added", 0, apItem("function_call", "a", "ca", "", ""))}, apEvent("response.output_item.done", 0, apItem("function_call", "a", "changed", "apply_patch", `{"input":"p"}`))},
	} {
		add("IdentityAndSnapshotEvidence/"+tc.name, apReq, tx(append(append(tc.before, tc.after), apCompleted)...)...)
	}
	// TestApplyPatchResponsesBridgeStagesAndFinish.
	add("StagesAndFinish/incomplete", apReq, append(tx(apEvent("response.output_item.added", 0, apItem("function_call", "a", "ca", "apply_patch", ""))), apOp{Op: "finish"})...)
	var stages []string
	for i := 0; i < 2; i++ {
		a := applypatch.WrapInput(fmt.Sprint("patch", i))
		stages = append(stages,
			apEvent("response.output_item.added", i, apItem("function_call", fmt.Sprint("a", i), fmt.Sprint("c", i), "apply_patch", "")),
			fmt.Sprintf(`{"type":"response.function_call_arguments.done","output_index":%d,"arguments":%s}`, i, apJSON(a)),
			apEvent("response.output_item.done", i, apItem("function_call", fmt.Sprint("a", i), fmt.Sprint("c", i), "apply_patch", a)),
			apEvent("response.output_item.done", i, apItem("function_call", fmt.Sprint("a", i), fmt.Sprint("c", i), "apply_patch", a)),
		)
	}
	add("StagesAndFinish/two", apReq, append(tx(stages...), apOp{Op: "finish"})...)
	// TestApplyPatchResponsesRequestHistoryAndWinners (bridge half).
	for i, req := range []string{
		`{"tools":[{"type":"function","name":"apply_patch","description":"ordinary"}],"input":[{"type":"additional_tools","tools":[{"type":"custom","name":"apply_patch"}]}]}`,
		`{"tools":[{"type":"namespace","name":"n","tools":[{"type":"custom","name":"apply_patch"}]},{"type":"function","name":"n__apply_patch","description":"ordinary"}]}`,
	} {
		item := apItem("function_call", "a", "c", "apply_patch", `{"x":1}`)
		if strings.Contains(req, `"namespace"`) {
			item = apItem("function_call", "a", "c", "n__apply_patch", `{"x":1}`)
		}
		add(fmt.Sprint("HistoryAndWinners/", i), req, tx(apEvent("response.output_item.done", 0, item))...)
	}
	// TestApplyPatchResponsesNamespaceMixedNonStream.
	add("NamespaceMixedNonStream", apNSReq, apOp{Op: "non_stream", In: `{"id":"r","output":[` + apItem("function_call", "a", "c", "n__apply_patch", applypatch.WrapInput("p")) + `,` + apItem("function_call", "b", "d", "n__lookup", `{"x":1}`) + `]}`})
	// TestApplyPatchResponsesContinuationMissingSnapshots.
	for _, source := range []string{"deltas", "arguments.done"} {
		kind, field := "response.function_call_arguments.done", "arguments"
		if source == "deltas" {
			kind, field = "response.function_call_arguments.delta", "delta"
		}
		add("MissingSnapshots/"+source, apReq, tx(
			apEvent("response.output_item.added", 0, apItem("function_call", "a", "c", "apply_patch", "")),
			fmt.Sprintf(`{"type":%s,"item_id":"a",%s:%s}`, apJSON(kind), apJSON(field), apJSON(applypatch.WrapInput("p"))),
			`{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"apply_patch"}}`,
			apCompleted,
		)...)
	}
	// TestApplyPatchResponsesContinuationNativeTerminalBytes.
	add("NativeTerminalBytes", apReq, tx(
		`{ "type":"response.output_item.done", "output_index":0, "item":{"type":"custom_tool_call","id":"n","name":"apply_patch","input":"raw"}}`,
		`{ "type":"response.completed", "sequence_number":71, "response": {"output":[{"type":"custom_tool_call","id":"n","name":"apply_patch","input":"raw"}]}}`,
	)...)
	// TestApplyPatchResponsesContinuationRootLateName.
	add("RootLateName", apReq, append(tx(
		`{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a"}}`,
		`{"type":"response.function_call_arguments.done","output_index":0,"item_id":"a","call_id":"c","name":"apply_patch","arguments":"{\"input\":\"p\"}"}`,
	), apOp{Op: "finish"})...)
	// TestApplyPatchResponsesContinuationNoInventedPreview.
	add("NoInventedPreview", apReq, tx(apEvent("response.output_item.done", 0, apItem("function_call", "a", "c", "apply_patch", applypatch.WrapInput("p"))))...)
	// TestApplyPatchResponsesContinuationAllMatchedProvenance.
	keys := []string{"output_index", "item_id", "call_id"}
	for _, patchKey := range keys {
		for _, firstKey := range keys {
			if firstKey == patchKey {
				continue
			}
			vals := map[string]string{"output_index": "0", "item_id": `"a"`, "call_id": `"ca"`}
			vals[patchKey] = map[string]string{"output_index": "1", "item_id": `"b"`, "call_id": `"cb"`}[patchKey]
			add("AllMatchedProvenance/"+patchKey+"-"+firstKey, apReq, tx(
				apEvent("response.output_item.added", 0, apItem("function_call", "a", "ca", "lookup", "")),
				apEvent("response.output_item.added", 1, apItem("function_call", "b", "cb", "apply_patch", "")),
				fmt.Sprintf(`{"type":"response.function_call_arguments.delta","output_index":%s,"item_id":%s,"call_id":%s,"delta":"{}"}`, vals["output_index"], vals["item_id"], vals["call_id"]),
			)...)
		}
	}
	for _, discover := range []int{0, 1, 2} {
		var events []string
		for i := 0; i < 3; i++ {
			events = append(events, apEvent("response.output_item.added", i, apItem("function_call", fmt.Sprint("i", i), fmt.Sprint("c", i), "", "")))
		}
		events = append(events, `{"type":"response.function_call_arguments.delta","output_index":0,"item_id":"i1","call_id":"c2","delta":""}`,
			apEvent("response.output_item.done", discover, apItem("function_call", fmt.Sprint("i", discover), fmt.Sprint("c", discover), "apply_patch", applypatch.WrapInput("p"))))
		add(fmt.Sprint("AllMatchedProvenance/pending-", discover), apReq, tx(events...)...)
	}
	// TestApplyPatchResponsesContinuationCompletedWindow.
	for _, terminal := range []bool{false, true} {
		item := apItem("function_call", "a", "c", "apply_patch", applypatch.WrapInput("p"))
		events := []string{apEvent("response.output_item.done", 0, item)}
		if terminal {
			events = append(events, `{"type":"response.completed","response":{"output":[`+item+`]}}`)
		}
		events = append(events, apEvent("response.output_item.done", 0, apItem("function_call", "a", "c", "apply_patch", applypatch.WrapInput("different"))))
		add(fmt.Sprint("CompletedWindow/", terminal), apReq, tx(events...)...)
	}
	// TestApplyPatchResponsesContinuationOmittedMixedCompletedItems.
	ordinary := `{"type":"message","id":"m","content":[{"type":"output_text","text":"ok"}]}`
	add("OmittedMixedCompleted/omitted", apReq, tx(
		apEvent("response.output_item.done", 0, apItem("function_call", "p", "cp", "apply_patch", applypatch.WrapInput("p"))),
		apEvent("response.output_item.done", 1, ordinary), apCompleted)...)
	add("OmittedMixedCompleted/collided", apReq, tx(
		apEvent("response.output_item.done", 0, apItem("function_call", "p", "cp", "apply_patch", applypatch.WrapInput("p"))),
		`{"type":"response.completed","response":{"output":[`+ordinary+`]}}`)...)
	// TestApplyPatchResponsesContinuationUnmatchedIdentityEvidence.
	for _, known := range []bool{false, true} {
		for _, key := range []string{`"item_id":"b"`, `"call_id":"cb"`, `"output_index":1`} {
			var events []string
			if known {
				events = append(events, apEvent("response.output_item.added", 0, apItem("function_call", "a", "ca", "lookup", "")))
			}
			events = append(events, `{"type":"response.output_item.added","output_index":1,"item_id":"a","call_id":"ca","item":{"type":"function_call","id":"b","call_id":"cb","name":""}}`,
				`{"type":"response.function_call_arguments.done",`+key+`,"name":"apply_patch","arguments":"{\"input\":\"p\"}"}`)
			add(fmt.Sprintf("UnmatchedIdentityEvidence/known=%v/%s", known, key), apReq, tx(events...)...)
		}
	}
	// TestApplyPatchResponsesContinuationMixedSequence.
	add("MixedSequence", apReq, tx(
		`{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"apply_patch","arguments":""}}`,
		`{"type":"response.output_item.added","sequence_number":2,"output_index":1,"item":{"type":"function_call","id":"b","name":"lookup","arguments":""}}`,
		`{"type":"response.function_call_arguments.done","sequence_number":3,"output_index":0,"item_id":"a","arguments":"{\"input\":\"p\"}"}`,
		`{"type":"response.output_item.done","sequence_number":4,"output_index":1,"item":{"type":"function_call","id":"b","name":"lookup","arguments":"{\"x\":1}"}}`,
		`{"type":"response.completed","sequence_number":5,"response":{"output":[]}}`,
	)...)
	// TestApplyPatchResponsesContinuationOrdinaryRootNamePassthrough.
	add("OrdinaryRootNamePassthrough", apNSReq, tx(`{ "type":"response.function_call_arguments.done", "output_index":0,"name":"lookup","namespace":"n","arguments":"{}"}`)...)
	// TestApplyPatchResponsesContinuationMixedNativeBytes.
	add("MixedNativeBytes", apReq, tx(
		`{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"function_call","id":"a","name":"apply_patch","arguments":""}}`,
		`  { "type":"response.output_item.added", "sequence_number":2, "output_index":1,"item":{"type":"custom_tool_call","id":"n","name":"apply_patch","input":""}}  `,
	)...)
	// apply_patch_identity_test.go TestApplyPatchResponsesNamedLateIdentity.
	for _, first := range []string{"item", "call", "neither", "neither-item-first", "neither-call-first"} {
		for _, boundary := range []string{"item", "terminal"} {
			id, call := "", ""
			if first == "item" {
				id = "a"
			}
			if first == "call" {
				call = "c"
			}
			events := []string{apEvent("response.output_item.added", 0, apItem("function_call", id, call, "apply_patch", ""))}
			for _, fragment := range []string{`{"input":"p`, `q"}`} {
				events = append(events, apDelta(0, fragment))
			}
			events = append(events, `{"type":"response.function_call_arguments.done","output_index":0,"arguments":"{\"input\":\"pq\"}"}`)
			if strings.HasPrefix(first, "neither-") {
				id, call = "a", ""
				if first == "neither-call-first" {
					id, call = "", "c"
				}
				events = append(events, apEvent("response.output_item.added", 0, apItem("function_call", id, call, "apply_patch", "")))
			}
			item := apItem("function_call", "a", "c", "apply_patch", `{"input":"pq"}`)
			if boundary == "item" {
				events = append(events, apEvent("response.output_item.done", 0, item))
			}
			events = append(events, `{"type":"response.completed","response":{"output":[`+item+`]}}`)
			add("NamedLateIdentity/"+first+"-"+boundary, apReq, tx(events...)...)
		}
	}
	// TestApplyPatchResponsesNamedLateIdentityEvidence.
	for _, tc := range []struct{ name, before, after string }{
		{"seen-item-change", apEvent("response.output_item.added", 0, apItem("function_call", "a", "", "apply_patch", "")), apEvent("response.output_item.done", 0, apItem("function_call", "changed", "c", "apply_patch", `{"input":"pq"}`))},
		{"seen-call-change", apEvent("response.output_item.added", 0, apItem("function_call", "", "c", "apply_patch", "")), apEvent("response.output_item.done", 0, apItem("function_call", "a", "changed", "apply_patch", `{"input":"pq"}`))},
		{"partial-snapshot", apEvent("response.output_item.added", 0, apItem("function_call", "", "", "apply_patch", `{"input":"p`)), apEvent("response.output_item.done", 0, apItem("function_call", "a", "c", "apply_patch", `{"input":"pq"}`))},
		{"invalid-snapshot", apEvent("response.output_item.added", 0, apItem("function_call", "a", "", "apply_patch", `{"input":"pq","extra":1}`)), apEvent("response.output_item.done", 0, apItem("function_call", "a", "c", "apply_patch", `{"input":"pq"}`))},
		{"unresolved-terminal", apEvent("response.output_item.done", 0, apItem("function_call", "a", "", "apply_patch", `{"input":"pq"}`)), apCompleted},
	} {
		add("NamedLateIdentityEvidence/"+tc.name, apReq, tx(tc.before, tc.after, tc.after)...)
	}
	// TestApplyPatchResponsesNamedLateIdentityInterleaved.
	var inter []string
	for i, item := range []string{apItem("function_call", "a0", "", "apply_patch", ""), apItem("function_call", "", "c1", "apply_patch", "")} {
		inter = append(inter, apEvent("response.output_item.added", i, item), apDelta(i, `{"input":"`+fmt.Sprint(i)+`"}`))
	}
	for _, i := range []int{1, 0} {
		inter = append(inter, apEvent("response.output_item.done", i, apItem("function_call", fmt.Sprint("a", i), fmt.Sprint("c", i), "apply_patch", `{"input":"`+fmt.Sprint(i)+`"}`)))
	}
	add("NamedLateIdentityInterleaved", apReq, tx(append(inter, apCompleted)...)...)

	// Edge cases beyond Go's tests.
	add("Edge/invalid-utf8-and-html", apReq, tx(
		apEvent("response.output_item.added", 0, apItem("function_call", "a", "c", "apply_patch", "")),
		apDelta(0, "{\"input\":\"<a&b>\xff\xfe"),
		apDelta(0, "\\u2028\\ud83d\\ude00 \\u00e9\"}"),
		apEvent("response.output_item.done", 0, apItem("function_call", "a", "c", "apply_patch", "")),
		`{"type":"response.incomplete","sequence_number":3,"response":{"id":"r9","output":[]}}`,
	)...)
	add("Edge/response-done-terminal", apReq, tx(
		apEvent("response.output_item.done", 2, apItem("function_call", "a", "c", "apply_patch", applypatch.WrapInput("x"))),
		`{"type":"response.done","response":{"output":[{"type":"message","id":"m"}]}}`,
	)...)
	add("Edge/failed-upstream-terminal", apReq, append(tx(
		apEvent("response.output_item.added", 0, apItem("function_call", "a", "c", "apply_patch", "")),
		`{"type":"response.failed","response":{"id":"r","status":"failed"}}`,
		apDelta(0, "{}"),
	), apOp{Op: "finish"})...)
	add("Edge/numeric-root-identity", apReq, tx(
		`{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a"}}`,
		`{"type":"response.function_call_arguments.done","output_index":0,"item_id":"a","call_id":7,"name":"apply_patch","namespace":1.50,"arguments":"{\"input\":\"p\"}"}`,
	)...)
	add("Edge/non-string-snapshot", apReq, tx(
		apEvent("response.output_item.added", 0, apItem("function_call", "a", "c", "apply_patch", "")),
		`{"type":"response.function_call_arguments.done","output_index":0,"item_id":"a","arguments":{"input":"p"}}`,
	)...)
	add("Edge/args-after-completion", apReq, tx(
		apEvent("response.output_item.done", 0, apItem("function_call", "a", "c", "apply_patch", applypatch.WrapInput("p"))),
		`{"type":"response.function_call_arguments.delta","output_index":0,"item_id":"a","delta":"x"}`,
	)...)
	add("Edge/snapshot-conflicts-stream", apReq, tx(
		apEvent("response.output_item.added", 0, apItem("function_call", "a", "c", "apply_patch", "")),
		apDelta(0, `{"input":"abc`),
		`{"type":"response.function_call_arguments.done","output_index":0,"item_id":"a","arguments":"{\"input\":\"abd\"}"}`,
	)...)
	add("Edge/stream-conflicts-snapshot", apReq, tx(
		apEvent("response.output_item.added", 0, apItem("function_call", "a", "c", "apply_patch", `{"input":"abc"}`)),
		apDelta(0, `{"input":"abX`),
	)...)
	add("Edge/namespace-ordinary-restore", apNSReq, tx(
		apEvent("response.output_item.added", 0, apItem("function_call", "a", "c", "n__lookup", "")),
		apEvent("response.output_item.done", 0, apItem("function_call", "a", "c", "n__lookup", `{"q":1}`)),
		apEvent("response.output_item.added", 1, apItem("function_call", "b", "d", "n__apply_patch", "")),
		apDelta(1, applypatch.WrapInput("*** Begin Patch\n*** End Patch")),
		`{"type":"response.completed","response":{"id":"rr","output":[`+apItem("function_call", "a", "c", "n__lookup", `{"q":1}`)+`]}}`,
	)...)
	add("Edge/non-stream-envelope", apReq,
		apOp{Op: "non_stream", In: `{"type":"response.completed","response":{"id":"r","output":[` + apItem("function_call", "a", "c", "apply_patch", applypatch.WrapInput("p")) + `,{"type":"message","id":"m"}]}}`},
		apOp{Op: "finish"})
	add("Edge/non-stream-invalid", apReq,
		apOp{Op: "non_stream", In: `{"output":[` + apItem("function_call", "a", "c", "apply_patch", `{"input":1}`) + `]}`},
		apOp{Op: "finish"}, apOp{Op: "transform", In: apCompleted})
	add("Edge/non-stream-object-output", apReq,
		apOp{Op: "non_stream", In: `{"output":` + apItem("function_call", "a", "c", "apply_patch", applypatch.WrapInput("p")) + `}`})
	add("Edge/non-stream-inactive", `{}`,
		apOp{Op: "non_stream", In: `{"output":[` + apItem("function_call", "a", "c", "apply_patch", `{"input":1}`) + `]}`})
	add("Edge/check-identity", apReq,
		apOp{Op: "check_identity", In: `{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","name":"n","namespace":"n"}}`},
		apOp{Op: "check_identity", In: `{"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"a"}}`},
		apOp{Op: "transform", In: apEvent("response.output_item.done", 0, apItem("function_call", "a", "c", "apply_patch", applypatch.WrapInput("p")))})
	add("Edge/explicit-fail", apReq,
		apOp{Op: "transform", In: `{"type":"response.created","sequence_number":4,"response":{"id":"resp_x"}}`},
		apOp{Op: "fail", In: "executor gave up"}, apOp{Op: "fail", In: "again"}, apOp{Op: "finish"})
	add("Edge/additional-tools-winner", `{"input":[{"type":"additional_tools","tools":[{"type":"custom","name":"apply_patch"}]}]}`, tx(
		apEvent("response.output_item.done", 0, apItem("function_call", "a", "c", "apply_patch", applypatch.WrapInput("p"))))...)
	add("Edge/no-type-events", apReq, tx(`{"output_index":0}`, `not json`, ``, `{"type":"response.output_text.delta","sequence_number":0,"delta":"x"}`)...)
	return out
}

func stateScenarios() []apScenario {
	var out []apScenario
	add := func(name, source, original, declarations string, ops ...apOp) {
		out = append(out, apScenario{Name: name, Kind: "state", Source: source, Original: original, Declarations: declarations, Ops: ops})
	}
	disp := apOp{Op: "add_dispatcher", Name: "n", Namespace: "n"}
	stream := func(lines ...string) []apOp {
		var ops []apOp
		for _, l := range lines {
			ops = append(ops, apOp{Op: "stream", In: l})
		}
		return ops
	}
	// TestApplyPatchResponsesHelperNativeSSEBytes.
	add("NativeSSEBytes", "codex", apReq, apReq, stream(
		"event: response.output_item.done",
		`data:   { "type":"response.output_item.done", "output_index":0, "item":{"type":"custom_tool_call","id":"a","name":"apply_patch","input":"raw"}}  `,
		"",
		"event: response.completed",
		`data:  { "type":"response.completed", "sequence_number":8,"response":{"output":[{"type":"custom_tool_call","id":"a","name":"apply_patch","input":"raw"}]}} `,
	)...)
	// TestApplyPatchResponsesHelperDispatcherKeysAndFinal.
	for _, key := range []string{`"output_index":0`, `"call_id":"c"`, `"item_id":"a"`} {
		for _, terminalOnly := range []bool{false, true} {
			item := `{"type":"function_call","id":"a","call_id":"c","name":"apply_patch","namespace":"n","arguments":"{\"input\":\"p\"}"}`
			last := `{"type":"response.output_item.done","output_index":0,"item":` + item + `}`
			if terminalOnly {
				last = `{"type":"response.completed","response":{"output":[` + item + `]}}`
			}
			add(fmt.Sprintf("DispatcherKeysAndFinal/%s/%v", key, terminalOnly), "openai-response", apNSReq, apNSReq, append([]apOp{disp},
				append(tx(`{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"n","namespace":"n","arguments":""}}`,
					`{"type":"response.function_call_arguments.delta",`+key+`,"delta":"{\"name\":\"apply_patch\",\"arguments\":{\"input\":\"p\"}}"}`, last),
					apOp{Op: "bridge_finish"}, apOp{Op: "finish"})...)...)
		}
	}
	// TestApplyPatchResponsesHelperChatFunctionPreference.
	add("ChatFunctionPreference", "openai",
		`{"tools":[{"type":"custom","name":"apply_patch"},{"type":"function","function":{"name":"apply_patch"}}]}`,
		`{"tools":[{"type":"custom","name":"apply_patch"},{"type":"function","name":"apply_patch"}]}`,
		append([]apOp{{Op: "active"}}, tx(`{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","name":"apply_patch","arguments":"ordinary"}}`)...)...)
	// TestApplyPatchResponsesHelperDispatcherOmittedArguments.
	for _, lateName := range []string{"n", "apply_patch"} {
		add("DispatcherOmittedArguments/"+lateName, "openai-response", apNSOnly, apNSOnly, append([]apOp{disp}, tx(
			`{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"n","arguments":""}}`,
			`{"type":"response.function_call_arguments.delta","output_index":0,"item_id":"a","delta":"{\"name\":\"apply_patch\",\"arguments\":{\"input\":\"p\"}}"}`,
			fmt.Sprintf(`{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":%q,"namespace":"n"}}`, lateName),
		)...)...)
	}
	// TestApplyPatchResponsesHelperClosedResponse.
	add("ClosedResponse", "openai-response", apNSOnly, apNSOnly, append(append([]apOp{disp}, tx(
		`{"type":"response.completed","response":{"output":[{"type":"function_call","id":"a","call_id":"c","namespace":"n","name":"apply_patch","arguments":"{\"input\":\"p\"}"}]}}`,
		`{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","call_id":"changed","name":"n","arguments":""}}`,
	)...), apOp{Op: "finish"})...)
	// TestApplyPatchResponsesHelperTransportTerminal.
	complete := `data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"apply_patch","arguments":"{\"input\":\"p\"}"}}`
	for _, jsonTerminal := range []bool{false, true} {
		lines := []string{complete}
		if jsonTerminal {
			lines = append(lines, `data: {"type":"response.completed","response":{"output":[]}}`)
		}
		lines = append(lines, "data:   [DONE]  ", complete, "event: response.completed", "", ": keepalive", "data:   [DONE]  ")
		add(fmt.Sprint("TransportTerminal/", jsonTerminal), "openai-response", apReq, apReq, append(stream(lines...), tx(apCompleted)...)...)
	}
	// TestApplyPatchResponsesHelperFailedTransport.
	add("FailedTransport", "openai-response", apReq, apReq, stream(
		`data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","name":"apply_patch","arguments":""}}`,
		"data: [DONE]", "data: [DONE]", `data: {"type":"response.completed","response":{"output":[]}}`, "event: response.completed")...)
	// TestApplyPatchResponsesHelperInactiveTransportBytes.
	add("InactiveTransportBytes", "openai-response", `{"tools":[{"type":"function","name":"apply_patch"}]}`, `{"tools":[{"type":"function","name":"apply_patch"}]}`, stream(
		"data: [DONE]", `data:  { "type":"response.completed", "response":{"output":[]}} `, "event: response.completed", "", "data: [DONE]")...)
	// TestApplyPatchResponsesHelperRetainedDispatcherProvenance.
	patch := `{"name":"apply_patch","arguments":{"input":"p"}}`
	ordinaryWrapper := `{"name":"lookup","arguments":{"input":"p"}}`
	for _, tc := range []struct {
		name      string
		delta     string
		wrappers  []string
		finalName string
	}{
		{"patch_then_ordinary", "", []string{patch, ordinaryWrapper}, "n"},
		{"ordinary_then_patch", "", []string{ordinaryWrapper, patch}, "n"},
		{"conflicting_inputs", "", []string{patch, `{"name":"apply_patch","arguments":{"input":"q"}}`}, "n"},
		{"full_source_conflicts_with_snapshot", patch, []string{ordinaryWrapper}, "n"},
		{"full_source_conflicts_with_child", patch, nil, "lookup"},
		{"ordinary_child_is_not_patch", "", []string{ordinaryWrapper}, "n"},
		{"ordinary_arguments_are_not_dispatcher_provenance", "", []string{`{"name":"lookup","arguments":{"name":"apply_patch","arguments":{"input":"not patch"}}}`}, "n"},
	} {
		ops := []apOp{disp, {Op: "transform", In: `{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","name":"n","arguments":""}}`}}
		if tc.delta != "" {
			ops = append(ops, apOp{Op: "transform", In: fmt.Sprintf(`{"type":"response.function_call_arguments.delta","item_id":"a","delta":%q}`, tc.delta)})
		}
		for _, wrapper := range tc.wrappers {
			var inner json.RawMessage
			var w struct {
				Arguments json.RawMessage `json:"arguments"`
			}
			_ = json.Unmarshal([]byte(wrapper), &w)
			inner = w.Arguments
			ops = append(ops,
				apOp{Op: "remember_args", In: fmt.Sprintf(`{"type":"response.function_call_arguments.done","item_id":"a","arguments":%q}`, wrapper)},
				apOp{Op: "transform", In: fmt.Sprintf(`{"type":"response.function_call_arguments.done","item_id":"a","arguments":%q}`, string(inner))})
		}
		ops = append(ops, apOp{Op: "transform", In: fmt.Sprintf(`{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"a","call_id":"late","name":%q,"namespace":"n"}}`, tc.finalName)})
		add("RetainedDispatcherProvenance/"+tc.name, "openai-response", apNSReq, apNSReq, ops...)
	}
	// TestApplyPatchResponsesHelperDispatcherSnapshotsAllMatched.
	for discover := 0; discover < 3; discover++ {
		ops := []apOp{disp}
		for i := 0; i < 3; i++ {
			ops = append(ops, apOp{Op: "transform", In: fmt.Sprintf(`{"type":"response.output_item.added","output_index":%d,"item":{"type":"function_call","id":"i%d","call_id":"c%d","name":"n","arguments":""}}`, i, i, i)})
		}
		ops = append(ops,
			apOp{Op: "remember_args", In: `{"type":"response.function_call_arguments.done","output_index":0,"item_id":"i1","call_id":"c2","arguments":"{\"name\":\"apply_patch\",\"arguments\":{\"input\":\"p\"}}"}`},
			apOp{Op: "transform", In: `{"type":"response.function_call_arguments.done","output_index":0,"item_id":"i1","call_id":"c2","arguments":"{\"input\":\"p\"}"}`},
			apOp{Op: "transform", In: fmt.Sprintf(`{"type":"response.output_item.done","output_index":%d,"item":{"type":"function_call","id":"i%d","call_id":"c%d","name":"n"}}`, discover, discover, discover)})
		add(fmt.Sprint("DispatcherSnapshotsAllMatched/", discover), "openai-response", apNSOnly, apNSOnly, ops...)
	}
	// TestApplyPatchResponsesHelperOrdinaryProgress.
	ops := []apOp{disp}
	for _, raw := range []string{
		`{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","name":"lookup","namespace":"n","arguments":""}}`,
		`{"type":"response.function_call_arguments.delta","item_id":"a","delta":"{\"x\":1}"}`,
		`{"type":"response.function_call_arguments.done","item_id":"a","arguments":"{\"x\":1}"}`,
		`{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"lookup","namespace":"n","arguments":"{\"x\":1}"}}`,
	} {
		ops = append(ops, apOp{Op: "remember_event", In: raw}, apOp{Op: "transform", In: raw})
	}
	add("OrdinaryProgress", "openai-response", apNSReq, apNSReq, ops...)
	// TestApplyPatchResponsesHelperDispatcherLifecycle.
	for _, closeAt := range []string{"response", "sentinel", "upstream_failure", "local_failure"} {
		ops := []apOp{disp}
		for _, raw := range []string{
			`{"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"a"}}`,
			`{"type":"response.function_call_arguments.done","item_id":"a","arguments":"{\"name\":\"apply_patch\",\"arguments\":{\"input\":\"p\"}}"}`,
		} {
			ops = append(ops, apOp{Op: "remember_event", In: raw}, apOp{Op: "transform", In: raw})
		}
		ops = append(ops, apOp{Op: "transform", In: `{"type":"response.output_item.done","output_index":2,"item":{"type":"function_call","id":"a","call_id":"c","name":"n"}}`},
			apOp{Op: "bridge_finish"}, apOp{Op: "finish"})
		switch closeAt {
		case "response":
			ops = append(ops, apOp{Op: "transform", In: `{"type":"response.completed","response":{"output":[{"type":"function_call","id":"a","call_id":"c","name":"n"}]}}`})
		case "sentinel":
			ops = append(ops, apOp{Op: "stream", In: "data: [DONE]"})
		case "upstream_failure":
			ops = append(ops, apOp{Op: "transform", In: `{"type":"response.failed","response":{"output":[]}}`})
		case "local_failure":
			ops = append(ops, apOp{Op: "transform", In: `{"type":"response.output_item.done","output_index":3,"item":{"type":"function_call","id":"a","name":"n"}}`})
		}
		ops = append(ops, apOp{Op: "transform", In: `{"type":"response.output_item.done","output_index":2,"item":{"type":"function_call","id":"a","name":"n"}}`}, apOp{Op: "finish"})
		add("DispatcherLifecycle/"+closeAt, "openai-response", apNSOnly, apNSOnly, ops...)
	}
	// TestApplyPatchResponsesHelperLateChildNamespace and TerminalSourceIdentity.
	ops = []apOp{disp}
	for _, raw := range []string{
		`{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","name":"n","arguments":""}}`,
		`{"type":"response.function_call_arguments.done","item_id":"a","arguments":"{\"name\":\"apply_patch\",\"arguments\":{\"input\":\"p\"}}"}`,
		`{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"apply_patch"}}`,
	} {
		ops = append(ops, apOp{Op: "remember_event", In: raw}, apOp{Op: "transform", In: raw})
	}
	add("LateChildNamespace", "openai-response", apNSOnly, apNSOnly, ops...)
	ops = []apOp{disp}
	for _, raw := range []string{
		`{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"a","name":"n","arguments":""}}`,
		`{"type":"response.function_call_arguments.done","item_id":"a","arguments":"{\"name\":\"apply_patch\",\"arguments\":{\"input\":\"p\"}}"}`,
		`{"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"a","call_id":"c","name":"n"}}`,
	} {
		ops = append(ops, apOp{Op: "remember_event", In: raw}, apOp{Op: "transform", In: raw})
	}
	ops = append(ops,
		apOp{Op: "remember_event", In: `{"type":"response.completed","response":{"output":[{"type":"message","id":"removed"},{"type":"function_call","id":"a","call_id":"c","name":"n","namespace":"other"}]}}`},
		apOp{Op: "transform", In: `{"type":"response.completed","response":{"output":[{"type":"function_call","id":"a","call_id":"c","name":"n","namespace":"n"}]}}`})
	add("TerminalSourceIdentity", "openai-response", apNSOnly, apNSOnly, ops...)
	// TestApplyPatchResponsesHelperIndexOnlyTerminal.
	ops = []apOp{disp}
	for _, raw := range []string{
		`{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call"}}`,
		`{"type":"response.function_call_arguments.done","output_index":0,"arguments":"{\"name\":\"apply_patch\",\"arguments\":{\"input\":\"p\"}}"}`,
		`{"type":"response.function_call_arguments.done","output_index":0,"item_id":"a","call_id":"c","arguments":"{\"name\":\"apply_patch\",\"arguments\":{\"input\":\"p\"}}"}`,
		`{"type":"response.completed","response":{"output":[{"type":"function_call","name":"n"}]}}`,
	} {
		ops = append(ops, apOp{Op: "remember_event", In: raw}, apOp{Op: "transform", In: raw})
	}
	add("IndexOnlyTerminal", "openai-response", apNSOnly, apNSOnly, ops...)
	// TestApplyPatchResponsesHelperCompletedOrdinaryDelta.
	ops = []apOp{disp}
	for _, raw := range []string{
		`{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","name":"n"}}`,
		`{"type":"response.function_call_arguments.done","item_id":"a","arguments":"{\"name\":\"lookup\",\"arguments\":{\"x\":1}}"}`,
		`{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"n"}}`,
		`{"type":"response.function_call_arguments.delta","item_id":"a","delta":"ordinary"}`,
	} {
		ops = append(ops, apOp{Op: "remember_event", In: raw}, apOp{Op: "transform", In: raw})
	}
	add("CompletedOrdinaryDelta", "openai-response", apNSReq, apNSReq, ops...)
	// TestApplyPatchResponsesHelperSourceTerminalRequired.
	for _, mode := range []string{"empty", "arguments", "item"} {
		var ops []apOp
		if mode != "empty" {
			event := `{"type":"response.function_call_arguments.done","item_id":"a","arguments":"{\"input\":\"p\"}"}`
			if mode == "item" {
				event = `{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"a","name":"apply_patch","arguments":"{\"input\":\"p\"}"}}`
			}
			ops = tx(`{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","name":"apply_patch","arguments":""}}`, event)
		}
		ops = append(ops, apOp{Op: "finish_stream"}, apOp{Op: "finish_stream"})
		add("SourceTerminalRequired/"+mode, "codex", apReq, apReq, ops...)
	}
	// TestApplyPatchResponsesHelperInactiveEOFAndDONE.
	for i, request := range []string{`{}`, `{"tools":[{"type":"function","name":"apply_patch"}]}`} {
		add(fmt.Sprint("InactiveEOFAndDONE/", i), "codex", request, request, apOp{Op: "finish_stream"}, apOp{Op: "stream", In: "data: [DONE]"})
	}
	// Edge cases: a full SSE conversion with event-line renaming and keepalives.
	add("Edge/sse-conversion", "openai-response", apReq, apReq, stream(
		"event: response.created",
		`data: {"type":"response.created","sequence_number":1,"response":{"id":"resp_1"}}`,
		"",
		": keepalive",
		"event: response.output_item.added",
		`data: {"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":{"type":"function_call","id":"fc","call_id":"cc","name":"apply_patch","arguments":""}}`,
		"",
		"event: response.function_call_arguments.delta",
		`data: {"type":"response.function_call_arguments.delta","sequence_number":3,"output_index":0,"item_id":"fc","delta":"{\"input\":\"*** Begin"}`,
		"event: response.function_call_arguments.delta",
		`data: {"type":"response.function_call_arguments.delta","sequence_number":4,"output_index":0,"item_id":"fc","delta":" Patch\"}"}`,
		"event: response.output_item.done",
		`data: {"type":"response.output_item.done","sequence_number":5,"output_index":0,"item":{"type":"function_call","id":"fc","call_id":"cc","name":"apply_patch","arguments":"{\"input\":\"*** Begin Patch\"}"}}`,
		"event: response.completed",
		`data: {"type":"response.completed","sequence_number":6,"response":{"id":"resp_1","output":[{"type":"function_call","id":"fc","call_id":"cc","name":"apply_patch","arguments":"{\"input\":\"*** Begin Patch\"}"}]}}`,
		"data: [DONE]",
	)...)
	add("Edge/sse-invalid-mid-stream", "openai-response", apReq, apReq, append(stream(
		"event: response.output_item.added",
		`data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc","call_id":"cc","name":"apply_patch","arguments":""}}`,
		"event: response.function_call_arguments.done",
		`data: {"type":"response.function_call_arguments.done","output_index":0,"item_id":"fc","arguments":"{\"input\":7}"}`,
		"data: [DONE]",
	), apOp{Op: "finish_stream"})...)
	add("Edge/dispatcher-unfinished-terminal", "openai-response", apNSOnly, apNSOnly, append([]apOp{disp}, tx(
		`{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"n","arguments":""}}`,
		`{"type":"response.function_call_arguments.delta","output_index":0,"item_id":"a","delta":"{\"name\":\"apply_patch\""}`,
		apCompleted,
	)...)...)
	add("Edge/dispatcher-ignored-without-patch-namespace", "openai-response", apReq, apReq,
		apOp{Op: "add_dispatcher", Name: "m", Namespace: "m"}, apOp{Op: "remember_event", In: `{"type":"response.function_call_arguments.done","item_id":"a","arguments":"{}"}`},
		apOp{Op: "transform", In: `{"type":"response.function_call_arguments.done","output_index":0,"item_id":"a","arguments":"{}"}`})
	add("Edge/chat-source-without-functions", "openai", `{"tools":[{"type":"custom","name":"apply_patch"}]}`, apReq,
		append([]apOp{{Op: "active"}}, tx(apEvent("response.output_item.done", 0, apItem("function_call", "a", "c", "apply_patch", applypatch.WrapInput("p"))), apCompleted)...)...)
	return out
}

func normalizeScenarios() []apScenario {
	var out []apScenario
	norm := func(name, kind, original string, inputs ...string) {
		var ops []apOp
		for _, in := range inputs {
			ops = append(ops, apOp{Op: "normalize", In: in})
		}
		out = append(out, apScenario{Name: name, Kind: kind, Original: original, Ops: ops})
	}
	// TestApplyPatchResponsesRequestHistoryAndWinners.
	var history []string
	for _, req := range []string{
		`{"tools":[{"type":"custom","name":"apply_patch"}],"input":[]}`,
		`{"tools":[{"type":"function","name":"apply_patch"}],"input":[]}`,
		`{"input":[]}`,
	} {
		history = append(history, strings.Replace(req, `"input":[]`, `"input":[{"type":"custom_tool_call","call_id":"old","name":"apply_patch","input":"{\"input\":\"raw\"}"},{"type":"custom_tool_call_output","call_id":"old","output":"ok"},{"type":"function_call","call_id":"fn","name":"apply_patch","arguments":"{\"input\":\"existing\"}"}]`, 1))
	}
	norm("HistoryAndWinners/history", "normalize", "", history...)
	norm("HistoryAndWinners/winners", "normalize", "",
		`{"tools":[{"type":"function","name":"apply_patch","description":"ordinary"}],"input":[{"type":"additional_tools","tools":[{"type":"custom","name":"apply_patch"}]}]}`,
		`{"tools":[{"type":"namespace","name":"n","tools":[{"type":"custom","name":"apply_patch"}]},{"type":"function","name":"n__apply_patch","description":"ordinary"}]}`)
	// Edge cases.
	norm("Edge/declarations", "normalize", "",
		`{"tools":[{"type":"custom","name":" apply_patch ","description":"Edit files. This is a FREEFORM tool, so do not wrap the patch in JSON.","format":{"type":"grammar","definition":"start: begin_patch\n*** Environment ID: x <&>"}}]}`,
		`{"tools":[{"type":"custom","name":"apply_patch"},{"type":"custom","name":"apply_patch","description":"second"}],"tool_choice":{"type":"custom","name":"apply_patch"}}`,
		`{"tools":[{"type":"function","name":"apply_patch"},{"type":"custom","name":"apply_patch"}],"tool_choice":{"type":"custom","name":"apply_patch"}}`,
		`{"tools":[{"type":"namespace","name":"n","children":[{"type":"custom","name":"apply_patch"},{"type":"function","name":"lookup"}]}],"tool_choice":{"type":"allowed_tools","tools":[{"type":"custom","name":"apply_patch","namespace":"n"},{"type":"function","name":"lookup"}]}}`,
		`{"tools":[{"type":"custom","name":"apply_patch"}],"input":[{"type":"additional_tools","tools":[{"type":"custom","name":"apply_patch"}]}]}`,
		`{"tools":{"type":"custom","name":"apply_patch"}}`,
		`{"tools":[{"type":"custom","name":"apply_patch"}],"input":"plain"}`,
		`{"tools":[{"type":"custom","name":"other"}],"input":[{"type":"custom_tool_call","call_id":"x","name":"other","input":5}]}`,
	)
	norm("Edge/errors", "normalize", "",
		`{"tools":[{"type":"custom","name":"apply_patch"}],"input":[{"type":"custom_tool_call","call_id":"x","name":"apply_patch","input":{"a":1}}]}`,
		`{"tools":[`, ``, "{\"input\":[{\"type\":\"custom_tool_call\",\"call_id\":\"x\",\"name\":\"apply_patch\",\"input\":\"<p>\xff\"}]}")
	// TestApplyPatchResponsesHelperRequestChatPreference.
	norm("RequestChatPreference", "normalize_executor",
		`{"tools":[{"type":"custom","name":"apply_patch"},{"type":"function","function":{"name":"apply_patch","parameters":{"type":"object","properties":{"x":{"type":"integer"}}}}}]}`,
		`{"tools":[{"type":"custom","name":"apply_patch"},{"type":"function","name":"apply_patch","parameters":{"type":"object","properties":{"x":{"type":"integer"}}}}]}`)
	norm("Edge/executor-without-original", "normalize_executor", "",
		`{"tools":[{"type":"custom","name":"apply_patch"}]}`, `{"input":[]}`)
	norm("Edge/executor-original-no-tools", "normalize_executor", `{"tools":[{"type":"function","function":{"name":"apply_patch"}}]}`,
		`{"input":[]}`, `{"tools":[{"type":"custom","name":"apply_patch"}]}`)
	return out
}

func codexScenarios() []apScenario {
	var out []apScenario
	original := apReq
	bridged, _ := translatorcommon.NormalizeApplyPatchResponsesRequest([]byte(original))
	raw := `data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"apply_patch","arguments":"{\"input\":\"p\"}"}}`
	add := func(name, kind string, bridgedParam bool, declarations string, lines ...string) {
		var ops []apOp
		for _, l := range lines {
			ops = append(ops, apOp{Op: "line", In: l})
		}
		out = append(out, apScenario{Name: name, Kind: kind, Request: original, Original: original, Declarations: declarations, Model: "m", Bridged: bridgedParam, Ops: ops})
	}
	// TestApplyPatchResponsesActualRequestGatesNativeCodex.
	add("GatesNativeCodex/native", "codex_stream", false, original, raw)
	add("GatesNativeCodex/configured", "codex_stream", false, string(bridged), raw)
	add("GatesNativeCodex/bridged", "codex_stream", true, string(bridged), raw)
	add("GatesNativeCodex/failure", "codex_stream", true, string(bridged), `data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","name":"apply_patch","arguments":"{}"}}`, raw)
	add("Bridged/full-stream", "codex_stream", true, string(bridged),
		`data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_9"}}`,
		`data: {"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"apply_patch","arguments":""}}`,
		`data: {"type":"response.function_call_arguments.delta","sequence_number":2,"output_index":0,"item_id":"a","delta":"{\"input\":\"x"}`,
		`data: {"type":"response.function_call_arguments.done","sequence_number":3,"output_index":0,"item_id":"a","arguments":"{\"input\":\"xy\"}"}`,
		`{"type":"response.output_item.done","sequence_number":4,"output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"apply_patch","arguments":"{\"input\":\"xy\"}"}}`,
		`data: {"type":"response.completed","sequence_number":5,"response":{"id":"resp_9","output":[]}}`)
	// TestConvertCodexResponseToOpenAIResponsesNonStreamIncomplete and bridged bodies.
	add("NonStream/incomplete", "codex_non_stream", false, original, `{"type":"response.incomplete","response":{"id":"resp_1","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}`)
	add("NonStream/bridged", "codex_non_stream", true, string(bridged), `{"type":"response.completed","response":{"id":"r","output":[`+apItem("function_call", "a", "c", "apply_patch", applypatch.WrapInput("p"))+`]}}`)
	add("NonStream/bridged-bare", "codex_non_stream", true, string(bridged), `{"id":"r","output":[`+apItem("function_call", "a", "c", "apply_patch", applypatch.WrapInput("p"))+`]}`)
	add("NonStream/bridged-invalid", "codex_non_stream", true, string(bridged), `{"type":"response.completed","response":{"output":[`+apItem("function_call", "a", "c", "apply_patch", `{"input":3}`)+`]}}`)
	add("NonStream/bridged-other-type", "codex_non_stream", true, string(bridged), `{"type":"response.created","response":{"id":"r"}}`)
	return out
}

// apGoTests names each ported scenario after the Go test it reproduces.
var apGoTests = [][2]string{
	{"DeltaAndFourCompletions/", "TestApplyPatchResponsesBridgeDeltaAndFourCompletions/"},
	{"Passthrough/", "TestApplyPatchResponsesBridgePassthrough/"},
	{"IdentityAndSnapshotEvidence/", "TestApplyPatchResponsesBridgeIdentityAndSnapshotEvidence/"},
	{"StagesAndFinish/", "TestApplyPatchResponsesBridgeStagesAndFinish/"},
	{"HistoryAndWinners/", "TestApplyPatchResponsesRequestHistoryAndWinners/"},
	{"NamespaceMixedNonStream", "TestApplyPatchResponsesNamespaceMixedNonStream"},
	{"MissingSnapshots/", "TestApplyPatchResponsesContinuationMissingSnapshots/"},
	{"NativeTerminalBytes", "TestApplyPatchResponsesContinuationNativeTerminalBytes"},
	{"RootLateName", "TestApplyPatchResponsesContinuationRootLateName"},
	{"NoInventedPreview", "TestApplyPatchResponsesContinuationNoInventedPreview"},
	{"AllMatchedProvenance/", "TestApplyPatchResponsesContinuationAllMatchedProvenance/"},
	{"CompletedWindow/", "TestApplyPatchResponsesContinuationCompletedWindow/"},
	{"OmittedMixedCompleted/", "TestApplyPatchResponsesContinuationOmittedMixedCompletedItems/"},
	{"UnmatchedIdentityEvidence/", "TestApplyPatchResponsesContinuationUnmatchedIdentityEvidence/"},
	{"MixedSequence", "TestApplyPatchResponsesContinuationMixedSequence"},
	{"OrdinaryRootNamePassthrough", "TestApplyPatchResponsesContinuationOrdinaryRootNamePassthrough"},
	{"MixedNativeBytes", "TestApplyPatchResponsesContinuationMixedNativeBytes"},
	{"NamedLateIdentity/", "TestApplyPatchResponsesNamedLateIdentity/"},
	{"NamedLateIdentityEvidence/", "TestApplyPatchResponsesNamedLateIdentityEvidence/"},
	{"NamedLateIdentityInterleaved", "TestApplyPatchResponsesNamedLateIdentityInterleaved"},
	{"NativeSSEBytes", "TestApplyPatchResponsesHelperNativeSSEBytes"},
	{"DispatcherKeysAndFinal/", "TestApplyPatchResponsesHelperDispatcherKeysAndFinal/"},
	{"ChatFunctionPreference", "TestApplyPatchResponsesHelperChatFunctionPreference"},
	{"DispatcherOmittedArguments/", "TestApplyPatchResponsesHelperDispatcherOmittedArguments/"},
	{"ClosedResponse", "TestApplyPatchResponsesHelperClosedResponse"},
	{"TransportTerminal/", "TestApplyPatchResponsesHelperTransportTerminal/"},
	{"FailedTransport", "TestApplyPatchResponsesHelperFailedTransport"},
	{"InactiveTransportBytes", "TestApplyPatchResponsesHelperInactiveTransportBytes"},
	{"RetainedDispatcherProvenance/", "TestApplyPatchResponsesHelperRetainedDispatcherProvenance/"},
	{"DispatcherSnapshotsAllMatched/", "TestApplyPatchResponsesHelperDispatcherSnapshotsAllMatched/"},
	{"OrdinaryProgress", "TestApplyPatchResponsesHelperOrdinaryProgress"},
	{"DispatcherLifecycle/", "TestApplyPatchResponsesHelperDispatcherLifecycle/"},
	{"LateChildNamespace", "TestApplyPatchResponsesHelperLateChildNamespace"},
	{"TerminalSourceIdentity", "TestApplyPatchResponsesHelperTerminalSourceIdentity"},
	{"IndexOnlyTerminal", "TestApplyPatchResponsesHelperIndexOnlyTerminal"},
	{"CompletedOrdinaryDelta", "TestApplyPatchResponsesHelperCompletedOrdinaryDelta"},
	{"SourceTerminalRequired/", "TestApplyPatchResponsesHelperSourceTerminalRequired/"},
	{"InactiveEOFAndDONE/", "TestApplyPatchResponsesHelperInactiveEOFAndDONE/"},
	{"RequestChatPreference", "TestApplyPatchResponsesHelperRequestChatPreference"},
	{"GatesNativeCodex/", "TestApplyPatchResponsesActualRequestGatesNativeCodex/"},
	{"NonStream/incomplete", "TestConvertCodexResponseToOpenAIResponsesNonStreamIncomplete"},
}

func apGoTestName(name string) string {
	for _, m := range apGoTests {
		if strings.HasSuffix(m[0], "/") && strings.HasPrefix(name, m[0]) {
			return m[1] + name[len(m[0]):]
		}
		if name == m[0] {
			return m[1]
		}
	}
	return name
}

func applyPatchResponses(outDir string) {
	scenarios := append(append(append(bridgeScenarios(), stateScenarios()...), normalizeScenarios()...), codexScenarios()...)
	for i := range scenarios {
		runAP(&scenarios[i])
		scenarios[i].Name = apGoTestName(scenarios[i].Name)
	}
	raw, err := json.MarshalIndent(scenarios, "", " ")
	if err != nil {
		panic(err)
	}
	if err := os.WriteFile(filepath.Join(outDir, "..", "apply_patch_responses.json"), append(raw, '\n'), 0o644); err != nil {
		panic(err)
	}
	fmt.Printf("apply_patch_responses: %d scenarios\n", len(scenarios))
}
