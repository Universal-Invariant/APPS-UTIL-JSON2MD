// src/js_helpers.rs
//! Dynamic JavaScript helper loading via QuickJS engine.
//!
//! Enabled with --features dynamic-helpers
//! Usage: --js-helpers path/to/helpers.js

#![allow(unexpected_cfgs)]

use anyhow::{Context, Result};
use handlebars::{
    Context as HbContext, Handlebars, Helper, Output, RenderContext, RenderError, RenderErrorReason,
};
use serde_json::Value;
use std::path::Path;

#[cfg(feature = "dynamic-helpers")]
use rquickjs::{
    CatchResultExt, Context as JsContext, Ctx, Filter, Runtime, Undefined, Value as JsValue,
};
#[cfg(feature = "dynamic-helpers")]
use std::sync::{Arc, Mutex};

/// Registry for dynamically loaded helpers (JS via QuickJS, Rust via libloading)
pub struct DynamicHelperRegistry {
    #[cfg(feature = "dynamic-helpers")]
    js_runtime: Option<(Runtime, Arc<Mutex<JsContext>>)>,
    #[cfg(feature = "dynamic-helpers")]
    loaded_plugins: Vec<libloading::Library>,
    #[cfg(feature = "dynamic-helpers")]
    js_helper_names: Vec<String>,
}

