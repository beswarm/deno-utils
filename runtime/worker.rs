// Copyright 2018-2022 the Deno authors. All rights reserved. MIT license.

use crate::inspector_server::InspectorServer;
use crate::js;
use crate::ops;
use crate::ops::io::Stdio;
use crate::permissions::Permissions;
use crate::BootstrapOptions;
use deno_broadcast_channel::InMemoryBroadcastChannel;
use deno_core::error::AnyError;
use deno_core::error::JsError;
use deno_core::futures::Future;
use deno_core::located_script_name;
use deno_core::resolve_url_or_path;
use deno_core::CompiledWasmModuleStore;
use deno_core::Extension;
use deno_core::GetErrorClassFn;
use deno_core::JsErrorCreateFn;
use deno_core::JsRuntime;
use deno_core::LocalInspectorSession;
use deno_core::ModuleId;
use deno_core::ModuleLoader;
use deno_core::ModuleSpecifier;
use deno_core::RuntimeOptions;
use deno_core::SharedArrayBufferStore;
// use deno_core::SourceMapGetter;
use deno_tls::rustls::RootCertStore;
use deno_web::BlobStore;
use log::debug;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

use crate::StartSnapshot;
use derive_builder::Builder;

pub type FormatJsErrorFn = dyn Fn(&JsError) -> String + Sync + Send;

/// This worker is created and used by almost all
/// subcommands in Deno executable.
///
/// It provides ops available in the `Deno` namespace.
///
/// All `WebWorker`s created during program execution
/// are descendants of this worker.
pub struct MainWorker {
    pub js_runtime: JsRuntime,
    main_module: Option<ModuleSpecifier>,
    should_break_on_first_statement: bool,
}

pub type RuntimeOptionsCallback = Rc<dyn Fn(RuntimeOptions) -> RuntimeOptions>;

#[derive(Builder, Clone)]
#[builder(default, pattern = "owned")]
pub struct WorkerOptions {
    pub bootstrap: BootstrapOptions,
    // pub extensions: Vec<Extension>,
    pub unsafely_ignore_certificate_errors: Option<Vec<String>>,
    pub root_cert_store: Option<RootCertStore>,
    pub user_agent: String,
    pub seed: Option<u64>,
    #[builder(default = "Rc::new(deno_core::FsModuleLoader)")]
    pub module_loader: Rc<dyn ModuleLoader>,
    // Callbacks invoked when creating new instance of WebWorker
    pub create_web_worker_cb: Arc<ops::worker_host::CreateWebWorkerCb>,
    pub web_worker_preload_module_cb: Arc<ops::worker_host::PreloadModuleCb>,
    pub format_js_error_fn: Option<Arc<FormatJsErrorFn>>,
    // pub source_map_getter: Option<Box<dyn SourceMapGetter>>,
    pub js_error_create_fn: Option<Rc<JsErrorCreateFn>>,
    pub maybe_inspector_server: Option<Arc<InspectorServer>>,
    pub should_break_on_first_statement: bool,
    pub get_error_class_fn: Option<GetErrorClassFn>,
    pub origin_storage_dir: Option<std::path::PathBuf>,
    pub blob_store: BlobStore,
    pub broadcast_channel: InMemoryBroadcastChannel,
    pub shared_array_buffer_store: Option<SharedArrayBufferStore>,
    pub compiled_wasm_module_store: Option<CompiledWasmModuleStore>,
    pub stdio: Stdio,

    #[builder(setter(custom))]
    pub main_module: Option<ModuleSpecifier>,
    pub permissions: Permissions,
    pub startup_snapshot: Option<StartSnapshot>,
    pub runtime_options_callback: Option<RuntimeOptionsCallback>,
}

impl WorkerOptionsBuilder {
    pub fn main_module(mut self, value: Option<impl AsRef<str>>) -> Self {
        self.main_module = Some(value.map(|v| resolve_url_or_path(v.as_ref()).unwrap()));
        self
    }
}

impl MainWorker {
    pub fn bootstrap_from_options(options: WorkerOptions, exts: Vec<Extension>) -> Self {
        let bootstrap_options = options.bootstrap.clone();
        let mut worker = Self::from_options(options, exts);
        worker.bootstrap(&bootstrap_options);
        worker
    }

