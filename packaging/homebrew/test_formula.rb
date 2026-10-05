require "formula"
require "formulary"
require "tmpdir"
require "yaml"

formula = Formulary.factory(ARGV.fetch(0))
expected_command = [formula.opt_bin/"cliproxy", "--config", formula.etc/"cliproxy-rs/config.yaml"].map(&:to_s)
raise "service does not pass config" unless formula.service.run == expected_command
raise "background service type" unless formula.service.run_type == :immediate

Dir.mktmpdir("cliproxy-homebrew-test") do |directory|
  formula.define_singleton_method(:etc) { Pathname(directory) }
  config_dir = Pathname(directory)/"cliproxy-rs"
  config = config_dir/"config.yaml"
  keys_file = config_dir/"keys.env"

  formula.post_install
  settings = YAML.safe_load(config.read)
  keys = keys_file.read.lines.to_h { |line| line.strip.split("=", 2) }
  raise "not localhost-only" unless settings.fetch("server") == { "host" => "127.0.0.1", "port" => 8317 }
  raise "remote management enabled" unless settings.fetch("management").fetch("allow-remote") == false
  client_key = keys.fetch("CLIPROXY_CLIENT_KEY")
  management_key = keys.fetch("CLIPROXY_MANAGEMENT_KEY")
  raise "invalid client key" unless client_key.match?(/\A[0-9a-f]{48}\z/)
  raise "invalid management key" unless management_key.match?(/\A[0-9a-f]{48}\z/)
  raise "keys match" if client_key == management_key
  raise "client key mismatch" unless settings.fetch("access").fetch("api-keys") == [client_key]
  raise "management key mismatch" unless settings.fetch("management").fetch("secret-key") == management_key
  raise "wrong auth directory" unless settings.fetch("oauth").fetch("auth-dir") == "#{config_dir}/auth"
  raise "missing private auth directory" unless (config_dir/"auth").directory?
  raise "public config directory" unless (config_dir.stat.mode & 0777) == 0700
  [config, keys_file].each do |file|
    raise "public #{file.basename}" unless (file.stat.mode & 0777) == 0600
  end

  original_keys = "\n  # Keep these existing keys.\n\n#{keys_file.read}\n \t\n"
  keys_file.unlink
  keys_file.write original_keys
  config.unlink
  formula.post_install
  raise "existing keys replaced" unless keys_file.read == original_keys
  restored = YAML.safe_load(config.read)
  raise "existing keys not reused" unless restored.fetch("management").fetch("secret-key") == management_key

  config.unlink
  config.write "user-owned config\n"
  formula.post_install
  raise "existing config replaced" unless config.read == "user-owned config\n"
  raise "existing keys changed" unless keys_file.read == original_keys
  keys_file.unlink
  formula.post_install
  raise "keys generated for existing config" if keys_file.exist?
end
puts "Formula: explicit service config, immediate service type, private config, key reuse and preservation passed"

workflow = YAML.safe_load_file(".github/workflows/release.yml")
raise "missing published event" unless workflow.fetch("on", workflow[true]).fetch("release").fetch("types") == ["published"]
jobs = workflow.fetch("jobs")
%w[dashboard test build docker].each do |job|
  raise "#{job} runs on publication" unless jobs.fetch(job).fetch("if") == "github.event_name != 'release'"
end
raise "draft job runs on publication" unless jobs.fetch("release").fetch("if").start_with?("github.event_name == 'push' &&")
homebrew = jobs.fetch("homebrew")
raise "tap job has wrong event" unless homebrew.fetch("if") == "github.event_name == 'release' && !github.event.release.prerelease"
token_step, *guarded_steps = homebrew.fetch("steps")
guarded_steps.each do |step|
  raise "tap step runs without token" unless step.fetch("if") == "steps.token.outputs.enabled == 'true'"
  raise "mutable action in tap job" if step["uses"] && !step["uses"].match?(/@[0-9a-f]{40}\z/)
end
formula_workflow = YAML.safe_load_file(".github/workflows/homebrew.yml")
checkout = formula_workflow.fetch("jobs").fetch("formula").fetch("steps").first.fetch("uses")
raise "mutable formula checkout" unless checkout.match?(/\Aactions\/checkout@[0-9a-f]{40}\z/)
Dir.mktmpdir("cliproxy-release-test") do |directory|
  ["", "test-token"].each do |token|
    output = Pathname(directory)/"output"
    output.unlink if output.exist?
    raise "token check failed" unless system({ "TAP_TOKEN" => token, "GITHUB_OUTPUT" => output.to_s },
                                            "bash", "-e", "-c", token_step.fetch("run"))
    expected = token.empty? ? "" : "enabled=true\n"
    raise "wrong token gate" unless (output.exist? ? output.read : "") == expected
  end
end
puts "Release workflow: build guards, stable publication and absent/present token gates passed"
