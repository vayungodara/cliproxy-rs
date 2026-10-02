"""Wraps CLIProxyAPI thinking/signature functions with recorders (see README.md).

Usage: patch.py COPY_ROOT
"""
import pathlib, sys
root = pathlib.Path(sys.argv[1])
IMPORT_REC = '"github.com/router-for-me/CLIProxyAPI/v8/internal/recorder"'

def rename(path, old, new):
    p = root / path
    s = p.read_text()
    assert s.count(old) == 1, (path, old, s.count(old))
    p.write_text(s.replace(old, new))

def write(path, body, imports):
    imp = '\n'.join('\t' + i for i in imports)
    (root / path).write_text(f"package {body[0]}\n\nimport (\n{imp}\n)\n\n{body[1]}\n")

# ---------------- thinking ----------------
for f in ['apply.go', 'summary.go']:
    p = root / 'internal/thinking' / f
    s = p.read_text()
    p.write_text(s.replace('registry.LookupModelInfo(', 'recLookup('))
rename('internal/thinking/apply.go', 'func applyThinking(', 'func applyThinkingImpl(')
rename('internal/thinking/validate.go', 'func ValidateConfig(', 'func validateConfigImpl(')
rename('internal/thinking/summary.go', 'func ExtractSummaryConfig(', 'func extractSummaryConfigImpl(')
rename('internal/thinking/summary.go', 'func ExtractExplicitSummaryConfig(', 'func extractExplicitSummaryConfigImpl(')
rename('internal/thinking/summary.go', 'func ExtractTranslatedSummaryConfig(', 'func extractTranslatedSummaryConfigImpl(')
rename('internal/thinking/summary.go', 'func applySummaryConfigForProvider(', 'func applySummaryConfigForProviderImpl(')
rename('internal/thinking/apply.go', 'func ExtractReasoningEffort(', 'func extractReasoningEffortImpl(')
rename('internal/thinking/apply.go', 'func ExtractTranslatedReasoningEffort(', 'func extractTranslatedReasoningEffortImpl(')
rename('internal/thinking/strip.go', 'func StripThinkingConfig(', 'func stripThinkingConfigImpl(')
rename('internal/thinking/suffix.go', 'func ParseSuffix(', 'func parseSuffixImpl(')
write('internal/thinking/zz_record.go', ('thinking', r'''
func recLookup(id string, provider ...string) *registry.ModelInfo {
	info := registry.LookupModelInfo(id, provider...)
	p := ""
	if len(provider) > 0 {
		p = provider[0]
	}
	recorder.Lookup(id, p, info)
	return info
}

func recCfg(c ThinkingConfig) any {
	return map[string]any{"mode": c.Mode.String(), "budget": c.Budget, "level": string(c.Level)}
}

func recSum(c SummaryConfig) any {
	mode := "unspecified"
	switch c.Mode {
	case SummaryDisabled:
		mode = "disabled"
	case SummaryEnabled:
		mode = "enabled"
	}
	return map[string]any{"mode": mode, "detail": c.Detail}
}

func recErr(err error) any {
	if err == nil {
		return nil
	}
	code := ""
	var te *ThinkingError
	if errors.As(err, &te) {
		code = string(te.Code)
	}
	return map[string]any{"message": err.Error(), "code": code}
}

func applyThinking(body, sourceBody []byte, model string, fromFormat string, toFormat string, providerKey string, resolvedModelInfo *registry.ModelInfo, modelInfoResolved bool, summaryConfig SummaryConfig, normalizedUpdatesChanged ...bool) ([]byte, error) {
	recorder.Begin()
	out, err := applyThinkingImpl(body, sourceBody, model, fromFormat, toFormat, providerKey, resolvedModelInfo, modelInfoResolved, summaryConfig, normalizedUpdatesChanged...)
	updates := len(normalizedUpdatesChanged) > 0 && normalizedUpdatesChanged[0]
	recorder.End("apply_thinking", map[string]any{"body": recorder.Str(body), "source": recorder.Str(sourceBody), "model": model, "from": fromFormat, "to": toFormat, "provider": providerKey, "resolved": modelInfoResolved, "info": recorder.Caps(resolvedModelInfo), "summary": recSum(summaryConfig), "updates_changed": updates}, map[string]any{"body": recorder.Str(out), "err": recErr(err)})
	return out, err
}

func ValidateConfig(config ThinkingConfig, modelInfo *registry.ModelInfo, fromFormat, toFormat string, fromSuffix bool) (*ThinkingConfig, error) {
	recorder.Begin()
	out, err := validateConfigImpl(config, modelInfo, fromFormat, toFormat, fromSuffix)
	var o any
	if out != nil {
		o = recCfg(*out)
	}
	recorder.End("validate_config", map[string]any{"config": recCfg(config), "info": recorder.Caps(modelInfo), "from": fromFormat, "to": toFormat, "from_suffix": fromSuffix}, map[string]any{"config": o, "err": recErr(err)})
	return out, err
}

func ExtractSummaryConfig(body []byte, format string) SummaryConfig {
	recorder.Begin()
	out := extractSummaryConfigImpl(body, format)
	recorder.End("extract_summary", map[string]any{"body": recorder.Str(body), "format": format}, recSum(out))
	return out
}

func ExtractExplicitSummaryConfig(body []byte, format string) SummaryConfig {
	recorder.Begin()
	out := extractExplicitSummaryConfigImpl(body, format)
	recorder.End("extract_explicit_summary", map[string]any{"body": recorder.Str(body), "format": format}, recSum(out))
	return out
}

func ExtractTranslatedSummaryConfig(body []byte, sourceFormat, targetFormat string) SummaryConfig {
	recorder.Begin()
	out := extractTranslatedSummaryConfigImpl(body, sourceFormat, targetFormat)
	recorder.End("extract_translated_summary", map[string]any{"body": recorder.Str(body), "from": sourceFormat, "to": targetFormat}, recSum(out))
	return out
}

func applySummaryConfigForProvider(body []byte, format, model, provider string, modelInfo *registry.ModelInfo, config SummaryConfig) []byte {
	recorder.Begin()
	out := applySummaryConfigForProviderImpl(body, format, model, provider, modelInfo, config)
	recorder.End("apply_summary", map[string]any{"body": recorder.Str(body), "format": format, "model": model, "provider": provider, "info": recorder.Caps(modelInfo), "config": recSum(config)}, recorder.Str(out))
	return out
}

func ExtractReasoningEffort(body []byte, provider, model string) string {
	recorder.Begin()
	out := extractReasoningEffortImpl(body, provider, model)
	recorder.End("extract_reasoning_effort", map[string]any{"body": recorder.Str(body), "provider": provider, "model": model}, out)
	return out
}

func ExtractTranslatedReasoningEffort(body []byte, provider string) string {
	recorder.Begin()
	out := extractTranslatedReasoningEffortImpl(body, provider)
	recorder.End("extract_translated_reasoning_effort", map[string]any{"body": recorder.Str(body), "provider": provider}, out)
	return out
}

func StripThinkingConfig(body []byte, provider string) []byte {
	recorder.Begin()
	out := stripThinkingConfigImpl(body, provider)
	recorder.End("strip_thinking", map[string]any{"body": recorder.Str(body), "provider": provider}, recorder.Str(out))
	return out
}

func ParseSuffix(model string) SuffixResult {
	recorder.Begin()
	out := parseSuffixImpl(model)
	recorder.End("parse_suffix", model, map[string]any{"model_name": out.ModelName, "has_suffix": out.HasSuffix, "raw_suffix": out.RawSuffix})
	return out
}
'''), ['"errors"', '', '"github.com/router-for-me/CLIProxyAPI/v8/internal/registry"', IMPORT_REC])