    fn from_options(options: WorkerOptions, exts: Vec<Extension>) -> Self {
        let main_module = options.main_module;
        let permissions = options.permissions;
        // Permissions: many ops depend on this
        let unstable = options.bootstrap.unstable;
        let enable_testing_features = options.bootstrap.enable_testing_features;
        let perm_ext = Extension::builder()
            .state(move |state| {
                state.put::<Permissions>(permissions.clone());
                state.put(ops::UnstableChecker { unstable });
                state.put(ops::TestingFeaturesEnabled(enable_testing_features));
                Ok(())
            })
            .build();

        // Internal modules
        let mut extensions: Vec<Extension> = vec![
            // Web APIs
            deno_webidl::init(),
            deno_console::init(),
            deno_url::init(),
            deno_web::init::<Permissions>(
                options.blob_store.clone(),
                options.bootstrap.location.clone(),
            ),
            deno_fetch::init::<Permissions>(deno_fetch::Options {
                user_agent: options.user_agent.clone(),
                root_cert_store: options.root_cert_store.clone(),
                unsafely_ignore_certificate_errors: options
                    .unsafely_ignore_certificate_errors
                    .clone(),
                file_fetch_handler: Rc::new(deno_fetch::FsFetchHandler),
                ..Default::default()
            }),
            deno_websocket::init::<Permissions>(
                options.user_agent.clone(),
                options.root_cert_store.clone(),
                options.unsafely_ignore_certificate_errors.clone(),
            ),
            deno_webstorage::init(options.origin_storage_dir.clone()),
            deno_broadcast_channel::init(options.broadcast_channel.clone(), unstable),
            deno_crypto::init(options.seed),
            deno_webgpu::init(unstable),
            // ffi
            deno_ffi::init::<Permissions>(unstable),
            // Runtime ops
            // ops::runtime::init(main_module.clone()),
            ops::worker_host::init(
                options.create_web_worker_cb.clone(),
                options.web_worker_preload_module_cb.clone(),
                options.format_js_error_fn.clone(),
            ),
            ops::spawn::init(),
            ops::fs_events::init(),
            ops::fs::init(),
            ops::io::init(),
            ops::io::init_stdio(options.stdio),
            deno_tls::init(),
            deno_net::init::<Permissions>(
                options.root_cert_store.clone(),
                unstable,
                options.unsafely_ignore_certificate_errors.clone(),
            ),
            ops::os::init(None),
            ops::permissions::init(),
            ops::process::init(),
            ops::signal::init(),
            ops::tty::init(),
            deno_http::init(),
            ops::http::init(),
            // Permissions ext (worker specific state)
            perm_ext,
        ];

        if let Some(v) = main_module.as_ref() {
            extensions.push(ops::runtime::init(v.clone()));
        }

        extensions.extend(exts);

        let snapshot = if let Some(v) = options.startup_snapshot {
            v.into()
        } else {
            js::deno_isolate_init()
        };

        let opts = RuntimeOptions {
            module_loader: Some(options.module_loader.clone()),
            startup_snapshot: Some(snapshot),
            // source_map_getter: options.source_map_getter,
            get_error_class_fn: options.get_error_class_fn,
            shared_array_buffer_store: options.shared_array_buffer_store.clone(),
            compiled_wasm_module_store: options.compiled_wasm_module_store.clone(),
            extensions,
            ..Default::default()
        };

        let opts = if let Some(cb) = options.runtime_options_callback {
            cb(opts)
        } else {
            opts
        };

        let mut js_runtime = JsRuntime::new(opts);

        if let Some(main) = main_module.as_ref() {
            if let Some(server) = options.maybe_inspector_server.clone() {
                server.register_inspector(
                    main.to_string(),
                    &mut js_runtime,
                    options.should_break_on_first_statement,
                );
            }
        }

        Self {
            js_runtime,
            main_module,
            should_break_on_first_statement: options.should_break_on_first_statement,
        }
    }

    pub fn bootstrap(&mut self, options: &BootstrapOptions) {
        let script = format!("bootstrap.mainRuntime({})", options.as_json());
        self.execute_script(&located_script_name!(), &script)
            .expect("Failed to execute bootstrap script");
    }

    /// See [JsRuntime::execute_script](deno_core::JsRuntime::execute_script)
    pub fn execute_script(&mut self, script_name: &str, source_code: &str) -> Result<(), AnyError> {
        self.js_runtime.execute_script(script_name, source_code)?;
        Ok(())
    }

    /// Loads and instantiates specified JavaScript module
    /// as "main" or "side" module.
    pub async fn preload_module(
        &mut self,
        module_specifier: &ModuleSpecifier,
        main: bool,
    ) -> Result<ModuleId, AnyError> {
        if main {
            self.js_runtime
                .load_main_module(module_specifier, None)
                .await
        } else {
            self.js_runtime
                .load_side_module(module_specifier, None)
                .await
        }
    }

    async fn evaluate_module(&mut self, id: ModuleId) -> Result<(), AnyError> {
        let mut receiver = self.js_runtime.mod_evaluate(id);
        tokio::select! {
          // Not using biased mode leads to non-determinism for relatively simple
          // programs.
          biased;

          maybe_result = &mut receiver => {
            debug!("received module evaluate {:#?}", maybe_result);
            maybe_result.expect("Module evaluation result not provided.")
          }

          event_loop_result = self.run_event_loop(false) => {
            event_loop_result?;
            let maybe_result = receiver.await;
            maybe_result.expect("Module evaluation result not provided.")
          }
        }
    }

