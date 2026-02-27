#![allow(unexpected_cfgs)]
//! JSON/CSV to Markdown converter with Handlebars templating and dynamic helpers.
//!
//! Supports:
//! - Built-in Rust helpers (table, substring, replacereg, etc.)
//! - Dynamic JS helpers via QuickJS (--js-helpers flag)
//! - Dynamic Rust plugins via libloading (--rs-plugin flag)

mod js_helpers;
mod plugin;

use anyhow::{Context, Result};
use clap::Parser;
use csv;
use handlebars::{
    Context as HbContext, Handlebars, Helper, RenderContext, RenderError, RenderErrorReason,
};
use js_helpers::DynamicHelperRegistry;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

/// Output strategy: single consolidated file or multiple files in a directory
#[derive(Clone)]
enum OutputStrategy {
    /// Write all rendered items to a single file
    SingleFile(PathBuf),
    /// Write each item to a separate file in the specified directory
    /// Optional split_config overrides per-item naming
    MultiFile {
        directory: PathBuf,
        split_config: Option<SplitConfig>,
    },
}

/// Configuration for per-item filename generation in multi-file mode
#[derive(Clone, Debug)]
struct SplitConfig {
    /// Template for generating per-item filenames (supports Handlebars syntax)
    /// - Empty: use settings.json_name
    /// - Plain string: treat as JSON path (e.g., "title", "user.name")
    /// - Contains "{{": treat as Handlebars template
    template: String,
}

impl SplitConfig {
    /// Parse split argument: empty → index mode, plain → path, "{{" → template
    fn from_arg(arg: Option<&str>) -> Self {
        match arg {
            None | Some("") => Self {
                template: String::new(),
            }, // Index mode
            Some(s) => Self {
                template: s.to_string(),
            }, // JSON path mode
        }
    }

    /// Check if using index-based naming (no template/path provided)
    fn is_index_mode(&self) -> bool {
        self.template.is_empty()
    }

    /// Check if using Handlebars template for naming
    fn is_template_mode(&self) -> bool {
        self.template.contains("{{")
    }
}

// ============================================================================
// Configuration
// ============================================================================

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct JsonImportSettings {
    /// Field to use for output filename (supports Handlebars template syntax)
    pub json_name: String,
    /// Allow path separators in json_name (creates subdirectories)
    pub json_name_path: bool,
    /// Output folder for generated markdown files
    pub folder_name: String,
    /// Top-level field to iterate over (for nested JSON structures)
    pub top_field: String,
    /// Prefix for output filenames
    pub note_prefix: String,
    /// Suffix for output filenames
    pub note_suffix: String,
    /// Force treating objects as arrays (single-item iteration)
    pub force_array: bool,
    /// Ensure unique filenames by appending counter on collision
    pub unique_names: bool,
}

impl Default for JsonImportSettings {
    fn default() -> Self {
        Self {
            json_name: "name".to_string(),
            json_name_path: false,
            folder_name: "JSON2MD".to_string(),
            top_field: String::new(),
            note_prefix: String::new(),
            note_suffix: String::new(),
            force_array: true,
            unique_names: false,
        }
    }
}

// ============================================================================
// CLI Arguments
// ============================================================================

#[derive(Parser, Debug)]
#[command(name = "json-to-md")]
#[command(about = "Convert JSON/CSV to Markdown with Handlebars templates and dynamic helpers")]
#[command(version)]
struct Args {
    /// Input data file (.json or .csv)
    #[arg(value_name = "DATA_FILE")]
    data_file: PathBuf,

    /// Handlebars template file (.md)
    #[arg(value_name = "TEMPLATE_FILE")]
    template_file: PathBuf,

    /// Output file path (single file mode). If omitted, generates multiple files in folder_name
    #[arg(short = 'o', long = "output", value_name = "FILE")]
    output: Option<PathBuf>,