for prov in ['antigravity', 'claude', 'codex', 'gemini', 'interactions', 'kimi', 'openai']:
    rename(f'internal/thinking/provider/{prov}/apply.go', 'func (a *Applier) Apply(', 'func (a *Applier) applyImpl(')
    write(f'internal/thinking/provider/{prov}/zz_record.go', (prov, f'''
func (a *Applier) Apply(body []byte, config thinking.ThinkingConfig, modelInfo *registry.ModelInfo) ([]byte, error) {{
	recorder.Begin()
	out, err := a.applyImpl(body, config, modelInfo)
	recorder.End("applier", map[string]any{{"provider": "{prov}", "body": recorder.Str(body), "config": map[string]any{{"mode": config.Mode.String(), "budget": config.Budget, "level": string(config.Level)}}, "info": recorder.Caps(modelInfo)}}, map[string]any{{"body": recorder.Str(out), "err": recorder.Err(err)}})
	return out, err
}}
'''), ['"github.com/router-for-me/CLIProxyAPI/v8/internal/registry"', '"github.com/router-for-me/CLIProxyAPI/v8/internal/thinking"', IMPORT_REC])

# helps.translatedRequestSummaryConfig
rename('internal/runtime/executor/helps/thinking.go', 'func translatedRequestSummaryConfig(', 'func translatedRequestSummaryConfigImpl(')
write('internal/runtime/executor/helps/zz_record.go', ('helps', r'''
func translatedRequestSummaryConfig(body, currentSourcePayload, originalSourcePayload []byte, model, fromFormat, toFormat string) thinking.SummaryConfig {
	recorder.Begin()
	out := translatedRequestSummaryConfigImpl(body, currentSourcePayload, originalSourcePayload, model, fromFormat, toFormat)
	has := sdktranslator.HasRequestTransformer(sdktranslator.FromString(strings.ToLower(strings.TrimSpace(fromFormat))), sdktranslator.FromString(strings.ToLower(strings.TrimSpace(toFormat))))
	mode := "unspecified"
	switch out.Mode {
	case thinking.SummaryDisabled:
		mode = "disabled"
	case thinking.SummaryEnabled:
		mode = "enabled"
	}
	recorder.End("translated_summary", map[string]any{"body": recorder.Str(body), "current": recorder.Str(currentSourcePayload), "original": recorder.Str(originalSourcePayload), "model": model, "from": fromFormat, "to": toFormat, "has_request_transformer": has}, map[string]any{"mode": mode, "detail": out.Detail})
	return out
}
'''), ['"strings"', '', '"github.com/router-for-me/CLIProxyAPI/v8/internal/thinking"', 'sdktranslator "github.com/router-for-me/CLIProxyAPI/v8/sdk/translator"', IMPORT_REC])

