// Copyright 2016 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use rustc_data_structures::sync::Lrc;
use std::sync::Arc;

use monomorphize::Instance;
use rustc::hir;
use rustc::hir::TransFnAttrFlags;
use rustc::hir::def_id::{CrateNum, DefId, LOCAL_CRATE, CRATE_DEF_INDEX};
use rustc::ich::Fingerprint;
use rustc::middle::exported_symbols::{SymbolExportLevel, ExportedSymbol, metadata_symbol_name};
use rustc::session::config;
use rustc::ty::{TyCtxt, SymbolName};
use rustc::ty::maps::Providers;
use rustc::ty::subst::Substs;
use rustc::util::nodemap::{FxHashMap, DefIdMap};
use rustc_allocator::ALLOCATOR_METHODS;
use rustc_data_structures::indexed_vec::IndexVec;
use std::collections::hash_map::Entry::*;

pub type ExportedSymbols = FxHashMap<
    CrateNum,
    Arc<Vec<(String, SymbolExportLevel)>>,
>;

pub fn threshold(tcx: TyCtxt) -> SymbolExportLevel {
    crates_export_threshold(&tcx.sess.crate_types.borrow())
}

fn crate_export_threshold(crate_type: config::CrateType) -> SymbolExportLevel {
    match crate_type {
        config::CrateTypeExecutable |
        config::CrateTypeStaticlib  |
        config::CrateTypeProcMacro  |
        config::CrateTypeCdylib     => SymbolExportLevel::C,
        config::CrateTypeRlib       |
        config::CrateTypeDylib      => SymbolExportLevel::Rust,
    }
}

pub fn crates_export_threshold(crate_types: &[config::CrateType])
                                      -> SymbolExportLevel {
    if crate_types.iter().any(|&crate_type| {
        crate_export_threshold(crate_type) == SymbolExportLevel::Rust
    }) {
        SymbolExportLevel::Rust
    } else {
        SymbolExportLevel::C
    }
}

fn reachable_non_generics_provider<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>,
                                             cnum: CrateNum)
                                             -> Lrc<DefIdMap<SymbolExportLevel>>
{
    assert_eq!(cnum, LOCAL_CRATE);

    if !tcx.sess.opts.output_types.should_trans() {
        return Lrc::new(DefIdMap())
    }

    // Check to see if this crate is a "special runtime crate". These
    // crates, implementation details of the standard library, typically
    // have a bunch of `pub extern` and `#[no_mangle]` functions as the
    // ABI between them. We don't want their symbols to have a `C`
    // export level, however, as they're just implementation details.
    // Down below we'll hardwire all of the symbols to the `Rust` export
    // level instead.
    let special_runtime_crate = tcx.is_panic_runtime(LOCAL_CRATE) ||
        tcx.is_compiler_builtins(LOCAL_CRATE);

    let mut reachable_non_generics: DefIdMap<_> = tcx.reachable_set(LOCAL_CRATE).0
        .iter()
        .filter_map(|&node_id| {
            // We want to ignore some FFI functions that are not exposed from
            // this crate. Reachable FFI functions can be lumped into two
            // categories:
            //
            // 1. Those that are included statically via a static library
            // 2. Those included otherwise (e.g. dynamically or via a framework)
            //
            // Although our LLVM module is not literally emitting code for the
            // statically included symbols, it's an export of our library which
            // needs to be passed on to the linker and encoded in the metadata.
            //
            // As a result, if this id is an FFI item (foreign item) then we only
            // let it through if it's included statically.
            match tcx.hir.get(node_id) {
                hir::map::NodeForeignItem(..) => {
                    let def_id = tcx.hir.local_def_id(node_id);
                    if tcx.is_statically_included_foreign_item(def_id) {
                        Some(def_id)
                    } else {
                        None
                    }
                }

                // Only consider nodes that actually have exported symbols.
                hir::map::NodeItem(&hir::Item {
                    node: hir::ItemStatic(..),
                    ..
                }) |
                hir::map::NodeItem(&hir::Item {
                    node: hir::ItemFn(..), ..
                }) |
                hir::map::NodeImplItem(&hir::ImplItem {
                    node: hir::ImplItemKind::Method(..),
                    ..
                }) => {
                    let def_id = tcx.hir.local_def_id(node_id);
                    let generics = tcx.generics_of(def_id);
                    if !generics.requires_monomorphization(tcx) &&
                        // Functions marked with #[inline] are only ever translated
                        // with "internal" linkage and are never exported.
                        !Instance::mono(tcx, def_id).def.requires_local(tcx) {
                        Some(def_id)
                    } else {
                        None
                    }
                }

                _ => None
            }
        })
        .map(|def_id| {
            let export_level = if special_runtime_crate {
                let name = tcx.symbol_name(Instance::mono(tcx, def_id)).as_str();
                // We can probably do better here by just ensuring that
                // it has hidden visibility rather than public
                // visibility, as this is primarily here to ensure it's
                // not stripped during LTO.
                //
                // In general though we won't link right if these
                // symbols are stripped, and LTO currently strips them.
                if &*name == "rust_eh_personality" ||
                   &*name == "rust_eh_register_frames" ||
                   &*name == "rust_eh_unregister_frames" {
                    SymbolExportLevel::C
                } else {
                    SymbolExportLevel::Rust
                }
            } else {
                symbol_export_level(tcx, def_id)
            };
            debug!("EXPORTED SYMBOL (local): {} ({:?})",
                   tcx.symbol_name(Instance::mono(tcx, def_id)),
                   export_level);
            (def_id, export_level)
        })
        .collect();

    if let Some(id) = *tcx.sess.derive_registrar_fn.get() {
        let def_id = tcx.hir.local_def_id(id);
        reachable_non_generics.insert(def_id, SymbolExportLevel::C);
    }

    if let Some(id) = *tcx.sess.plugin_registrar_fn.get() {
        let def_id = tcx.hir.local_def_id(id);
        reachable_non_generics.insert(def_id, SymbolExportLevel::C);
    }

    Lrc::new(reachable_non_generics)
}