    /// JavaScript helper file to load dynamically
    #[arg(long = "js-helpers", value_name = "FILE")]
    js_helpers: Option<PathBuf>,

    /// Rust plugin library to load (.so/.dll/.dylib)
    #[arg(long = "rs-plugin", value_name = "FILE")]
    rs_plugin: Option<PathBuf>,

    /// Settings file (JSON) to override defaults
    #[arg(short, long, value_name = "FILE")]
    settings: Option<PathBuf>,

    /// Enable verbose debug output
    #[arg(short, long)]
    verbose: bool,

    /// Split output: generate one file per array entry.
    /// - Without arg: append index (output_0.md, output_1.md)
    /// - With field path: use JSON field value (output_{value}.md)
    /// - With Handlebars: use template syntax (output_{{upper title}}.md)
    ///
    /// Note: The CLI uses Option<Option<String>> to distinguish:
    /// - No flag: None
    /// - `-s` (no value): Some(None)
    /// - `-s value`: Some(Some("value"))
    #[arg(short = 'x', long = "split", value_name = "TEMPLATE", num_args = 0..=1)]
    split: Option<Option<String>>,
}

// ============================================================================
// Logging Utilities
// ============================================================================

/// Conditional debug logging - only prints if verbose mode is enabled
macro_rules! debug_log {
    ($verbose:expr, $($arg:tt)*) => {
        if $verbose {
            eprintln!($($arg)*);
        }
    };
}

/// User-facing info message (always printed to stderr)
macro_rules! info_log {
    ($($arg:tt)*) => {
        eprintln!($($arg)*);
    };
}

/// User-facing success message (printed to stdout)
macro_rules! success_log {
    ($($arg:tt)*) => {
        println!($($arg)*);
    };
}

/// Error logging helper
macro_rules! error_log {
    ($($arg:tt)*) => {
        eprintln!("Error: {}", format!($($arg)*));
    };
}

// ============================================================================
// Utilities
// ============================================================================

/// Navigate nested JSON using dot notation: "user.profile.name"
/// Supports '@' prefix to fallback to alternative data source
fn objfield(src: &Value, field: &str, fallback: Option<&Value>) -> Option<Value> {
    if field.is_empty() {
        return Some(src.clone());
    }

    let (path, source) = if field.starts_with('@') && fallback.is_some() {
        (&field[1..], fallback.unwrap())
    } else {
        (field, src)
    };

    let mut current = source;
    for part in path.split('.') {
        current = match current {
            Value::Object(obj) => obj.get(part)?,
            _ => return None,
        };
    }
    Some(current.clone())
}

/// Sanitize filename for filesystem safety across platforms
fn valid_filename(name: &str, allow_paths: bool) -> String {
    let pattern = if allow_paths {
        r#"[<>:"\\|?\*]"#
    } else {
        r#"[<>:"/\\|?\*]"#
    };
    Regex::new(pattern)
        .expect("valid_filename regex compilation failed")
        .replace_all(name, "_")
        .to_string()
}

/// Convert displayable errors to Handlebars RenderError
fn re_err(msg: impl std::fmt::Display) -> RenderError {
    RenderError::from(RenderErrorReason::Other(msg.to_string()))
}

// ============================================================================
// Built-in Handlebars Helpers
// ============================================================================

/// replace regex multiple
fn hb_table_regex(
    h: &Helper<'_>,
    _: &Handlebars<'_>,
    _: &HbContext,
    _: &mut RenderContext<'_, '_>,
    out: &mut dyn handlebars::Output,
) -> Result<(), RenderError> {
    let params = h.params();
    if params.len() < 3 {
        return Ok(());
    }

    let input = params[0].render();
    for chunk in params[1..params.len() - 1].chunks(2) {
        if chunk.len() < 2 {
            break;
        }
        let pattern = chunk[0].render();
        let replacement = chunk[1].render();

        if let Ok(re) = Regex::new(&format!("^{}$", &pattern)) {
            if let Some(caps) = re.captures(&input) {
                let mut result = replacement;
                for (i, m) in caps.iter().enumerate().skip(1) {
                    if let Some(text) = m {
                        result = result.replace(&format!("${}", i), text.as_str());
                    }
                }
                return Ok(out.write(&result).map_err(re_err)?);
            }
        }
    }
    Ok(out.write(&input).map_err(re_err)?)
}

