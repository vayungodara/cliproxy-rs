//! Codex `apply_patch` custom tool description (internal/client/codex/apply-patch).
//
// ponytail: only tool detection and the Claude-facing declaration are ported; the
// Responses bridge and patch input wrapping arrive with the apply_patch stream support.

use cpa_common::json::Res;

pub(crate) const PARAMETERS: &str = r#"{"type":"object","properties":{"input":{"type":"string","description":"The complete apply_patch patch text."}},"required":["input"],"additionalProperties":false}"#;

const INSTRUCTIONS: &str = "Call this function with a JSON object whose input field contains the complete patch text.
Use the Codex apply_patch format, not a conventional git unified diff.
Start with *** Begin Patch and end with *** End Patch.
Use *** Add File: path, *** Delete File: path, or *** Update File: path.
Every added-file content line starts with +.
For updates, use @@; context lines start with one space, removed lines with -, and added lines with +.
Use *** Move to: path for a rename and *** End of File when required by the patch grammar.
Example input:
*** Begin Patch
*** Update File: src/main.go
@@
-old
+new
*** End Patch";

/// applypatch.IsCustomTool.
pub(crate) fn is_custom_tool(tool: &Res<'_>) -> bool {
    tool.get("type").str() == "custom" && crate::common::trim_space(&tool.get("name").bytes()) == b"apply_patch"
}

/// applypatch.Description: the original description (minus the freeform warning), the
/// JSON-wrapper instructions, and the original grammar.
pub(crate) fn description(tool: &Res<'_>) -> Vec<u8> {
    let original = tool.get("description").bytes();
    let warning: &[u8] = b"This is a FREEFORM tool, so do not wrap the patch in JSON.";
    let mut stripped = vec![];
    let mut rest = &original[..];
    while let Some(i) = rest.windows(warning.len()).position(|w| w == warning) {
        stripped.extend_from_slice(&rest[..i]);
        rest = &rest[i + warning.len()..];
    }
    stripped.extend_from_slice(rest);
    let mut out = vec![];
    if !crate::common::trim_space(&stripped).is_empty() {
        out.extend_from_slice(&stripped);
        out.extend_from_slice(b"\n\n");
    }
    out.extend_from_slice(INSTRUCTIONS.as_bytes());
    let grammar = tool.get("format.definition").bytes();
    if !grammar.is_empty() {
        if grammar.windows(19).any(|w| w == b"*** Environment ID:") {
            out.extend_from_slice(b"\n\nUse *** Environment ID: as specified by the patch grammar.");
        }
        out.extend_from_slice(b"\n\nOriginal patch grammar:\n");
        out.extend_from_slice(&grammar);
    }
    out
}