fn is_reachable_non_generic_provider_local<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>,
                                                     def_id: DefId)
                                                     -> bool {
    let export_threshold = threshold(tcx);

    if let Some(&level) = tcx.reachable_non_generics(def_id.krate).get(&def_id) {
        level.is_below_threshold(export_threshold)
    } else {
        false
    }
}

fn is_reachable_non_generic_provider_extern<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>,
                                                      def_id: DefId)
                                                      -> bool {
    tcx.reachable_non_generics(def_id.krate).contains_key(&def_id)
}

fn exported_symbols_provider_local<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>,
                                             cnum: CrateNum)
                                             -> Arc<Vec<(ExportedSymbol<'tcx>,
                                                         SymbolExportLevel)>>
{
    assert_eq!(cnum, LOCAL_CRATE);

    if !tcx.sess.opts.output_types.should_trans() {
        return Arc::new(vec![])
    }

    let mut symbols: Vec<_> = tcx.reachable_non_generics(LOCAL_CRATE)
                                 .iter()
                                 .map(|(&def_id, &level)| {
                                    (ExportedSymbol::NonGeneric(def_id), level)
                                 })
                                 .collect();

    if let Some(_) = *tcx.sess.entry_fn.borrow() {
        let symbol_name = "main".to_string();
        let exported_symbol = ExportedSymbol::NoDefId(SymbolName::new(&symbol_name));

        symbols.push((exported_symbol, SymbolExportLevel::C));
    }

    if tcx.sess.allocator_kind.get().is_some() {
        for method in ALLOCATOR_METHODS {
            let symbol_name = format!("__rust_{}", method.name);
            let exported_symbol = ExportedSymbol::NoDefId(SymbolName::new(&symbol_name));

            symbols.push((exported_symbol, SymbolExportLevel::Rust));
        }
    }

    if tcx.sess.opts.debugging_opts.pgo_gen.is_some() {
        // These are weak symbols that point to the profile version and the
        // profile name, which need to be treated as exported so LTO doesn't nix
        // them.
        const PROFILER_WEAK_SYMBOLS: [&'static str; 2] = [
            "__llvm_profile_raw_version",
            "__llvm_profile_filename",
        ];
        for sym in &PROFILER_WEAK_SYMBOLS {
            let exported_symbol = ExportedSymbol::NoDefId(SymbolName::new(sym));
            symbols.push((exported_symbol, SymbolExportLevel::C));
        }
    }

    if tcx.sess.crate_types.borrow().contains(&config::CrateTypeDylib) {
        let symbol_name = metadata_symbol_name(tcx);
        let exported_symbol = ExportedSymbol::NoDefId(SymbolName::new(&symbol_name));

        symbols.push((exported_symbol, SymbolExportLevel::Rust));
    }

    if tcx.share_generics() && tcx.local_crate_exports_generics() {
        use rustc::mir::mono::{Linkage, Visibility, MonoItem};
        use rustc::ty::InstanceDef;

        // Normally, we require that shared monomorphizations are not hidden,
        // because if we want to re-use a monomorphization from a Rust dylib, it
        // needs to be exported.
        // However, on platforms that don't allow for Rust dylibs, having
        // external linkage is enough for monomorphization to be linked to.
        let need_visibility = tcx.sess.target.target.options.dynamic_linking &&
                              !tcx.sess.target.target.options.only_cdylib;

        let (_, cgus) = tcx.collect_and_partition_translation_items(LOCAL_CRATE);

        for (mono_item, &(linkage, visibility)) in cgus.iter()
                                                       .flat_map(|cgu| cgu.items().iter()) {
            if linkage != Linkage::External {
                // We can only re-use things with external linkage, otherwise
                // we'll get a linker error
                continue
            }

            if need_visibility && visibility == Visibility::Hidden {
                // If we potentially share things from Rust dylibs, they must
                // not be hidden
                continue
            }

            if let &MonoItem::Fn(Instance {
                def: InstanceDef::Item(def_id),
                substs,
            }) = mono_item {
                if substs.types().next().is_some() {
                    symbols.push((ExportedSymbol::Generic(def_id, substs),
                                  SymbolExportLevel::Rust));
                }
            }
        }
    }

    // Sort so we get a stable incr. comp. hash.
    symbols.sort_unstable_by(|&(ref symbol1, ..), &(ref symbol2, ..)| {
        symbol1.compare_stable(tcx, symbol2)
    });

    Arc::new(symbols)
}