# ---------------- signature ----------------
S = 'internal/signature/'
sig_renames = [
    ('provider_compatibility.go', 'DetectSignatureProviderForBlock'),
    ('provider_compatibility.go', 'DecideSignatureCompatibilityForModel'),
    ('provider_compatibility.go', 'CompatibleAntigravityClaudeThinkingSignature'),
    ('provider_compatibility.go', 'IsRecognizedReasoningSignature'),
    ('provider_compatibility.go', 'SplitSignatureProviderPrefix'),
    ('provider_compatibility.go', 'SignatureProviderFromModelName'),
    ('claude_validation.go', 'InspectClaudeCAISSignature'),
    ('claude_validation.go', 'NormalizeClaudeThinkingSignature'),
    ('claude_validation.go', 'NormalizeClaudeProviderNativeThinkingSignature'),
    ('claude_validation.go', 'InspectClaudeSignaturePayload'),
    ('claude_validation.go', 'InspectClaudeDoubleLayerSignature'),
    ('claude_validation.go', 'InspectClaudeSingleLayerSignature'),
    ('claude_validation.go', 'IsValidClaudeThinkingSignature'),
    ('claude_validation.go', 'HasDecodableClaudeThinkingSignature'),
    ('claude_validation.go', 'ValidateClaudeThinkingSignatures'),
    ('gemini_validation.go', 'InspectGeminiThoughtSignature'),
    ('gemini_validation.go', 'ValidateGeminiThoughtSignatures'),
    ('gemini_validation.go', 'ValidateGeminiFunctionCallPairing'),
    ('gpt_validation.go', 'InspectGPTReasoningSignature'),
    ('grok_validation.go', 'InspectGrokEncryptedContent'),
    ('kimi_validation.go', 'InspectKimiThinkingSignature'),
    ('gemini_sanitize.go', 'SanitizeGeminiRequestThoughtSignatures'),
    ('gemini_sanitize.go', 'GeminiReplaySignatureOrBypass'),
    ('claude_messages_sanitize.go', 'SanitizeClaudeMessagesSignaturesForTarget'),
    ('claude.go', 'StripInvalidClaudeThinkingBlocks'),
    ('claude.go', 'StripInvalidClaudeThinkingBlocksAndEmptyMessages'),
]
for f, name in sig_renames:
    rename(S + f, f'func {name}(', f'func {name[0].lower()}{name[1:]}Impl(')

