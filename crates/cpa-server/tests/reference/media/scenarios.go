package main

import "strings"

const formBoundary = "XFORMBOUNDARYX"

// part is one multipart form part; a file part has a filename.
type part struct {
	name, filename, contentType, body string
}

// form renders a multipart/form-data body with the fixed client boundary.
func form(parts ...part) string {
	var b strings.Builder
	for _, p := range parts {
		b.WriteString("--" + formBoundary + "\r\n")
		if p.filename != "" {
			b.WriteString(`Content-Disposition: form-data; name="` + p.name + `"; filename="` + p.filename + "\"\r\n")
		} else {
			b.WriteString(`Content-Disposition: form-data; name="` + p.name + "\"\r\n")
		}
		if p.contentType != "" {
			b.WriteString("Content-Type: " + p.contentType + "\r\n")
		}
		b.WriteString("\r\n" + p.body + "\r\n")
	}
	b.WriteString("--" + formBoundary + "--\r\n")
	return b.String()
}

const (
	multipartType = "multipart/form-data; boundary=" + formBoundary
	jsonType      = "application/json"
	pngBytes      = "\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR"
	gifBytes      = "GIF89a\x01\x00\x01\x00"
)

func ok(body string) upstream {
	return upstream{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: body}
}

func sse(body string) upstream {
	return upstream{Status: 200, Headers: [][2]string{{"Content-Type", "text/event-stream"}}, Body: body}
}

func status(code int, body string) upstream {
	return upstream{Status: code, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: body}
}

// slow delays a reply past the 1s keep-alive intervals of the shared config.
func slow(u upstream) upstream {
	u.DelayMs = 1400
	return u
}

func post(name, path, contentType, body string, replies ...upstream) scenario {
	return scenario{Name: name, Method: "POST", Path: path, ContentType: contentType, Body: body, Upstreams: replies}
}

func get(name, path string, replies ...upstream) scenario {
	return scenario{Name: name, Method: "GET", Path: path, Upstreams: replies}
}