/// replace with regex
fn hb_replace_regex(
    h: &Helper<'_>,
    _: &Handlebars<'_>,
    _: &HbContext,
    _: &mut RenderContext<'_, '_>,
    out: &mut dyn handlebars::Output,
) -> Result<(), RenderError> {
    let params = h.params();
    if params.len() != 3 {
        return Ok(());
    }

    let text = params[0].render();
    let pattern = params[1].render();
    let repl = params[2].render();

    match Regex::new(&pattern) {
        Ok(re) => Ok(out
            .write(&re.replace_all(&text, repl.as_str()))
            .map_err(re_err)?),
        Err(e) => {
            // Log regex error but continue with original text
            debug_log!(true, "⚠️ Invalid regex '{}': {}", pattern, e);
            Ok(out.write(&text).map_err(re_err)?)
        }
    }
}

/// Register all built-in helpers with the Handlebars instance
fn register_helpers(hb: &mut Handlebars<'_>) {
    hb.register_helper("tableRegex", Box::new(hb_table_regex));
    hb.register_helper("replaceRegex", Box::new(hb_replace_regex));
}

// ============================================================================
// Core Generation Logic
// ============================================================================

/// Determine output strategy based on CLI args, data structure, and settings
fn determine_output_strategy(
    output_arg: Option<&PathBuf>,
    split_arg: Option<Option<&str>>,
    data: &Value,
    settings: &JsonImportSettings,
) -> Result<OutputStrategy> {
    // Parse split configuration
    let split_config = split_arg.map(SplitConfig::from_arg);

    match output_arg {
        // User explicitly specified output path
        Some(out) => {
            // Check if it's likely a directory vs file
            let is_dir = out.is_dir()
                || out.to_string_lossy().ends_with('/')
                || out.to_string_lossy().ends_with('\\')
                || (out.extension().is_none() && out.file_name().is_some());

            if is_dir {
                // Ensure directory exists
                fs::create_dir_all(out)?;
                Ok(OutputStrategy::MultiFile {
                    directory: out.clone(),
                    split_config,
                })
            } else {
                // Single-file mode: ensure parent dir exists
                if let Some(parent) = out.parent() {
                    fs::create_dir_all(parent)?;
                }
                Ok(OutputStrategy::SingleFile(out.clone()))
            }
        }
        // No output specified: infer from data structure
        None => {
            match data {
                // Single-item array: default to single-file mode for convenience
                Value::Array(arr) if arr.len() == 1 => {
                    // Derive filename from json_name field
                    let item = &arr[0];
                    let base_name = if settings.json_name.contains("{{") {
                        // Template syntax: use placeholder (user should use -o for this case)
                        "output".to_string()
                    } else {
                        objfield(item, &settings.json_name, None)
                            .and_then(|v| v.as_str().map(String::from))
                            .unwrap_or_else(|| "output".to_string())
                    };

                    let filename = format!(
                        "{}{}{}.md",
                        settings.note_prefix,
                        valid_filename(&base_name, settings.json_name_path),
                        settings.note_suffix
                    );

                    Ok(OutputStrategy::SingleFile(PathBuf::from(filename)))
                }
                // Multiple items: default to multi-file mode with optional split
                _ => {
                    let out_dir = PathBuf::from(&settings.folder_name);
                    fs::create_dir_all(&out_dir)?;
                    Ok(OutputStrategy::MultiFile {
                        directory: out_dir,
                        split_config,
                    })
                }
            }
        }
    }
}