impl DynamicHelperRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self {
            #[cfg(feature = "dynamic-helpers")]
            js_runtime: None,
            #[cfg(feature = "dynamic-helpers")]
            loaded_plugins: Vec::new(),
            #[cfg(feature = "dynamic-helpers")]
            js_helper_names: Vec::new(),
        }
    }

    /// Stub implementation when dynamic-helpers feature is disabled
    #[cfg(not(feature = "dynamic-helpers"))]
    pub fn load_js_helpers(&mut self, path: &Path) -> Result<Vec<String>> {
        eprintln!("⚠️ JS helpers require: cargo build --features dynamic-helpers");
        Ok(vec![])
    }

    /// Stub implementation when dynamic-helpers feature is disabled
    #[cfg(not(feature = "dynamic-helpers"))]
    pub fn load_rust_plugin(
        &mut self,
        path: &Path,
        _hb: &mut Handlebars<'_>,
    ) -> Result<Vec<String>> {
        eprintln!("⚠️ Rust plugins require: cargo build --features dynamic-helpers");
        Ok(vec![])
    }

    /// Stub implementation when dynamic-helpers feature is disabled
    #[cfg(not(feature = "dynamic-helpers"))]
    pub fn register_with_handlebars(&self, _hb: &mut Handlebars<'_>) -> Result<()> {
        Ok(())
    }

    /// Load JavaScript helpers from file using QuickJS engine
    #[cfg(feature = "dynamic-helpers")]
    pub fn load_js_helpers(&mut self, js_path: &Path) -> Result<Vec<String>> {
        let js_code = std::fs::read_to_string(js_path)
            .with_context(|| format!("Failed to read JS: {}", js_path.display()))?;

        let rt = Runtime::new().context("QuickJS runtime init failed")?;
        let ctx = JsContext::full(&rt).context("QuickJS context init failed")?;
        let ctx = Arc::new(Mutex::new(ctx));

        let discovered = {
            let ctx_guard = ctx.lock().unwrap();
            ctx_guard
                .with(|ctx| {
                    // Inject minimal console stub to prevent "console is not defined" errors
                    let console_inject = r#"
globalThis.console = { log: function() {}, error: function() {} };
"#;
                    let _ = ctx.eval::<(), _>(console_inject.as_bytes()).catch(&ctx);

                    // Execute user helper code
                    let _ = ctx.eval::<(), _>(js_code.as_bytes()).catch(&ctx);

                    let globals = ctx.globals();
                    let mut found = Vec::new();

                    // Scan globals for user-defined helpers (exclude built-in JS functions)
                    let keys_iter = globals.own_keys::<String>(Filter::new().string());
                    for key_result in keys_iter {
                        if let Ok(key) = key_result {
                            // Skip known JavaScript built-ins
                            if is_builtin_js_function(&key) {
                                continue;
                            }

                            // Verify it's actually a function
                            if let Ok(val) = globals.get::<_, JsValue>(&key) {
                                if val.is_function() {
                                    found.push(key);
                                }
                            }
                        }
                    }
                    Ok(found)
                })
                .map_err(|e: rquickjs::Error| anyhow::anyhow!("JS context error: {}", e))?
        };

        self.js_runtime = Some((rt, ctx));
        self.js_helper_names = discovered.clone();
        Ok(discovered)
    }

    /// Register discovered JS helpers with Handlebars instance
    #[cfg(feature = "dynamic-helpers")]
    pub fn register_with_handlebars(&self, hb: &mut Handlebars<'_>) -> Result<()> {
        if let Some((_, ctx_arc)) = &self.js_runtime {
            for name in &self.js_helper_names {
                let js_name = name.clone();
                let ctx_clone = ctx_arc.clone();

                // Create Handlebars helper closure that calls JS function via QuickJS
                let helper = move |h: &Helper<'_>,
                                   _: &Handlebars<'_>,
                                   _: &HbContext,
                                   _: &mut RenderContext<'_, '_>,
                                   out: &mut dyn Output|
                      -> Result<(), RenderError> {
                    let ctx_guard = ctx_clone.lock().unwrap();

                    let call_result = ctx_guard.with(|ctx| -> Result<String, String> {
                        // Get JS function from global scope
                        let js_func: rquickjs::Function = ctx
                            .globals()
                            .get(&js_name)
                            .map_err(|e| format!("Helper '{}' not found: {}", js_name, e))?;

                        // Convert Handlebars params to QuickJS values
                        let mut js_args: Vec<JsValue> = Vec::new();
                        for param in h.params() {
                            let val = param.value();
                            if let Ok(js_val) = serde_value_to_js(&ctx, val) {
                                js_args.push(js_val);
                            }
                        }

                        // Call JS function with appropriate argument pattern
                        let js_result: Result<JsValue<'_>, rquickjs::CaughtError<'_>> =
                            match js_args.len() {
                                0 => js_func.call(()).catch(&ctx),
                                1 => js_func.call((js_args[0].clone(),)).catch(&ctx),
                                2 => js_func
                                    .call((js_args[0].clone(), js_args[1].clone()))
                                    .catch(&ctx),
                                3 => js_func
                                    .call((
                                        js_args[0].clone(),
                                        js_args[1].clone(),
                                        js_args[2].clone(),
                                    ))
                                    .catch(&ctx),
                                4 => js_func
                                    .call((
                                        js_args[0].clone(),
                                        js_args[1].clone(),
                                        js_args[2].clone(),
                                        js_args[3].clone(),
                                    ))
                                    .catch(&ctx),
                                5 => js_func
                                    .call((
                                        js_args[0].clone(),
                                        js_args[1].clone(),
                                        js_args[2].clone(),
                                        js_args[3].clone(),
                                        js_args[4].clone(),
                                    ))
                                    .catch(&ctx),
                                6 => js_func
                                    .call((
                                        js_args[0].clone(),
                                        js_args[1].clone(),
                                        js_args[2].clone(),
                                        js_args[3].clone(),
                                        js_args[4].clone(),
                                        js_args[5].clone(),
                                    ))
                                    .catch(&ctx),
                                _ => {
                                    // Fallback: pack args into array + apply pattern
                                    let args_arr = rquickjs::Array::new(ctx.clone())
                                        .map_err(|e| e.to_string())?;
                                    for (i, arg) in js_args.iter().enumerate() {
                                        let _ = args_arr.set(i, arg.clone());
                                    }
                                    js_func.call((Undefined, args_arr)).catch(&ctx)
                                }
                            };

                        // Convert JS result to Rust String for Handlebars
                        match js_result {
                            Ok(result_val) => {
                                if let Some(js_str) = result_val.as_string() {
                                    js_str.to_string().map_err(|e| e.to_string())
                                } else {
                                    // Fallback: JSON stringify complex results
                                    let json_global: rquickjs::Object = ctx
                                        .globals()
                                        .get("JSON")
                                        .map_err(|e| format!("JSON global not found: {}", e))?;
                                    let stringify: rquickjs::Function = json_global
                                        .get("stringify")
                                        .map_err(|e| format!("JSON.stringify not found: {}", e))?;

                                    match stringify
                                        .call::<_, rquickjs::Value<'_>>((result_val,))
                                        .catch(&ctx)
                                    {
                                        Ok(json_val) => {
                                            if let Some(json_str) = json_val.as_string() {
                                                json_str.to_string().map_err(|e| e.to_string())
                                            } else {
                                                Err("JSON.stringify returned non-string".to_string())
                                            }
                                        }
                                        Err(e) => Err(format!("JSON.stringify failed: {}", e)),
                                    }
                                }
                            }
                            Err(e) => Err(format!("JS call failed: {}", e)),
                        }
                    });

                    // Write result to Handlebars output or return error
                    match call_result {
                        Ok(output) => {
                            out.write(&output)
                                .map_err(|e| RenderError::from(RenderErrorReason::NestedError(Box::new(e))))?;
                        }
                        Err(e) => {
                            return Err(RenderError::from(RenderErrorReason::Other(format!(
                                "Helper '{}': {}",
                                js_name, e
                            ))));
                        }
                    }
                    Ok(())
                };

                hb.register_helper(name, Box::new(helper));
            }
        }
        Ok(())
    }

    /// Load Rust plugin library and register its helpers
    #[cfg(feature = "dynamic-helpers")]
    pub fn load_rust_plugin(
        &mut self,
        lib_path: &Path,
        target_hb: &mut Handlebars<'_>,
    ) -> Result<Vec<String>> {
        use libloading::Library;

        let lib = unsafe { Library::new(lib_path) }
            .with_context(|| format!("Failed to load: {}", lib_path.display()))?;

        let factory: libloading::Symbol<crate::plugin::PluginFactory> =
            unsafe { lib.get(b"create_helpers") }
                .with_context(|| "Missing 'create_helpers' export")?;

        let plugin = factory();
        plugin.register(target_hb);
        self.loaded_plugins.push(lib);
        Ok(vec![])
    }
}

