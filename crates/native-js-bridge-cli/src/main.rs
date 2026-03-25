//! native-js-bridge — generate npm distribution files for native Rust CLI binaries.
//!
//! Reads `[package.metadata.native-js-bridge]` from Cargo.toml and generates:
//! - `{root}/package.json`
//! - `{root}/bin/{binary}.js`
//! - `{root}/npm/{binary}-{os}-{cpu}/package.json` (one per platform)
//!
//! ```toml
//! [package.metadata.native-js-bridge]
//! scope  = "vikr01"
//! binary = "forge"
//!
//! [[package.metadata.native-js-bridge.platforms]]
//! os      = "darwin"
//! cpu     = "arm64"
//! target  = "aarch64-apple-darwin"
//! bin_ext = ""
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use serde::Deserialize;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "native-js-bridge",
    about = "Generate npm distribution files for native Rust CLI binaries"
)]
struct Cli {
    /// Override the version from Cargo.toml (e.g. CI passes the git tag).
    #[arg(long)]
    version: Option<String>,

    /// Path to the crate root (default: current directory).
    #[arg(long, default_value = ".")]
    root: PathBuf,
}

// ---------------------------------------------------------------------------
// Cargo.toml structs
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CargoToml {
    package: CargoPackage,
}

#[derive(Deserialize)]
struct CargoPackage {
    #[allow(dead_code)]
    name: String,
    version: Option<String>,
    description: Option<String>,
    license: Option<String>,
    metadata: Option<PackageMetadata>,
}

#[derive(Deserialize)]
struct PackageMetadata {
    #[serde(rename = "native-js-bridge")]
    native_js_bridge: Option<NativeJsMeta>,
}

#[derive(Deserialize)]
struct NativeJsMeta {
    scope: String,
    binary: String,
    platforms: Vec<PlatformEntry>,
    /// Arbitrary key/value pairs merged into the root `package.json` after the
    /// generated fields. Any key may be set; later entries override earlier ones.
    ///
    /// ```toml
    /// [package.metadata.native-js-bridge.package_json]
    /// engines    = { node = ">=18" }
    /// keywords   = ["cli", "pc-build"]
    /// repository = "https://github.com/vikr01/pc-build"
    /// ```
    #[serde(default)]
    package_json: toml::Table,
}

#[derive(Deserialize)]
struct PlatformEntry {
    os: String,
    cpu: String,
    /// Cross-compilation target triple — used by CI, not by the generator itself.
    #[allow(dead_code)]
    target: String,
    bin_ext: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn platform_pkg_name(scope: &str, binary: &str, os: &str, cpu: &str) -> String {
    format!("@{scope}/{binary}-{os}-{cpu}")
}

// ---------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------

fn generate_root_package_json(
    root: &Path,
    scope: &str,
    binary: &str,
    version: &str,
    description: Option<&str>,
    license: Option<&str>,
    platforms: &[PlatformEntry],
    extra: &toml::Table,
) -> Result<()> {
    let mut pkg = serde_json::Map::new();
    pkg.insert(
        "name".to_string(),
        serde_json::json!(format!("@{scope}/{binary}")),
    );
    pkg.insert("version".to_string(), serde_json::json!(version));
    if let Some(desc) = description {
        if !desc.is_empty() {
            pkg.insert("description".to_string(), serde_json::json!(desc));
        }
    }
    if let Some(lic) = license {
        if !lic.is_empty() {
            pkg.insert("license".to_string(), serde_json::json!(lic));
        }
    }
    pkg.insert(
        "bin".to_string(),
        serde_json::json!({ binary: format!("bin/{binary}.js") }),
    );

    // optionalDependencies: exact version, same as the package being published.
    // This is the pattern used by esbuild, turbo, and other native binary packages.
    // workspace:* is NOT in the npm spec and only works with pnpm/yarn berry.
    let mut optional_deps = serde_json::Map::new();
    for p in platforms {
        let name = platform_pkg_name(scope, binary, &p.os, &p.cpu);
        optional_deps.insert(name, serde_json::json!(version));
    }
    pkg.insert(
        "optionalDependencies".to_string(),
        serde_json::Value::Object(optional_deps),
    );

    let files: Vec<serde_json::Value> = vec![
        serde_json::json!(format!("bin/{binary}.js")),
        serde_json::json!("package.json"),
    ];
    pkg.insert("files".to_string(), serde_json::Value::Array(files));

    // Merge extra fields last — user values override generated ones.
    for (key, val) in extra {
        let json_val = serde_json::to_value(val)
            .with_context(|| format!("package_json.{key}: cannot convert TOML value to JSON"))?;
        pkg.insert(key.clone(), json_val);
    }

    let json = serde_json::to_string_pretty(&serde_json::Value::Object(pkg))
        .context("Failed to serialize root package.json")?;
    std::fs::write(root.join("package.json"), format!("{json}\n"))
        .context("Failed to write package.json")?;
    Ok(())
}

fn generate_bin_shim(
    root: &Path,
    scope: &str,
    binary: &str,
    platforms: &[PlatformEntry],
) -> Result<()> {
    // Build tab-indented PACKAGES entries to replace the placeholder line.
    let mut packages_entries = String::new();
    for p in platforms {
        let key = format!("{}-{}", p.os, p.cpu);
        let name = platform_pkg_name(scope, binary, &p.os, &p.cpu);
        packages_entries.push_str(&format!(
            "\t{key:?}: {{ name: {name:?}, binExt: {bin_ext:?} }},\n",
            key = key,
            name = name,
            bin_ext = p.bin_ext,
        ));
    }

    const TEMPLATE: &str = include_str!("shim_template.js");
    let shim = TEMPLATE
        .replace("\t// __NATIVE_JS_BRIDGE_PACKAGES__\n", &packages_entries)
        .replace("__BINARY__", binary);

    let bin_dir = root.join("bin");
    std::fs::create_dir_all(&bin_dir).context("Failed to create bin/")?;
    let shim_path = bin_dir.join(format!("{binary}.js"));
    std::fs::write(&shim_path, shim).context("Failed to write bin shim")?;

    // chmod +x on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&shim_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&shim_path, perms)?;
    }

