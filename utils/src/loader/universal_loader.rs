use deno_core::anyhow::bail;
use deno_core::error::AnyError;
use deno_core::futures::FutureExt;
use deno_core::resolve_import;
use deno_core::ModuleLoader;
use deno_core::ModuleSource;
use deno_core::ModuleSourceFuture;
use deno_core::ModuleSpecifier;
use deno_core::ModuleType;
use deno_graph::source::LoadFuture;
use deno_graph::source::LoadResponse;
use deno_graph::source::Loader;
use deno_transpiler::compile;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use crate::get_source_code;
use crate::ModuleStore;

#[derive(Default, Clone)]
pub struct UniversalModuleLoader {
    store: Option<Arc<dyn ModuleStore>>,
    compile: bool,
}

impl UniversalModuleLoader {
    pub fn new(module_store: Option<Arc<dyn ModuleStore>>, compile: bool) -> Self {
        Self {
            store: module_store,
            compile,
        }
    }

    pub async fn get_and_update_source(
        self,
        m: &ModuleSpecifier,
        minify: bool,
    ) -> Result<String, AnyError> {
        let mut code = get_source_code(m).await?;
        if self.compile {
            code = compile(m, code, minify)?;
        }
        if let Some(store) = self.store.as_ref() {
            store.put(m.to_string(), code.clone());
        }
        Ok(code)
    }
}

impl ModuleLoader for UniversalModuleLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _is_main: bool,
    ) -> Result<ModuleSpecifier, AnyError> {
        Ok(resolve_import(specifier, referrer)?)
    }

    fn load(
        &self,
        module_specifier: &ModuleSpecifier,
        _maybe_referrer: Option<ModuleSpecifier>,
        _is_dyn_import: bool,
    ) -> Pin<Box<ModuleSourceFuture>> {
        let m = module_specifier.clone();
        let string_specifier = m.to_string();

        let loader = self.clone();
        async move {
            let module_type = get_module_type(&m)?;
            if let Some(store) = loader.store.as_ref() {
                if let Some(code) = store.get(&string_specifier) {
                    return Ok(ModuleSource {
                        code,
                        module_type,
                        module_url_specified: string_specifier.clone(),
                        module_url_found: string_specifier,
                    });
                }
            }
            let code = loader.get_and_update_source(&m, false).await?;

            Ok(ModuleSource {
                code,
                module_type,
                module_url_specified: string_specifier.clone(),
                module_url_found: string_specifier,
            })
        }
        .boxed_local()
    }
}

impl Loader for UniversalModuleLoader {
    fn load(&mut self, specifier: &ModuleSpecifier, _is_dynamic: bool) -> LoadFuture {
        let loader = self.clone();
        let m = specifier.clone();
        async move {
            let code = loader.get_and_update_source(&m, false).await?;
            Ok(Some(LoadResponse::Module {
                content: Arc::new(code),
                specifier: m,
                maybe_headers: None,
            }))
        }
        .boxed_local()
    }
}

fn get_module_type(m: &ModuleSpecifier) -> Result<ModuleType, AnyError> {
    let path = if let Ok(path) = m.to_file_path() {
        path
    } else {
        PathBuf::from(m.path())
    };
    match path.extension() {
        Some(ext) => {
            let lowercase_str = ext.to_str().map(|s| s.to_lowercase());
            match lowercase_str.as_deref() {
                Some("json") => Ok(ModuleType::Json),
                None => bail!("Unknown extension"),
                _ => Ok(ModuleType::JavaScript),
            }
        }
        None => bail!("Unknown media type {:?}", path),
    }
}