/// Generate filename for a single item based on split configuration
fn generate_item_filename(
    item: &Value,
    idx: usize,
    base_name: &str,
    split_config: Option<&SplitConfig>,
    settings: &JsonImportSettings,
    hb: &Handlebars<'_>,
) -> Result<String> {
    let name = match split_config {
        None => {
            // Use settings.json_name (original behavior)
            if settings.json_name.contains("{{") {
                hb.render_template(&settings.json_name, item)?
            } else {
                objfield(item, &settings.json_name, None)
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_else(|| format!("item_{}", idx))
            }
        }
        Some(config) if config.is_index_mode() => {
            // Index mode: append counter
            format!("{}_{}", base_name, idx)
        }
        Some(config) if config.is_template_mode() => {
            // Handlebars template mode: render with full context
            hb.render_template(&config.template, item)?
        }
        Some(config) => {
            // JSON path mode: extract field value
            objfield(item, &config.template, None)
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_else(|| format!("{}_{}", base_name, idx))
        }
    };

    // Apply prefix/suffix and sanitize
    let final_name = format!(
        "{}{}{}",
        settings.note_prefix,
        valid_filename(&name, settings.json_name_path),
        settings.note_suffix
    );

    Ok(final_name)
}

/// Process data and generate markdown using the template and helpers
fn generate_notes(
    hb: &mut Handlebars<'_>,
    data: Value,
    source_name: &str,
    template_src: &str,
    settings: &JsonImportSettings,
    output_strategy: OutputStrategy,
    verbose: bool,
) -> Result<()> {
    info_log!("Converting: {}", source_name);

    hb.register_template_string("tpl", template_src)
        .context("Template compilation failed")?;

    let seen_names = std::cell::RefCell::new(HashSet::new());
    let data_ref = &data;

    // For single-file mode: accumulate content
    let mut single_file_content = String::new();
    let mut item_count = 0;
    let item_separator = "\n\n---\n\n"; // Configurable via settings if desired

    let mut process_item = |item: &Value, idx: usize, output: &OutputStrategy| -> Result<()> {
        if !item.is_object() {
            return Ok(());
        }

        // Build render context with item data + metadata
        let mut ctx_map = serde_json::Map::new();
        if let Value::Object(obj) = item {
            ctx_map.extend(obj.clone());
        }
        ctx_map.insert("SourceIndex".into(), (idx as i64).into());
        ctx_map.insert("dataRoot".into(), data_ref.clone());
        ctx_map.insert("SourceFilename".into(), source_name.into());

        // Generate filename for this item (used for multi-file output OR template context)
        let item_filename = match output {
            OutputStrategy::MultiFile {
                directory,
                split_config,
            } => {
                // Multi-file mode: generate actual output filename
                let base_name = directory
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("output");

                generate_item_filename(item, idx, base_name, split_config.as_ref(), settings, hb)?
            }
            OutputStrategy::SingleFile(_) => {
                // Single-file mode: generate placeholder for template context only
                if settings.json_name.contains("{{") {
                    hb.render_template(&settings.json_name, &Value::Object(ctx_map.clone()))
                        .unwrap_or_default()
                } else {
                    let ctx_for_lookup = Value::Object(ctx_map.clone());
                    objfield(&ctx_for_lookup, &settings.json_name, Some(data_ref))
                        .and_then(|v| v.as_str().map(String::from))
                        .unwrap_or_else(|| format!("item_{}", idx))
                }
            }
        };

        // Add _note_name_ to context so templates can reference it (optional but useful)
        ctx_map.insert("_note_name_".into(), Value::String(item_filename.clone()));
        let ctx = Value::Object(ctx_map); // Rebuild ctx with _note_name_ included

        // For multi-file mode: skip items with empty filenames (can't write _.md)
        if matches!(output, OutputStrategy::MultiFile { .. }) && item_filename.is_empty() {
            debug_log!(
                verbose,
                "⚠️ Skipping item {}: empty filename (multi-file mode)",
                idx
            );
            return Ok(());
        }

        // Render template to markdown (always needed)
        let body = hb.render("tpl", &ctx).context("Template render failed")?;

        // Handle output based on strategy
        match output {
            OutputStrategy::SingleFile(_output_file) => {
                // SINGLE-FILE MODE: Accumulate content
                if item_count > 0 {
                    single_file_content.push_str(item_separator);
                }
                single_file_content.push_str(&body);
                item_count += 1;
                debug_log!(
                    verbose,
                    "📝 Appended item {} to single output ({} bytes)",
                    idx,
                    body.len()
                );
            }
            OutputStrategy::MultiFile {
                directory: output_dir,
                ..
            } => {
                // MULTI-FILE MODE: Write individual files using generated filename
                let safe = valid_filename(&item_filename, settings.json_name_path);
                let mut path = output_dir.join(&safe);

                // Handle filename collisions
                let path_str = path.to_string_lossy().to_string();
                if settings.unique_names || seen_names.borrow().contains(&path_str) {
                    let base = path.clone();
                    let mut n = 0;
                    while seen_names
                        .borrow()
                        .contains(&path.to_string_lossy().to_string())
                    {
                        n += 1;
                        path = base.with_file_name(format!(
                            "{}{}",
                            base.file_stem().unwrap().to_string_lossy(),
                            n
                        ));
                        if let Some(ext) = base.extension() {
                            path = path.with_extension(ext);
                        }
                    }
                }
                seen_names
                    .borrow_mut()
                    .insert(path.to_string_lossy().to_string());
                path.set_extension("md");

                fs::write(&path, &body)?;

                debug_log!(
                    verbose,
                    "✅ Wrote {} bytes to {}",
                    body.len(),
                    path.display()
                );
                success_log!("Created: {}", path.display());
                item_count += 1;
            }
        }
        Ok(())
    };

    // Resolve target data (support nested top_field)
    let target = if !settings.top_field.is_empty() {
        objfield(data_ref, &settings.top_field, None)
            .context(format!("Field '{}' not found", settings.top_field))?
    } else {
        data_ref.clone()
    };

    // Iterate and process each item
    match target {
        Value::Array(arr) => {
            for (i, item) in arr.iter().enumerate() {
                process_item(item, i, &output_strategy)?;
            }
        }
        Value::Object(_) if settings.force_array => {
            process_item(&target, 0, &output_strategy)?;
        }
        Value::Object(obj) => {
            for (i, (_, val)) in obj.into_iter().enumerate() {
                process_item(&val, i, &output_strategy)?;
            }
        }
        _ => {
            process_item(&target, 0, &output_strategy)?;
        }
    }

    // Write single output file if in single-file mode
    if let OutputStrategy::SingleFile(output_file) = &output_strategy {
        if item_count == 0 {
            debug_log!(verbose, "⚠️ No items rendered to output file");
            // Write empty file to indicate success
            fs::write(output_file, "")?;
        } else {
            fs::write(output_file, &single_file_content)?;
            success_log!(
                "Created: {} ({} items, {} bytes)",
                output_file.display(),
                item_count,
                single_file_content.len()
            );
            debug_log!(
                verbose,
                "✅ Wrote {} items to {}",
                item_count,
                output_file.display()
            );
        }
    }

    Ok(())
}