    /// Loads, instantiates and executes specified JavaScript module.
    pub async fn execute_side_module(
        &mut self,
        module_specifier: &ModuleSpecifier,
    ) -> Result<(), AnyError> {
        let id = self.preload_module(module_specifier, false).await?;
        self.wait_for_inspector_session();
        self.evaluate_module(id).await
    }

    /// Loads, instantiates and executes specified JavaScript module.
    ///
    /// This module will have "import.meta.main" equal to true.
    pub async fn execute_main_module(&mut self) -> Result<(), AnyError> {
        // let id = self.preload_module(None).await?;
        let id = if let Some(v) = self.main_module.as_ref() {
            self.js_runtime.load_main_module(v, None).await?
        } else {
            self.js_runtime
                .load_main_module(
                    &resolve_url_or_path("main_module").unwrap(),
                    Some("await mainModule()".into()),
                )
                .await?
        };
        self.wait_for_inspector_session();
        self.evaluate_module(id).await
    }

    fn wait_for_inspector_session(&mut self) {
        if self.should_break_on_first_statement {
            self.js_runtime
                .inspector()
                .wait_for_session_and_break_on_next_statement()
        }
    }

    /// Create new inspector session. This function panics if Worker
    /// was not configured to create inspector.
    pub async fn create_inspector_session(&mut self) -> LocalInspectorSession {
        let inspector = self.js_runtime.inspector();
        inspector.create_local_session()
    }

    pub fn poll_event_loop(
        &mut self,
        cx: &mut Context,
        wait_for_inspector: bool,
    ) -> Poll<Result<(), AnyError>> {
        self.js_runtime.poll_event_loop(cx, wait_for_inspector)
    }

    pub async fn run_event_loop(&mut self, wait_for_inspector: bool) -> Result<(), AnyError> {
        self.js_runtime.run_event_loop(wait_for_inspector).await
    }

    /// A utility function that runs provided future concurrently with the event loop.
    ///
    /// Useful when using a local inspector session.
    pub async fn with_event_loop<'a, T>(
        &mut self,
        mut fut: Pin<Box<dyn Future<Output = T> + 'a>>,
    ) -> T {
        loop {
            tokio::select! {
              result = &mut fut => {
                return result;
              }
              _ = self.run_event_loop(false) => {}
            };
        }
    }

    /// Return exit code set by the executed code (either in main worker
    /// or one of child web workers).
    pub fn get_exit_code(&mut self) -> i32 {
        let op_state_rc = self.js_runtime.op_state();
        let op_state = op_state_rc.borrow();
        let exit_code = op_state.borrow::<Arc<AtomicI32>>().load(Relaxed);
        exit_code
    }

    /// Dispatches "load" event to the JavaScript runtime.
    ///
    /// Does not poll event loop, and thus not await any of the "load" event handlers.
    pub fn dispatch_load_event(&mut self, script_name: &str) -> Result<(), AnyError> {
        self.execute_script(
            script_name,
            // NOTE(@bartlomieju): not using `globalThis` here, because user might delete
            // it. Instead we're using global `dispatchEvent` function which will
            // used a saved reference to global scope.
            "dispatchEvent(new Event('load'))",
        )
    }

    /// Dispatches "unload" event to the JavaScript runtime.
    ///
    /// Does not poll event loop, and thus not await any of the "unload" event handlers.
    pub fn dispatch_unload_event(&mut self, script_name: &str) -> Result<(), AnyError> {
        self.execute_script(
            script_name,
            // NOTE(@bartlomieju): not using `globalThis` here, because user might delete
            // it. Instead we're using global `dispatchEvent` function which will
            // used a saved reference to global scope.
            "dispatchEvent(new Event('unload'))",
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::test_util::*;

    #[tokio::test]
    async fn execute_mod_esm_imports_a() {
        let p = testdata_path("esm_imports_a.js");
        let mut worker = create_test_worker(p);
        let result = worker.execute_main_module().await;
        if let Err(err) = result {
            eprintln!("execute_mod err {:?}", err);
        }
        if let Err(e) = worker.run_event_loop(false).await {
            panic!("Future got unexpected error: {:?}", e);
        }
    }

    #[tokio::test]
    async fn execute_mod_circular() {
        let p = testdata_path("circular1.js");
        let mut worker = create_test_worker(p);
        let result = worker.execute_main_module().await;
        if let Err(err) = result {
            eprintln!("execute_mod err {:?}", err);
        }
        if let Err(e) = worker.run_event_loop(false).await {
            panic!("Future got unexpected error: {:?}", e);
        }
    }

    #[tokio::test]
    async fn execute_mod_resolve_error() {
        // "foo" is not a valid module specifier so this should return an error.
        let mut worker = create_test_worker("does-not-exist");
        let result = worker.execute_main_module().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn execute_mod_002_hello() {
        // This assumes cwd is project root (an assumption made throughout the
        // tests).
        let p = testdata_path("001_hello.js");
        let mut worker = create_test_worker(p);
        let result = worker.execute_main_module().await;
        assert!(result.is_ok());
    }
}
