use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use errors::{Result, anyhow};
use rhai::{AST, CallFnOptions, Dynamic, Engine, Scope};
use tera::{
    Filter as TeraFilter, Function as TeraFunction, Result as TeraResult, Tera, Test as TeraTest,
    Value,
};

#[derive(Debug)]
pub struct Registry {
    engine: Arc<Engine>,
}

enum ScriptKind {
    Filter,
    Test,
    Function,
}

impl Registry {
    pub fn new() -> Registry {
        let engine = Engine::new();
        Registry { engine: Arc::new(engine) }
    }

    /// Register `filter.rhai`, `test.rhai`, and `function.rhai` from `folder`
    /// with `tera`, skipping any that don't exist.
    pub fn register(&self, folder: impl AsRef<Path>, tera: &mut Tera) -> Result<()> {
        let folder = folder.as_ref();
        for (file, kind) in [
            ("filter.rhai", ScriptKind::Filter),
            ("test.rhai", ScriptKind::Test),
            ("function.rhai", ScriptKind::Function),
        ] {
            let path = folder.join(file);
            if path.is_file() {
                self.register_file(&path, tera, kind)?;
            }
        }
        Ok(())
    }

    /// Register every function defined in the script with `tera` under the
    /// given kind. Rhai allows arity-based overloads; tera identifies by name
    /// only, so the first occurrence of each name wins.
    fn register_file(&self, path: &Path, tera: &mut Tera, kind: ScriptKind) -> Result<()> {
        let path = path.canonicalize()?;
        let ast = Arc::new(self.engine.compile_file(path.clone()).map_err(|e| {
            anyhow!("failed to compile rhai script, path={}, error={}", path.display(), e)
        })?);

        let mut seen = HashSet::new();
        for meta in ast.iter_functions() {
            if !seen.insert(meta.name.to_string()) {
                continue;
            }
            let handler = ScriptFn {
                engine: Arc::clone(&self.engine),
                ast: Arc::clone(&ast),
                name: meta.name.to_string(),
            };
            match kind {
                ScriptKind::Filter => tera.register_filter(&handler.name, handler.clone()),
                ScriptKind::Test => tera.register_tester(&handler.name, handler.clone()),
                ScriptKind::Function => tera.register_function(&handler.name, handler.clone()),
            };
        }
        Ok(())
    }
}

#[derive(Clone)]
struct ScriptFn {
    engine: Arc<Engine>,
    ast: Arc<AST>,
    name: String,
}

impl ScriptFn {
    fn invoke(&self, args: Vec<Dynamic>) -> std::result::Result<Dynamic, Box<rhai::EvalAltResult>> {
        let mut scope = Scope::new();

        let opts = CallFnOptions::new().rewind_scope(false).eval_ast(false);
        self.engine.call_fn_with_options(opts, &mut scope, &self.ast, &self.name, args)
    }
}

fn to_dynamic(v: &Value) -> TeraResult<Dynamic> {
    rhai::serde::to_dynamic(v)
        .map_err(|e| tera::Error::msg(format!("converting value to rhai: {e}")))
}

fn from_dynamic(d: Dynamic) -> TeraResult<Value> {
    rhai::serde::from_dynamic(&d)
        .map_err(|e| tera::Error::msg(format!("converting rhai result: {e}")))
}

fn args_to_dynamic(args: &HashMap<String, Value>) -> TeraResult<Dynamic> {
    let obj = serde_json::to_value(args)
        .map_err(|e| tera::Error::msg(format!("serializing args: {e}")))?;
    to_dynamic(&obj)
}

// `value | name(k=v, ...)`  -> rhai fn(value, args_map)
impl TeraFilter for ScriptFn {
    fn filter(&self, value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
        let result = self
            .invoke(vec![to_dynamic(value)?, args_to_dynamic(args)?])
            .map_err(|e| tera::Error::msg(format!("rhai filter `{}`: {e}", self.name)))?;
        from_dynamic(result)
    }
}

// `name(k=v, ...)`  -> rhai fn(args_map)
impl TeraFunction for ScriptFn {
    fn call(&self, args: &HashMap<String, Value>) -> TeraResult<Value> {
        let result = self
            .invoke(vec![args_to_dynamic(args)?])
            .map_err(|e| tera::Error::msg(format!("rhai function `{}`: {e}", self.name)))?;
        from_dynamic(result)
    }
}

// `x is name(a, b, ...)`  -> rhai fn(value, a, b, ...) returning bool
impl TeraTest for ScriptFn {
    fn test(&self, value: Option<&Value>, args: &[Value]) -> TeraResult<bool> {
        let mut rhai_args: Vec<Dynamic> = Vec::with_capacity(args.len() + 1);
        rhai_args.push(match value {
            Some(v) => to_dynamic(v)?,
            None => Dynamic::UNIT,
        });
        for a in args {
            rhai_args.push(to_dynamic(a)?);
        }
        let result = self
            .invoke(rhai_args)
            .map_err(|e| tera::Error::msg(format!("rhai test `{}`: {e}", self.name)))?;
        result.as_bool().map_err(|got| {
            tera::Error::msg(format!("rhai test `{}` must return bool, got `{got}`", self.name))
        })
    }
}