/// Check if a global name is a built-in JavaScript function to exclude from helper discovery
#[cfg(feature = "dynamic-helpers")]
fn is_builtin_js_function(name: &str) -> bool {
    const BUILTINS: &[&str] = &[
        "undefined", "NaN", "Math", "Reflect", "globalThis", "JSON", "Atomics",
        "performance", "Infinity", "Object", "Function", "Error", "EvalError",
        "RangeError", "ReferenceError", "SyntaxError", "TypeError", "URIError",
        "InternalError", "AggregateError", "Iterator", "Array", "parseInt",
        "parseFloat", "isNaN", "isFinite", "queueMicrotask", "decodeURI",
        "decodeURIComponent", "encodeURI", "encodeURIComponent", "escape",
        "unescape", "Number", "Boolean", "String", "Symbol", "eval", "Date",
        "RegExp", "Proxy", "Map", "Set", "WeakMap", "WeakSet", "ArrayBuffer",
        "SharedArrayBuffer", "Uint8ClampedArray", "Int8Array", "Uint8Array",
        "Int16Array", "Uint16Array", "Int32Array", "Uint32Array", "BigInt64Array",
        "BigUint64Array", "Float16Array", "Float32Array", "Float64Array",
        "DataView", "Promise", "BigInt", "WeakRef", "FinalizationRegistry",
        "DOMException",
    ];
    BUILTINS.contains(&name)
}

/// Convert serde_json::Value to rquickjs::Value for JS interop
#[cfg(feature = "dynamic-helpers")]
fn serde_value_to_js<'js>(ctx: &Ctx<'js>, val: &Value) -> Result<JsValue<'js>, String> {
    use rquickjs::IntoJs;

    match val {
        Value::Null => Ok(rquickjs::Null.into_js(ctx).map_err(|e| e.to_string())?),
        Value::Bool(b) => Ok(b.into_js(ctx).map_err(|e| e.to_string())?),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(i.into_js(ctx).map_err(|e| e.to_string())?)
            } else if let Some(f) = n.as_f64() {
                Ok(f.into_js(ctx).map_err(|e| e.to_string())?)
            } else {
                Ok(rquickjs::Null.into_js(ctx).map_err(|e| e.to_string())?)
            }
        }
        Value::String(s) => Ok(s.as_str().into_js(ctx).map_err(|e| e.to_string())?),
        Value::Array(arr) => {
            let js_arr = rquickjs::Array::new(ctx.clone()).map_err(|e| e.to_string())?;
            for (i, item) in arr.iter().enumerate() {
                if let Ok(js_val) = serde_value_to_js(ctx, item) {
                    let _ = js_arr.set(i, js_val);
                }
            }
            Ok(js_arr.into_value())
        }
        Value::Object(obj) => {
            let js_obj = rquickjs::Object::new(ctx.clone()).map_err(|e| e.to_string())?;
            for (k, v) in obj {
                if let Ok(js_val) = serde_value_to_js(ctx, v) {
                    let _ = js_obj.set(k.as_str(), js_val);
                }
            }
            Ok(js_obj.into_value())
        }
    }
}