// ============================================================================
// Entry Point
// ============================================================================

fn main() -> Result<()> {
    let args = Args::parse();
    let verbose = args.verbose;

    // Load settings (file or defaults)
    let settings = if let Some(p) = &args.settings {
        serde_json::from_str(&fs::read_to_string(p)?)?
    } else {
        JsonImportSettings::default()
    };

    // Validate and read input data
    let data_path = &args.data_file;
    if !data_path.exists() {
        anyhow::bail!("Data file not found: {}", data_path.display());
    }

    let raw = fs::read_to_string(data_path)
        .with_context(|| format!("Failed to read data file: {}", data_path.display()))?;

    debug_log!(
        verbose,
        "📄 Reading: {} ({} bytes)",
        data_path.display(),
        raw.len()
    );

    // Strip UTF-8 BOM if present (common on Windows)
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(&raw);

    // Detect format by extension
    let is_csv = data_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("csv"))
        .unwrap_or(false);

    debug_log!(
        verbose,
        "📋 Format detected: {}",
        if is_csv { "CSV" } else { "JSON" }
    );

    // Parse input data
    let data: Value = if is_csv {
        let mut rdr = csv::Reader::from_reader(raw.as_bytes());
        let headers = rdr
            .headers()
            .with_context(|| "CSV: failed to read headers")?
            .clone();
        let mut rows = Vec::new();
        for (line_num, record) in rdr.records().enumerate() {
            let record = record.with_context(|| format!("CSV: error on line {}", line_num + 2))?;
            let mut map = serde_json::Map::new();
            for (h, f) in headers.iter().zip(record.iter()) {
                map.insert(h.to_string(), Value::String(f.to_string()));
            }
            rows.push(Value::Object(map));
        }
        debug_log!(verbose, "✅ Parsed {} CSV rows", rows.len());
        Value::Array(rows)
    } else {
        serde_json::from_str(raw).with_context(|| {
            let first_line = raw.lines().next().unwrap_or("");
            format!("JSON parse failed. First line: {:?}", first_line)
        })?
    };

    // Load template
    let template = fs::read_to_string(&args.template_file).context("Read template")?;

    // Initialize Handlebars with built-in helpers
    let mut hb = Handlebars::new();
    hb.set_strict_mode(false);
    hb.register_escape_fn(handlebars::no_escape);
    register_helpers(&mut hb);

    // Load dynamic helpers if requested
    let mut dyn_helpers = DynamicHelperRegistry::new();

    if let Some(js_path) = &args.js_helpers {
        debug_log!(verbose, "🔌 Loading JS helpers from: {}", js_path.display());
        match dyn_helpers.load_js_helpers(js_path) {
            Ok(names) => {
                debug_log!(verbose, "✅ Loaded {} JS helpers: {:?}", names.len(), names);
            }
            Err(e) => {
                error_log!("Failed to load JS helpers: {}", e);
                // Continue without JS helpers rather than failing entirely
            }
        }
    }

    if let Some(rs_path) = &args.rs_plugin {
        debug_log!(
            verbose,
            "🔌 Loading Rust plugin from: {}",
            rs_path.display()
        );
        match dyn_helpers.load_rust_plugin(rs_path, &mut hb) {
            Ok(names) => {
                debug_log!(
                    verbose,
                    "✅ Loaded {} Rust plugin helpers: {:?}",
                    names.len(),
                    names
                );
            }
            Err(e) => {
                error_log!("Failed to load Rust plugin: {}", e);
                // Continue without plugin rather than failing entirely
            }
        }
    }

    // Register dynamic helpers with Handlebars
    if let Err(e) = dyn_helpers.register_with_handlebars(&mut hb) {
        error_log!("Failed to register dynamic helpers: {}", e);
        // Continue with built-in helpers only
    }

    // Determine output strategy
    let output_strategy = determine_output_strategy(
        args.output.as_ref(),
        args.split.as_ref().map(|opt| opt.as_deref()), // Convert Option<Option<String>> → Option<Option<&str>>
        &data,
        &settings,
    )?;
    // Generate notes with the determined strategy
    generate_notes(
        &mut hb,
        data,
        args.data_file
            .file_name()
            .unwrap()
            .to_string_lossy()
            .as_ref(),
        &template,
        &settings,
        output_strategy.clone(), // ← Pass the strategy
        verbose,
    )?;

    // Only print generic "Import Finished" for multi-file mode (single-file already logged)
    if matches!(output_strategy, OutputStrategy::MultiFile { .. }) {
        success_log!("Import Finished.");
    }

    Ok(())
}
