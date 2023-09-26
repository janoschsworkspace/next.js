use std::collections::HashMap;

use anyhow::{bail, Result};
use indexmap::IndexMap;
use turbo_tasks::{
    graph::{GraphTraversal, NonDeterministic},
    TryFlatJoinIterExt, Value, ValueToString, Vc,
};
use turbopack_binding::{
    swc::core::{
        css::modules::imports,
        ecma::{
            ast::{
                CallExpr, Callee, Expr, Ident, ImportDefaultSpecifier, Lit, Program, Prop,
                PropName, PropOrSpread,
            },
            visit::{Visit, VisitWith},
        },
    },
    turbo::tasks_fs::FileSystemPath,
    turbopack::{
        core::{
            chunk::{
                availability_info::{self, AvailabilityInfo},
                Chunk, ChunkData, ChunkableModule, ChunkingContext, ChunksData, EvaluatableAsset,
                EvaluatableAssets,
            },
            issue::{IssueSeverity, OptionIssueSource},
            module::Module,
            output::{OutputAsset, OutputAssets},
            reference::primary_referenced_modules,
            reference_type::EcmaScriptModulesReferenceSubType,
            resolve::{origin::PlainResolveOrigin, parse::Request, pattern::Pattern},
        },
        ecmascript::{
            chunk::{EcmascriptChunkPlaceable, EcmascriptChunkingContext},
            parse::ParseResult,
            resolve::esm_resolve,
            EcmascriptModuleAsset,
        },
        ecmascript_plugin::transform::directives::client,
    },
};

pub(crate) async fn create_react_lodable_manifest(
    entry: Vc<Box<dyn EcmascriptChunkPlaceable>>,
    chunking_context: Vc<Box<dyn ChunkingContext>>,
) -> Result<Vc<DynamicImportChunks>> {
    // Traverse referenced modules graph, collect all of the dynamic imports:
    // - Read the Program AST of the Module, this is the origin (A)
    //  - If there's `dynamic(import(B))`, then B is the module that is being
    //    imported
    // Returned import mappings are in the form of
    // Vec<(A, (B, Module<B>))> (where B is the raw import source string, and
    // Module<B> is the actual resolved Module)
    let imported_modules_mapping = NonDeterministic::new()
        .skip_duplicates()
        .visit([Vc::upcast(entry)], get_referenced_modules)
        .await
        .completed()?
        .into_inner()
        .into_iter()
        .map(|module| collect_dynamic_imports(module));

    let mut chunks_hash: HashMap<String, Vc<OutputAssets>> = HashMap::new();
    let mut import_mappings: IndexMap<Vc<String>, Vec<(String, Vc<OutputAssets>)>> =
        IndexMap::new();

    // Iterate over the collected import mappings, and create a chunk for each
    // dynamic import.
    for module_mapping in imported_modules_mapping {
        let imports_map = &*module_mapping.await?;
        if let Some(module_mapping) = imports_map {
            let (origin_module, dynamic_imports) = &*module_mapping.await?;
            for (imported_raw_str, imported_module) in dynamic_imports {
                let chunk = if let Some(chunk) = chunks_hash.get(imported_raw_str) {
                    chunk.clone()
                } else {
                    let Some(imported_module) = Vc::try_resolve_sidecast::<
                        Box<dyn EcmascriptChunkPlaceable>,
                    >(imported_module.clone())
                    .await?
                    else {
                        bail!("module must be evaluatable");
                    };

                    // [Note]: this seems to create a duplicated chunk for the same module to the original import() call
                    // and the explicit chunk we ask in here. So there'll be at least 2 chunks for
                    // the same module, relying on naive hash to have additonal
                    // chunks in case if there are same modules being imported in differnt origins.
                    let chunk = imported_module.as_chunk(
                        Vc::upcast(chunking_context),
                        Value::new(AvailabilityInfo::Root {
                            current_availability_root: Vc::upcast(entry),
                        }),
                    );
                    let chunk_group = chunking_context.chunk_group(chunk);
                    chunks_hash.insert(imported_raw_str.to_string(), chunk_group.clone());
                    chunk_group
                };

                import_mappings
                    .entry(origin_module.ident().path().to_string())
                    .or_insert_with(Vec::new)
                    .push((imported_raw_str.clone(), chunk));
            }
        }
    }

    Ok(Vc::cell(import_mappings))
}

async fn get_referenced_modules(
    parent: Vc<Box<dyn Module>>,
) -> Result<impl Iterator<Item = Vc<Box<dyn Module>>> + Send> {
    primary_referenced_modules(parent)
        .await
        .map(|modules| modules.clone_value().into_iter())
}