fn upstream_monomorphizations_provider<'a, 'tcx>(
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    cnum: CrateNum)
    -> Lrc<DefIdMap<Lrc<FxHashMap<&'tcx Substs<'tcx>, CrateNum>>>>
{
    debug_assert!(cnum == LOCAL_CRATE);

    let cnums = tcx.all_crate_nums(LOCAL_CRATE);

    let mut instances = DefIdMap();

    let cnum_stable_ids: IndexVec<CrateNum, Fingerprint> = {
        let mut cnum_stable_ids = IndexVec::from_elem_n(Fingerprint::ZERO,
                                                        cnums.len() + 1);

        for &cnum in cnums.iter() {
            cnum_stable_ids[cnum] = tcx.def_path_hash(DefId {
                krate: cnum,
                index: CRATE_DEF_INDEX,
            }).0;
        }

        cnum_stable_ids
    };

    for &cnum in cnums.iter() {
        for &(ref exported_symbol, _) in tcx.exported_symbols(cnum).iter() {
            if let &ExportedSymbol::Generic(def_id, substs) = exported_symbol {
                let substs_map = instances.entry(def_id)
                                          .or_insert_with(|| FxHashMap());

                match substs_map.entry(substs) {
                    Occupied(mut e) => {
                        // If there are multiple monomorphizations available,
                        // we select one deterministically.
                        let other_cnum = *e.get();
                        if cnum_stable_ids[other_cnum] > cnum_stable_ids[cnum] {
                            e.insert(cnum);
                        }
                    }
                    Vacant(e) => {
                        e.insert(cnum);
                    }
                }
            }
        }
    }

    Lrc::new(instances.into_iter()
                      .map(|(key, value)| (key, Lrc::new(value)))
                      .collect())
}

fn upstream_monomorphizations_for_provider<'a, 'tcx>(
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    def_id: DefId)
    -> Option<Lrc<FxHashMap<&'tcx Substs<'tcx>, CrateNum>>>
{
    debug_assert!(!def_id.is_local());
    tcx.upstream_monomorphizations(LOCAL_CRATE)
       .get(&def_id)
       .cloned()
}

fn is_unreachable_local_definition_provider(tcx: TyCtxt, def_id: DefId) -> bool {
    if let Some(node_id) = tcx.hir.as_local_node_id(def_id) {
        !tcx.reachable_set(LOCAL_CRATE).0.contains(&node_id)
    } else {
        bug!("is_unreachable_local_definition called with non-local DefId: {:?}",
              def_id)
    }
}

pub fn provide(providers: &mut Providers) {
    providers.reachable_non_generics = reachable_non_generics_provider;
    providers.is_reachable_non_generic = is_reachable_non_generic_provider_local;
    providers.exported_symbols = exported_symbols_provider_local;
    providers.upstream_monomorphizations = upstream_monomorphizations_provider;
    providers.is_unreachable_local_definition = is_unreachable_local_definition_provider;
}

pub fn provide_extern(providers: &mut Providers) {
    providers.is_reachable_non_generic = is_reachable_non_generic_provider_extern;
    providers.upstream_monomorphizations_for = upstream_monomorphizations_for_provider;
}

fn symbol_export_level(tcx: TyCtxt, sym_def_id: DefId) -> SymbolExportLevel {
    // We export anything that's not mangled at the "C" layer as it probably has
    // to do with ABI concerns. We do not, however, apply such treatment to
    // special symbols in the standard library for various plumbing between
    // core/std/allocators/etc. For example symbols used to hook up allocation
    // are not considered for export
    let trans_fn_attrs = tcx.trans_fn_attrs(sym_def_id);
    let is_extern = trans_fn_attrs.contains_extern_indicator();
    let std_internal = trans_fn_attrs.flags.contains(TransFnAttrFlags::RUSTC_STD_INTERNAL_SYMBOL);

    if is_extern && !std_internal {
        SymbolExportLevel::C
    } else {
        SymbolExportLevel::Rust
    }
}
