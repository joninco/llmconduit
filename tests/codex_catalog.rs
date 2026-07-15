use std::fs;
use std::path::PathBuf;
use std::process::Command;

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "llmconduit-codex-catalog-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&path).expect("create isolated CODEX_HOME");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn codex_catalog_example_decodes_with_current_schema() {
    let codex = std::env::var_os("CODEX_BIN").unwrap_or_else(|| "codex".into());
    let version = match Command::new(&codex).arg("--version").output() {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("skipping Codex catalog schema check: Codex CLI is not installed");
            return;
        }
        Err(error) => panic!("failed to inspect Codex CLI: {error}"),
    };
    assert!(
        version.status.success(),
        "Codex CLI version check failed: {}",
        String::from_utf8_lossy(&version.stderr)
    );

    let codex_home = TestDirectory::new();
    let catalog = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("docs/examples/codex-model-catalog.glm-5.2-nvfp4.json");
    let catalog_json: serde_json::Value =
        serde_json::from_slice(&fs::read(&catalog).expect("read checked Codex model catalog"))
            .expect("parse checked Codex model catalog");
    let model = &catalog_json["models"][0];
    assert_eq!(model["apply_patch_tool_type"], "freeform");
    assert_eq!(model["experimental_supported_tools"], serde_json::json!([]));
    assert_eq!(model["supports_search_tool"], true);
    assert_eq!(model["multi_agent_version"], "v2");
    assert_eq!(model["supports_parallel_tool_calls"], false);

    let catalog_value = serde_json::to_string(catalog.to_string_lossy().as_ref())
        .expect("serialize catalog path as TOML-compatible string");

    let output = Command::new(codex)
        .args(["features", "list"])
        .arg("-c")
        .arg("model=\"GLM-5.2-NVFP4\"")
        .arg("-c")
        .arg("model_provider=\"llmconduit\"")
        .arg("-c")
        .arg(format!("model_catalog_json={catalog_value}"))
        .arg("-c")
        .arg("model_providers.llmconduit.name=\"Local llmconduit\"")
        .arg("-c")
        .arg("model_providers.llmconduit.base_url=\"http://127.0.0.1:9/v1\"")
        .arg("-c")
        .arg("model_providers.llmconduit.env_key=\"LLMCONDUIT_API_TOKEN\"")
        .arg("-c")
        .arg("model_providers.llmconduit.wire_api=\"responses\"")
        .arg("-c")
        .arg("model_providers.llmconduit.requires_openai_auth=false")
        .env("CODEX_HOME", &codex_home.0)
        .env("LLMCONDUIT_API_TOKEN", "catalog-schema-check-only")
        .env_remove("OPENAI_API_KEY")
        .env_remove("CHATGPT_API_KEY")
        .output()
        .expect("run Codex catalog schema check");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "Codex rejected the checked model catalog\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let diagnostics = format!("{stdout}\n{stderr}").to_ascii_lowercase();
    for warning in [
        "missing field `models`",
        "missing field models",
        "failed to load model catalog",
        "failed to parse model catalog",
        "model metadata for `glm-5.2-nvfp4` not found",
    ] {
        assert!(
            !diagnostics.contains(warning),
            "Codex emitted a model-catalog warning: {warning}\n{diagnostics}"
        );
    }
}