#[turbo_tasks::function]
async fn collect_dynamic_imports(
    module: Vc<Box<dyn Module>>,
) -> Result<Vc<OptionDynamicImportsMap>> {
    let Some(ecmascript_asset) =
        Vc::try_resolve_downcast_type::<EcmascriptModuleAsset>(module).await?
    else {
        return Ok(OptionDynamicImportsMap::none());
    };

    let ParseResult::Ok { program, .. } = &*ecmascript_asset.parse().await? else {
        bail!(
            "failed to parse module '{}'",
            &*module.ident().to_string().await?
        );
    };

    // Reading the Program AST, collect raw imported module str if it's wrapped in
    // dynamic()
    let mut visitor = LodableImportVisitor::new();
    program.visit_with(&mut visitor);

    if visitor.import_sources.is_empty() {
        return Ok(OptionDynamicImportsMap::none());
    }

    let mut import_sources = vec![];
    for import in visitor.import_sources.drain(..) {
        // Using the given `Module` which is the origin of the dynamic import, trying to
        // resolve the module that is being imported.
        let dynamic_imported_resolved_module = *esm_resolve(
            Vc::upcast(PlainResolveOrigin::new(
                ecmascript_asset.await?.asset_context,
                module.ident().path(),
            )),
            Request::parse(Value::new(Pattern::Constant(import.to_string()))),
            Value::new(EcmaScriptModulesReferenceSubType::Undefined),
            OptionIssueSource::none(),
            IssueSeverity::Error.cell(),
        )
        .first_module()
        .await?;

        if let Some(dynamic_imported_resolved_module) = dynamic_imported_resolved_module {
            import_sources.push((import, dynamic_imported_resolved_module));
        }
    }

    Ok(Vc::cell(Some(Vc::cell((module.clone(), import_sources)))))
}

struct LodableImportVisitor {
    //in_dynamic_call: bool,
    dynamic_ident: Option<Ident>,
    pub import_sources: Vec<String>,
}

impl LodableImportVisitor {
    fn new() -> Self {
        Self {
            //in_dynamic_call: false,
            import_sources: vec![],
            dynamic_ident: None,
        }
    }
}

struct CollectImportSourceVisitor {
    import_source: Option<String>,
}

impl CollectImportSourceVisitor {
    fn new() -> Self {
        Self {
            import_source: None,
        }
    }
}

impl Visit for CollectImportSourceVisitor {
    fn visit_call_expr(&mut self, call_expr: &CallExpr) {
        // find import source from import('path/to/module')
        // [NOTE]: Turbopack does not support webpack-specific comment directives, i.e
        // import(/* webpackChunkName: 'hello1' */ '../../components/hello3')
        // Renamed chunk in the comment will be ignored.
        if let Callee::Import(import) = call_expr.callee {
            if let Some(arg) = call_expr.args.first() {
                if let Expr::Lit(lit) = &*arg.expr {
                    if let Lit::Str(str_) = &lit {
                        self.import_source = Some(str_.value.to_string());
                    }
                }
            }
        }

        // Don't need to visit children, we expect import() won't have any
        // nested calls as dynamic() should be statically analyzable import.
    }
}

impl Visit for LodableImportVisitor {
    fn visit_import_decl(&mut self, decl: &turbopack_binding::swc::core::ecma::ast::ImportDecl) {
        // find import decl from next/dynamic, i.e import dynamic from 'next/dynamic'
        if decl.src.value == *"next/dynamic" {
            if let Some(specifier) = decl.specifiers.first().map(|s| s.as_default()).flatten() {
                self.dynamic_ident = Some(specifier.local.clone());
            }
        }
    }

    fn visit_call_expr(&mut self, call_expr: &CallExpr) {
        // Collect imports if the import call is wrapped in the call dynamic()
        if let Callee::Expr(ident) = &call_expr.callee {
            if let Expr::Ident(ident) = &**ident {
                if let Some(dynamic_ident) = &self.dynamic_ident {
                    if ident.sym == *dynamic_ident.sym {
                        let mut collect_import_source_visitor = CollectImportSourceVisitor::new();
                        call_expr.visit_children_with(&mut collect_import_source_visitor);

                        if let Some(import_source) = collect_import_source_visitor.import_source {
                            self.import_sources.push(import_source);
                        }
                    }
                }
            }
        }

        call_expr.visit_children_with(self);
    }
}

/// A struct contains mapping for the dynamic imports to construct chunk
/// (Origin Module, Vec<(ImportSourceString, Module)>)
#[turbo_tasks::value(transparent)]
pub struct DynamicImportsMap(pub (Vc<Box<dyn Module>>, Vec<(String, Vc<Box<dyn Module>>)>));

/// An Option wrapper around [DynamicImportsMap].
#[turbo_tasks::value(transparent)]
pub struct OptionDynamicImportsMap(Option<Vc<DynamicImportsMap>>);

#[turbo_tasks::value_impl]
impl OptionDynamicImportsMap {
    #[turbo_tasks::function]
    pub fn none() -> Vc<Self> {
        Vc::cell(None)
    }
}

#[turbo_tasks::value(transparent)]
pub struct DynamicImportChunks(pub IndexMap<Vc<String>, Vec<(String, Vc<OutputAssets>)>>);
