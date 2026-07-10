//! Emit Rust-authored dashboard JSON Schemas for the frontend generator.

use llmconduit::dashboard_contracts::{declarations_schema, root_schemas};
use std::path::{Path, PathBuf};

fn main() {
    let output = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            eprintln!("usage: dashboard-contracts <output-directory>");
            std::process::exit(2);
        });
    if let Err(error) = emit(&output) {
        eprintln!("dashboard contract generation failed: {error}");
        std::process::exit(1);
    }
}

fn emit(output: &Path) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(output)?;
    write_json(output.join("contracts.schema.json"), &declarations_schema())?;
    for (name, schema) in root_schemas() {
        write_json(output.join(format!("{name}.schema.json")), &schema)?;
    }
    Ok(())
}

fn write_json(
    path: PathBuf,
    value: &impl serde::Serialize,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    std::fs::write(path, bytes)?;
    Ok(())
}