    Ok(())
}

fn generate_platform_package_json(
    root: &Path,
    scope: &str,
    binary: &str,
    version: &str,
    p: &PlatformEntry,
) -> Result<()> {
    let name = platform_pkg_name(scope, binary, &p.os, &p.cpu);
    let dir = root
        .join("npm")
        .join(format!("{binary}-{}-{}", p.os, p.cpu));

    if !dir.exists() {
        anyhow::bail!("Platform directory does not exist: {}", dir.display());
    }

    let bin_filename = format!("{}{}", binary, p.bin_ext);
    let mut pkg = serde_json::Map::new();
    pkg.insert("name".to_string(), serde_json::json!(name));
    pkg.insert("version".to_string(), serde_json::json!(version));
    pkg.insert("os".to_string(), serde_json::json!([&p.os]));
    pkg.insert("cpu".to_string(), serde_json::json!([&p.cpu]));
    pkg.insert(
        "files".to_string(),
        serde_json::json!(["package.json", bin_filename]),
    );

    let json = serde_json::to_string_pretty(&serde_json::Value::Object(pkg))
        .context("Failed to serialize platform package.json")?;
    std::fs::write(dir.join("package.json"), format!("{json}\n"))
        .with_context(|| format!("Failed to write package.json for {name}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = cli
        .root
        .canonicalize()
        .context("Failed to resolve --root")?;

    js_bridge_core::logger::init();
    let log = js_bridge_core::logger::Logger::new("native-js-bridge");

    // Parse Cargo.toml.
    let cargo_toml_path = root.join("Cargo.toml");
    let raw = std::fs::read_to_string(&cargo_toml_path)
        .with_context(|| format!("Failed to read {}", cargo_toml_path.display()))?;
    let cargo: CargoToml = toml::from_str(&raw)
        .with_context(|| format!("Failed to parse {}", cargo_toml_path.display()))?;

    let meta = cargo
        .package
        .metadata
        .as_ref()
        .and_then(|m| m.native_js_bridge.as_ref())
        .with_context(|| {
            format!(
                "Missing [package.metadata.native-js-bridge] in {}",
                cargo_toml_path.display()
            )
        })?;

    let version = cli
        .version
        .or(cargo.package.version)
        .unwrap_or_else(|| "0.0.0".to_string());

    let description = cargo.package.description.as_deref();
    let license = cargo.package.license.as_deref();

    log.step(&format!(
        "Generating npm files for @{}/{}@{}",
        meta.scope, meta.binary, version
    ));

    generate_root_package_json(
        &root,
        &meta.scope,
        &meta.binary,
        &version,
        description,
        license,
        &meta.platforms,
        &meta.package_json,
    )
    .context("root package.json")?;
    log.step("  wrote package.json");

    generate_bin_shim(&root, &meta.scope, &meta.binary, &meta.platforms).context("bin shim")?;
    log.step(&format!("  wrote bin/{}.js", meta.binary));

    for p in &meta.platforms {
        generate_platform_package_json(&root, &meta.scope, &meta.binary, &version, p)
            .with_context(|| format!("npm/{}-{}-{}/package.json", meta.binary, p.os, p.cpu))?;
        log.step(&format!(
            "  wrote npm/{binary}-{os}-{cpu}/package.json",
            binary = meta.binary,
            os = p.os,
            cpu = p.cpu,
        ));
    }

    log.done(&format!(
        "Done — @{}/{}@{}",
        meta.scope, meta.binary, version
    ));
    Ok(())
}