write(S + 'zz_record.go', ('signature', r'''
func recClaudeOpt(opts []ClaudeSignatureValidationOptions) any {
	o := claudeSignatureValidationOptions(opts)
	return map[string]any{"prefix_only": o.PrefixOnly, "base64_only": o.Base64Only, "allow_empty": o.AllowEmptySignatureWithEmptyText, "strict": o.Strict}
}

func recGeminiOpt(opts []GeminiThoughtSignatureValidationOptions) any {
	o := geminiThoughtSignatureValidationOptions(opts)
	return map[string]any{"allow_bypass_sentinel": o.AllowBypassSentinel, "require_known_envelope": o.RequireKnownEnvelope, "require_observed_marker": o.RequireObservedMarker}
}

func recDecision(d SignatureCompatibilityDecision) any {
	return map[string]any{"target": string(d.TargetProvider), "detected": string(d.DetectedProvider), "block_kind": string(d.BlockKind), "compatible": d.Compatible, "action": string(d.Action), "replacement": d.ReplacementSignature, "normalized": d.NormalizedSignature, "reason": d.Reason}
}

func recTree(t *ClaudeSignatureTree) any {
	if t == nil {
		return nil
	}
	var f2 any
	if t.Field2 != nil {
		f2 = *t.Field2
	}
	return map[string]any{"encoding_layers": t.EncodingLayers, "channel_id": t.ChannelID, "field2": f2, "routing_class": t.RoutingClass, "infrastructure_class": t.InfrastructureClass, "schema_features": t.SchemaFeatures, "model_text": t.ModelText, "legacy_route_hint": t.LegacyRouteHint, "has_field7": t.HasField7}
}

func DetectSignatureProviderForBlock(rawSignature string, blockKind SignatureBlockKind) SignatureProvider {
	recorder.Begin()
	out := detectSignatureProviderForBlockImpl(rawSignature, blockKind)
	recorder.End("detect", map[string]any{"raw": rawSignature, "block_kind": string(blockKind)}, string(out))
	return out
}

func DecideSignatureCompatibilityForModel(targetProvider SignatureProvider, targetModel string, rawSignature string, blockKind SignatureBlockKind) SignatureCompatibilityDecision {
	recorder.Begin()
	out := decideSignatureCompatibilityForModelImpl(targetProvider, targetModel, rawSignature, blockKind)
	recorder.End("decide", map[string]any{"target": string(targetProvider), "model": targetModel, "raw": rawSignature, "block_kind": string(blockKind)}, recDecision(out))
	return out
}

func CompatibleAntigravityClaudeThinkingSignature(rawSignature string) (string, bool) {
	recorder.Begin()
	out, ok := compatibleAntigravityClaudeThinkingSignatureImpl(rawSignature)
	recorder.End("antigravity_claude", rawSignature, map[string]any{"sig": out, "ok": ok})
	return out, ok
}

func IsRecognizedReasoningSignature(rawSignature string) bool {
	recorder.Begin()
	out := isRecognizedReasoningSignatureImpl(rawSignature)
	recorder.End("recognized", rawSignature, out)
	return out
}

func SplitSignatureProviderPrefix(rawSignature string) (SignatureProvider, string, bool) {
	recorder.Begin()
	p, rest, ok := splitSignatureProviderPrefixImpl(rawSignature)
	recorder.End("split_prefix", rawSignature, map[string]any{"provider": string(p), "rest": rest, "ok": ok})
	return p, rest, ok
}

func SignatureProviderFromModelName(modelName string) SignatureProvider {
	recorder.Begin()
	out := signatureProviderFromModelNameImpl(modelName)
	recorder.End("provider_from_model", modelName, string(out))
	return out
}

func InspectClaudeCAISSignature(rawSignature string) (*ClaudeCAISSignatureInfo, error) {
	recorder.Begin()
	info, err := inspectClaudeCAISSignatureImpl(rawSignature)
	var o any
	if info != nil {
		o = map[string]any{"first_byte": info.FirstByte, "envelope_version": info.EnvelopeVersion, "channel_id": info.ChannelID, "model_text": info.ModelText, "block_kind": info.BlockKind, "context_id": info.ContextID, "signature_len": info.SignatureLen}
	}
	recorder.End("inspect_cais", rawSignature, map[string]any{"info": o, "err": recorder.Err(err)})
	return info, err
}

func NormalizeClaudeThinkingSignature(rawSignature string, opts ...ClaudeSignatureValidationOptions) (string, error) {
	recorder.Begin()
	out, err := normalizeClaudeThinkingSignatureImpl(rawSignature, opts...)
	recorder.End("normalize_claude", map[string]any{"raw": rawSignature, "opt": recClaudeOpt(opts)}, map[string]any{"sig": out, "err": recorder.Err(err)})
	return out, err
}

func NormalizeClaudeProviderNativeThinkingSignature(rawSignature string, opts ...ClaudeSignatureValidationOptions) (string, error) {
	recorder.Begin()
	out, err := normalizeClaudeProviderNativeThinkingSignatureImpl(rawSignature, opts...)
	recorder.End("normalize_claude_native", map[string]any{"raw": rawSignature, "opt": recClaudeOpt(opts)}, map[string]any{"sig": recorder.Str([]byte(out)), "err": recorder.Err(err)})
	return out, err
}

func InspectClaudeSignaturePayload(payload []byte, encodingLayers int) (*ClaudeSignatureTree, error) {
	recorder.Begin()
	out, err := inspectClaudeSignaturePayloadImpl(payload, encodingLayers)
	recorder.End("inspect_claude_payload", map[string]any{"payload": base64.StdEncoding.EncodeToString(payload), "layers": encodingLayers}, map[string]any{"tree": recTree(out), "err": recorder.Err(err)})
	return out, err
}

func InspectClaudeDoubleLayerSignature(sig string) (*ClaudeSignatureTree, error) {
	recorder.Begin()
	out, err := inspectClaudeDoubleLayerSignatureImpl(sig)
	recorder.End("inspect_claude_double", sig, map[string]any{"tree": recTree(out), "err": recorder.Err(err)})
	return out, err
}

func InspectClaudeSingleLayerSignature(sig string) (*ClaudeSignatureTree, error) {
	recorder.Begin()
	out, err := inspectClaudeSingleLayerSignatureImpl(sig)
	recorder.End("inspect_claude_single", sig, map[string]any{"tree": recTree(out), "err": recorder.Err(err)})
	return out, err
}

func IsValidClaudeThinkingSignature(rawSignature string, opts ...ClaudeSignatureValidationOptions) bool {
	recorder.Begin()
	out := isValidClaudeThinkingSignatureImpl(rawSignature, opts...)
	recorder.End("valid_claude", map[string]any{"raw": rawSignature, "opt": recClaudeOpt(opts)}, out)
	return out
}

func HasDecodableClaudeThinkingSignature(rawSignature string) bool {
	recorder.Begin()
	out := hasDecodableClaudeThinkingSignatureImpl(rawSignature)
	recorder.End("decodable_claude", rawSignature, out)
	return out
}

func ValidateClaudeThinkingSignatures(inputRawJSON []byte, opts ...ClaudeSignatureValidationOptions) error {
	recorder.Begin()
	err := validateClaudeThinkingSignaturesImpl(inputRawJSON, opts...)
	recorder.End("validate_claude", map[string]any{"body": recorder.Str(inputRawJSON), "opt": recClaudeOpt(opts)}, recorder.Err(err))
	return err
}

func InspectGeminiThoughtSignature(rawSignature string, opts ...GeminiThoughtSignatureValidationOptions) (*GeminiThoughtSignatureInfo, error) {
	recorder.Begin()
	info, err := inspectGeminiThoughtSignatureImpl(rawSignature, opts...)
	var o any
	if info != nil {
		o = map[string]any{"is_bypass_sentinel": info.IsBypassSentinel, "bypass_sentinel": info.BypassSentinel, "decoded_len": info.DecodedLen, "first_byte": info.FirstByte, "has_observed_marker": info.HasObservedMarker, "known_envelope": info.KnownEnvelope, "envelope": string(info.Envelope), "record_count": info.RecordCount, "opaque_payload_len": info.OpaquePayloadLen}
	}
	recorder.End("inspect_gemini", map[string]any{"raw": rawSignature, "opt": recGeminiOpt(opts)}, map[string]any{"info": o, "err": recorder.Err(err)})
	return info, err
}

func ValidateGeminiThoughtSignatures(inputRawJSON []byte, opts ...GeminiThoughtSignatureValidationOptions) error {
	recorder.Begin()
	err := validateGeminiThoughtSignaturesImpl(inputRawJSON, opts...)
	recorder.End("validate_gemini", map[string]any{"body": recorder.Str(inputRawJSON), "opt": recGeminiOpt(opts)}, recorder.Err(err))
	return err
}

func ValidateGeminiFunctionCallPairing(inputRawJSON []byte) error {
	recorder.Begin()
	err := validateGeminiFunctionCallPairingImpl(inputRawJSON)
	recorder.End("validate_pairing", recorder.Str(inputRawJSON), recorder.Err(err))
	return err
}

func InspectGPTReasoningSignature(rawSignature string) (*GPTReasoningSignatureInfo, error) {
	recorder.Begin()
	info, err := inspectGPTReasoningSignatureImpl(rawSignature)
	var o any
	if info != nil {
		o = map[string]any{"decoded_len": info.DecodedLen, "ciphertext_len": info.CiphertextLen}
	}
	recorder.End("inspect_gpt", rawSignature, map[string]any{"info": o, "err": recorder.Err(err)})
	return info, err
}

func InspectGrokEncryptedContent(raw string) (*GrokEncryptedContentInfo, error) {
	recorder.Begin()
	info, err := inspectGrokEncryptedContentImpl(raw)
	var o any
	if info != nil {
		o = map[string]any{"raw_len": info.RawLen, "decoded_len": info.DecodedLen}
	}
	recorder.End("inspect_grok", raw, map[string]any{"info": o, "err": recorder.Err(err)})
	return info, err
}

func InspectKimiThinkingSignature(raw string) (*KimiThinkingSignatureInfo, error) {
	recorder.Begin()
	info, err := inspectKimiThinkingSignatureImpl(raw)
	var o any
	if info != nil {
		o = map[string]any{"raw_len": info.RawLen, "decoded_len": info.DecodedLen, "mode": string(info.Mode)}
	}
	recorder.End("inspect_kimi", raw, map[string]any{"info": o, "err": recorder.Err(err)})
	return info, err
}

func SanitizeGeminiRequestThoughtSignatures(payload []byte, contentsPath string) []byte {
	recorder.Begin()
	out := sanitizeGeminiRequestThoughtSignaturesImpl(payload, contentsPath)
	recorder.End("sanitize_gemini", map[string]any{"body": recorder.Str(payload), "path": contentsPath}, recorder.Str(out))
	return out
}

func GeminiReplaySignatureOrBypass(rawSignature string, blockKind SignatureBlockKind) string {
	recorder.Begin()
	out := geminiReplaySignatureOrBypassImpl(rawSignature, blockKind)
	recorder.End("gemini_replay", map[string]any{"raw": rawSignature, "block_kind": string(blockKind)}, out)
	return out
}

func SanitizeClaudeMessagesSignaturesForTarget(payload []byte, opts ClaudeMessagesSignatureSanitizeOptions) ([]byte, SignatureSanitizeReport) {
	recorder.Begin()
	out, report := sanitizeClaudeMessagesSignaturesForTargetImpl(payload, opts)
	decisions := []any{}
	for _, d := range report.Decisions {
		decisions = append(decisions, recDecision(d))
	}
	recorder.End("sanitize_claude_messages", map[string]any{"body": recorder.Str(payload), "target": string(opts.TargetProvider), "model": opts.TargetModel, "drop_empty_messages": opts.DropEmptyMessages, "drop_tool_signatures": opts.DropToolSignatures, "drop_empty_thinking_placeholders": opts.DropEmptyThinkingPlaceholders, "preserve_empty_thinking_blocks": opts.PreserveEmptyThinkingBlocks},
		map[string]any{"body": recorder.Str(out), "target": string(report.TargetProvider), "preserved": report.Preserved, "dropped_blocks": report.DroppedBlocks, "dropped_signatures": report.DroppedSignatures, "replaced_signatures": report.ReplacedSignatures, "decisions": decisions})
	return out, report
}

func StripInvalidClaudeThinkingBlocks(payload []byte, opts ...ClaudeSignatureValidationOptions) []byte {
	recorder.Begin()
	out := stripInvalidClaudeThinkingBlocksImpl(payload, opts...)
	recorder.End("strip_claude", map[string]any{"body": recorder.Str(payload), "opt": recClaudeOpt(opts)}, recorder.Str(out))
	return out
}

func StripInvalidClaudeThinkingBlocksAndEmptyMessages(payload []byte, opts ...ClaudeSignatureValidationOptions) []byte {
	recorder.Begin()
	out := stripInvalidClaudeThinkingBlocksAndEmptyMessagesImpl(payload, opts...)
	recorder.End("strip_claude_empty", map[string]any{"body": recorder.Str(payload), "opt": recClaudeOpt(opts)}, recorder.Str(out))
	return out
}
'''), ['"encoding/base64"', '', IMPORT_REC])
print("patched")