func scenarios() []scenario {
	const gen = "/v1/images/generations"
	const edits = "/v1/images/edits"
	const create = "/openai/v1/videos"
	twoImages := ok(`{"created":1700000001,"data":[{"b64_json":"QUJD","revised_prompt":" a cat, refined "},{"url":"http://UPSTREAM/img/2.png","mime_type":"image/webp"}],"usage":{"total_tokens":5,"input_tokens":2}}`)
	return []scenario{
		// --- image generations ---
		post("gen_xai_b64", gen, jsonType,
			`{"model":"grok-imagine-image","prompt":"  a cat  ","size":"1792x1024","n":2,"quality":" high ","user":"u1"}`,
			twoImages),
		post("gen_xai_url_format", gen, jsonType,
			`{"model":"xai/grok-imagine-image-quality","prompt":"a dog","response_format":"URL","aspect_ratio":"portrait","resolution":"2K"}`,
			ok(`{"data":[{"b64_json":"QUJD","output_format":"jpeg"},{"b64_json":"","url":""},{"url":"http://UPSTREAM/img/3.png"}]}`)),
		post("gen_xai_stream", gen, jsonType,
			`{"model":"grok/grok-imagine-image-2.0","prompt":"a bird","stream":true,"size":"2048x2048"}`,
			twoImages),
		post("gen_xai_stream_url", gen, jsonType,
			`{"model":"grok-imagine-image","prompt":"a bird","stream":true,"response_format":"url"}`,
			ok(`{"created":1700000003,"data":[{"b64_json":"QUJD","mime_type":"png"}]}`)),
		post("gen_xai_no_output", gen, jsonType,
			`{"model":"grok-imagine-image","prompt":"empty"}`,
			ok(`{"created":1700000004,"data":[{"revised_prompt":"x"}]}`)),
		post("gen_xai_invalid_upstream_json", gen, jsonType,
			`{"model":"grok-imagine-image","prompt":"broken","stream":true}`,
			ok(`{"data":[`)),
		post("gen_compat", gen, jsonType,
			`{"model":"img","prompt":"a lake","n":1,"style":"vivid","stream":false}`,
			ok(`{"created":1700000005,"data":[{"b64_json":"WFla","revised_prompt":"lake"}],"usage":{"total_tokens":3}}`)),
		post("gen_compat_url", gen, jsonType,
			`{"model":"img","prompt":"a lake","response_format":"url"}`,
			ok(`{"created":1700000006,"data":[{"url":"http://UPSTREAM/img/lake.png"}]}`)),
		post("gen_compat_stream", gen, jsonType,
			`{"model":"img","prompt":"a lake","stream":true}`,
			sse("event: image_generation.partial_image\ndata: {\"type\":\"image_generation.partial_image\",\"b64_json\":\"AA\"}\n\nevent: image_generation.completed\ndata: {\"type\":\"image_generation.completed\",\"b64_json\":\"QUJD\"}\n\n")),
		post("gen_compat_upstream_error", gen, jsonType,
			`{"model":"img","prompt":"bad"}`,
			status(400, `{"error":{"message":"prompt rejected","type":"invalid_request_error"}}`)),
		post("gen_unsupported_model", gen, jsonType, `{"model":"dall-e-3","prompt":"x"}`),
		post("gen_chat_model_rejected", gen, jsonType, `{"model":"chat","prompt":"x"}`),
		post("gen_missing_prompt", gen, jsonType, `{"model":"grok-imagine-image","prompt":"   "}`),
		post("gen_invalid_json", gen, jsonType, `{"model":`),
		post("gen_default_model_unserved", gen, jsonType, `{"prompt":"x"}`),

		// --- image edits ---
		post("edit_json_xai_two_images", edits, jsonType,
			`{"model":"grok-imagine-image","prompt":"make it blue","image":{"image_url":{"url":" https://example.com/a.png "}},"images":["https://example.com/b.png",{"url":"https://example.com/c.png"}],"size":"2048x2048","n":"3"}`,
			twoImages),
		post("edit_json_xai_one_image", edits, jsonType+"; charset=utf-8",
			`{"model":"grok-imagine-image","prompt":"crop","image":"https://example.com/a.png","response_format":"url","aspect_ratio":"20:9"}`,
			ok(`{"created":1700000007,"data":[{"url":"http://UPSTREAM/img/e.png"}]}`)),
		post("edit_json_xai_stream", edits, jsonType,
			`{"model":"grok-imagine-image","prompt":"crop","image":"https://example.com/a.png","stream":true}`,
			ok(`{"created":1700000008,"data":[{"b64_json":"RUZH"}]}`)),
		post("edit_json_xai_missing_image", edits, jsonType,
			`{"model":"grok-imagine-image","prompt":"crop","images":[{"image_url":""}]}`),
		post("edit_json_compat", edits, jsonType,
			`{"model":"img","prompt":"recolor","image":"https://example.com/a.png","stream":false}`,
			ok(`{"created":1700000009,"data":[{"b64_json":"Q09N"}]}`)),
		post("edit_multipart_xai", edits, multipartType,
			form(
				part{name: "model", body: "grok-imagine-image"},
				part{name: "prompt", body: " add a hat "},
				part{name: "image[]", filename: "a.png", contentType: "image/png", body: "PNGDATA"},
				part{name: "image[]", filename: "b.gif", body: gifBytes},
				part{name: "image", filename: "ignored.png", contentType: "image/png", body: "IGNORED"},
				part{name: "size", body: "1024x1536"},
				part{name: "n", body: "2"},
				part{name: "quality", body: "low"},
			),
			twoImages),
		post("edit_multipart_xai_stream", edits, multipartType,
			form(
				part{name: "model", body: "grok-imagine-image"},
				part{name: "prompt", body: "hat"},
				part{name: "image", filename: "a.png", body: pngBytes},
				part{name: "stream", body: "YES"},
				part{name: "n", body: "x"},
			),
			ok(`{"created":1700000010,"data":[{"b64_json":"SEFU"}]}`)),
		post("edit_multipart_compat", edits, multipartType,
			form(
				part{name: "model", body: "img"},
				part{name: "prompt", body: "hat"},
				part{name: "image", filename: "a.png", contentType: "image/png", body: "PNGDATA"},
				part{name: "size", body: "1024x1024"},
				part{name: "stream", body: "false"},
			),
			ok(`{"created":1700000011,"data":[{"b64_json":"TVBD"}]}`)),
		post("edit_multipart_compat_stream", edits, multipartType,
			form(
				part{name: "model", body: "img"},
				part{name: "prompt", body: "hat"},
				part{name: "image", filename: "a.png", contentType: "image/png", body: "PNGDATA"},
				part{name: "stream", body: "1"},
			),
			sse("event: image_edit.completed\ndata: {\"type\":\"image_edit.completed\",\"b64_json\":\"TVBD\"}\n\n")),
		post("edit_multipart_missing_image", edits, multipartType,
			form(part{name: "model", body: "grok-imagine-image"}, part{name: "prompt", body: "hat"})),
		post("edit_multipart_unsupported_model", edits, multipartType,
			form(part{name: "model", body: "dall-e-2"}, part{name: "prompt", body: "hat"})),
		post("edit_multipart_no_boundary", edits, "multipart/form-data", "x"),
		post("edit_bad_content_type", edits, "Text/Plain; charset=utf-8", "hello"),
		post("edit_no_content_type", edits, "", "hello"),

		// --- OpenAI Videos API ---
		post("video_create_json", create, jsonType,
			`{"model":"sora-2","prompt":" waves ","seconds":"20","size":"1280x720","input_reference":{"image_url":"https://example.com/i.png"}}`,
			ok(`{"request_id":"req-vid-1"}`)),
		get("video_retrieve_pinned", create+"/req-vid-1",
			ok(`{"status":"done","progress":100,"model":"grok-imagine-video","video":{"url":"http://UPSTREAM/files/v.mp4","duration":8},"created_at":1700000100}`)),
		get("video_retrieve_pinned_again", create+"/req-vid-1",
			ok(`{"status":"processing","progress":40}`)),
		get("video_content", create+"/req-vid-1/content",
			ok(`{"status":"done","video":{"url":"http://UPSTREAM/files/v.mp4"}}`),
			upstream{Status: 200, Headers: [][2]string{{"Content-Type", "video/mp4"}, {"Content-Disposition", `attachment; filename="v.mp4"`}, {"ETag", `"abc"`}, {"X-Other", "dropped"}}, Body: "MP4DATA"}),
		get("video_content_download_404", create+"/req-vid-1/content",
			ok(`{"status":"done","video":{"url":"http://UPSTREAM/files/missing.mp4"}}`),
			upstream{Status: 404, Body: ""}),
		get("video_content_download_error_body", create+"/req-vid-1/content",
			ok(`{"status":"done","video":{"url":"http://UPSTREAM/files/gone.mp4"}}`),
			upstream{Status: 410, Body: "  expired  "}),
		get("video_content_connect_refused", create+"/req-vid-1/content",
			ok(`{"status":"done","video":{"url":"http://127.0.0.1:9/refused.mp4"}}`)),
		get("video_content_no_url", create+"/req-vid-1/content",
			ok(`{"status":"processing"}`)),
		get("video_content_invalid_url", create+"/req-vid-1/content",
			ok(`{"status":"done","video":{"url":"ftp://example.com/v.mp4"}}`)),
		get("video_content_variant", create+"/req-vid-1/content?variant=thumbnail"),
		post("video_create_form_urlencoded", create, "application/x-www-form-urlencoded",
			"model=grok-imagine-video-1.5-preview&prompt=hi+there&aspect_ratio=landscape&resolution=480P&reference_image_urls=https%3A%2F%2Fa%2F1.png%2C+https%3A%2F%2Fa%2F2.png",
			ok(`{"request_id":"req-vid-2","status":"pending","progress":5}`)),
		get("video_retrieve_preview_model", create+"/req-vid-2",
			ok(`{"status":"failed","error":{"message":"moderated","code":"content_policy"},"code":"outer"}`)),
		post("video_create_multipart", create, multipartType,
			form(
				part{name: "model", body: "grok-imagine-video"},
				part{name: "prompt", body: "rain"},
				part{name: "seconds", body: "0"},
				part{name: "size", body: "1024x1792"},
				part{name: "input_reference[image_url]", body: "https://example.com/r.png"},
			),
			ok(`{"id":"req-vid-3","status":"in_progress"}`)),
		get("video_retrieve_pinned_to_creator", create+"/req-vid-3",
			ok(`{"status":"queued"}`)),
		get("video_retrieve_pinned_to_creator_again", create+"/req-vid-3",
			ok(`{"status":"queued"}`)),
		post("video_create_bad_size", create, jsonType, `{"model":"sora-2","prompt":"x","size":"640x480"}`),
		post("video_create_bad_seconds", create, jsonType, `{"prompt":"x","seconds":"1.5"}`),
		post("video_create_file_id", create, jsonType, `{"prompt":"x","input_reference":{"file_id":"file-1"}}`),
		post("video_create_unsupported_model", create, jsonType, `{"model":"veo-3","prompt":"x"}`),
		post("video_create_invalid_json", create, jsonType, `{"prompt":`),
		post("video_create_missing_id", create, jsonType, `{"prompt":"x"}`, ok(`{"status":"queued"}`)),
		get("video_retrieve_unbound", create+"/req-unknown",
			ok(`{"status":"queued"}`)),

		// --- xAI-native videos ---
		post("native_generations", "/v1/videos/generations", jsonType,
			`{"model":"x-ai/grok-imagine-video-1.5","prompt":"p","duration":5}`,
			ok(`{"request_id":"req-native-1"}`)),
		get("native_retrieve_pinned", "/v1/videos/req-native-1",
			ok(`{"status":"done","video":{"url":"http://UPSTREAM/files/n.mp4"}}`)),
		post("native_videos_root", "/v1/videos", jsonType,
			`{"prompt":"p"}`,
			ok(`{"request_id":"req-native-2"}`)),
		post("native_edits", "/v1/videos/edits", jsonType,
			`{"model":"grok-imagine-video","prompt":"p","video":{"url":"https://example.com/v.mp4"}}`,
			ok(`{"request_id":"req-native-3"}`)),
		post("native_extensions", "/v1/videos/extensions", jsonType,
			`{"model":"grok-imagine-video","prompt":"p","video_id":"req-native-1"}`,
			ok(`{"request_id":"req-native-4"}`)),
		post("native_unsupported_model", "/v1/videos", jsonType, `{"model":"sora-2","prompt":"p"}`),
		post("native_invalid_json", "/v1/videos/generations", jsonType, `[`),
		get("native_retrieve_upstream_error", "/v1/videos/req-native-2",
			status(404, `{"error":"not found"}`)),

		// --- GET /v1/models for Grok Shell (no upstream call) ---
		{Name: "models_grok_shell", Method: "GET", Path: "/v1/models",
			Headers: map[string]string{"User-Agent": "grok-pager/0.2.119 grok-shell/0.2.119 (macos; aarch64)"}},
		{Name: "models_grok_shell_case", Method: "GET", Path: "/v1/models?client_version=1.0",
			Headers: map[string]string{"User-Agent": "Grok-Shell/1.0"}},

		// --- keep-alives: the reply arrives after one 1s interval ---
		post("slow_gen_xai", gen, jsonType,
			`{"model":"grok-imagine-image","prompt":"slow"}`,
			slow(ok(`{"created":1700000020,"data":[{"b64_json":"U0xX"}]}`))),
		post("slow_gen_xai_stream", gen, jsonType,
			`{"model":"grok-imagine-image","prompt":"slow","stream":true}`,
			slow(ok(`{"created":1700000021,"data":[{"b64_json":"U0xX"}]}`))),
		post("slow_gen_xai_stream_error", gen, jsonType,
			`{"model":"grok-imagine-image","prompt":"slow","stream":true}`,
			slow(status(400, `{"error":{"message":"bad prompt","type":"invalid_request_error"}}`))),
		post("slow_gen_xai_stream_no_output", gen, jsonType,
			`{"model":"grok-imagine-image","prompt":"slow","stream":true}`,
			slow(ok(`{"data":[]}`))),
		post("slow_gen_compat_error", gen, jsonType,
			`{"model":"img","prompt":"slow"}`,
			slow(status(400, `{"error":{"message":"nope","type":"invalid_request_error"}}`))),
		post("slow_gen_compat_stream", gen, jsonType,
			`{"model":"img","prompt":"slow","stream":true}`,
			slow(sse("event: image_generation.completed\ndata: {\"type\":\"image_generation.completed\",\"b64_json\":\"U0xX\"}\n\n"))),
		post("slow_gen_compat_stream_error", gen, jsonType,
			`{"model":"img","prompt":"slow","stream":true}`,
			slow(status(400, `{"error":{"message":"nope","type":"invalid_request_error"}}`))),
		post("gen_compat_stream_empty", gen, jsonType,
			`{"model":"img","prompt":"empty","stream":true}`,
			sse("")),
		post("slow_video_create", create, jsonType,
			`{"prompt":"slow"}`,
			slow(ok(`{"request_id":"req-slow-1"}`))),
		get("slow_video_content_download", create+"/req-slow-1/content",
			ok(`{"status":"done","video":{"url":"http://UPSTREAM/files/slow.mp4"}}`),
			slow(upstream{Status: 200, Headers: [][2]string{{"Content-Type", "video/mp4"}}, Body: "SLOWMP4"})),
		get("slow_native_retrieve", "/v1/videos/req-slow-1",
			slow(ok(`{"status":"queued"}`))),

		// --- rate limits last: they cool the model down ---
		post("gen_xai_429", gen, jsonType,
			`{"model":"grok-imagine-image-quality","prompt":"busy"}`,
			status(429, `{"error":{"message":"slow down","type":"rate_limit"}}`),
			status(429, `{"error":{"message":"slow down","type":"rate_limit"}}`),
			status(429, `{"error":{"message":"slow down","type":"rate_limit"}}`)),
		post("gen_xai_after_429", gen, jsonType,
			`{"model":"grok-imagine-image-quality","prompt":"busy","stream":true}`,
			twoImages),
	}
}
