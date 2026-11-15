//! Generate a bridge using only Slint's public getters, setters and callbacks.
//! Keeping this derived from the compiled API means new library panels cannot
//! silently acquire a second, disconnected copy of their backend state.
use quote::{format_ident, quote};
use std::{collections::BTreeMap, env, fs, path::PathBuf};
use syn::{ImplItem, Item, Type, Visibility};

pub fn generate() {
    let dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let source = fs::read_to_string(dir.join("main.rs")).unwrap();
    let file = syn::parse_file(&source).expect("parse Slint public API");
    let items = match &file.items[0] {
        Item::Mod(m) => &m.content.as_ref().unwrap().1,
        _ => panic!("expected Slint module"),
    };
    let mut apis = BTreeMap::new();
    let mut globals = Vec::new();
    for item in items {
        let Item::Impl(i) = item else { continue };
        let Type::Path(t) = &*i.self_ty else { continue };
        let name = t.path.segments.last().unwrap().ident.to_string();
        if let Some((_, path, _)) = &i.trait_ {
            let text = quote!(#path).to_string();
            if text.contains("Global") && text.contains("AppWindow") {
                globals.push(name);
            }
            continue;
        }
        let methods: BTreeMap<_, _> = i
            .items
            .iter()
            .filter_map(|m| {
                let ImplItem::Fn(f) = m else { return None };
                matches!(f.vis, Visibility::Public(_))
                    .then(|| (f.sig.ident.to_string(), f.sig.clone()))
            })
            .collect();
        apis.insert(name, methods);
    }
    let root = apis
        .keys()
        .find(|n| n.trim_start_matches("r#") == "LibraryWindow")
        .unwrap()
        .clone();
    let main = apis
        .keys()
        .find(|n| n.trim_start_matches("r#") == "AppWindow")
        .unwrap()
        .clone();
    let mut pure_callbacks = std::collections::BTreeSet::new();
    for entry in fs::read_dir("ui").unwrap().flatten() {
        if entry.path().extension().is_some_and(|e| e == "slint") {
            let text = fs::read_to_string(entry.path()).unwrap();
            for line in text.lines() {
                if let Some(rest) = line.trim().strip_prefix("pure callback ") {
                    if let Some((name, _)) = rest.split_once('(') {
                        pure_callbacks.insert(name.replace('-', "_"));
                    }
                }
            }
        }
    }
    let mut setup = Vec::new();
    let mut callbacks = Vec::new();
    globals.push(root.clone());
    for name in globals {
        let methods = &apis[&name];
        let is_root = name == root;
        let owner_methods = if is_root { &apis[&main] } else { methods };
        let ty = format_ident!("{}", name.trim_start_matches("r#"));
        let owner = if is_root {
            quote!(owner)
        } else {
            quote!(owner.global::<crate::#ty>())
        };
        let view = if is_root {
            quote!(view)
        } else {
            quote!(view.global::<crate::#ty>())
        };
        for (method, sig) in methods {
            let ident = &sig.ident;
            if let Some(prop) = method.strip_prefix("get_") {
                let setter = format!("set_{prop}");
                if !methods.contains_key(&setter) || !owner_methods.contains_key(&setter) {
                    continue;
                }
                // Cross-window drag state belongs to the target window. In
                // particular DataTransfer is not an equatable UI value.
                if name.trim_start_matches("r#") == "TrackDrag"
                    && [
                        "cross_data",
                        "cross_x",
                        "cross_y",
                        "cross_over",
                        "cross_dropped",
                    ]
                    .contains(&prop)
                {
                    continue;
                }
                // Window geometry belongs to each window rather than the shared library.
                if name.trim_start_matches("r#") == "LibraryEdit"
                    && ["window_width", "window_height", "top_search"].contains(&prop)
                {
                    continue;
                }
                let set = format_ident!("{setter}");
                // Only the visible browser's viewport should request mini waveforms.
                if name.trim_start_matches("r#") == "ColumnUi"
                    && ["first_shown", "shown_count"].contains(&prop)
                {
                    setup.push(quote! {
                        {
                            let weak_owner = owner.as_weak();
                            let weak_view = view.as_weak();
                            sync.push(Box::new(move || {
                                if let (Some(owner), Some(view)) = (weak_owner.upgrade(), weak_view.upgrade()) {
                                    #owner.#set(#view.#ident());
                                }
                            }));
                        }
                    });
                    continue;
                }
                setup.push(quote! {
                    {
                        let mut previous = #owner.#ident();
                        #view.#set(previous.clone());
                        let weak_owner = owner.as_weak();
                        let weak_view = view.as_weak();
                        sync.push(Box::new(move || {
                            let (Some(owner), Some(view)) = (weak_owner.upgrade(), weak_view.upgrade()) else { return };
                            let from_owner = #owner.#ident();
                            let from_view = #view.#ident();
                            if from_owner != previous {
                                #view.#set(from_owner.clone());
                                previous = from_owner;
                            } else if from_view != previous {
                                #owner.#set(from_view.clone());
                                previous = from_view;
                            }
                        }));
                    }
                });
            } else if let Some(callback) = method.strip_prefix("on_") {
                let invoke = format!("invoke_{callback}");
                if !owner_methods.contains_key(&invoke) {
                    continue;
                }
                let invoke = format_ident!("{invoke}");
                // invoke_* gives the callback's argument count.
                let argc = owner_methods[&invoke.to_string()].inputs.len() - 1;
                let args: Vec<_> = (0..argc).map(|n| format_ident!("arg{n}")).collect();
                let synchronize_call = if pure_callbacks.contains(callback) {
                    quote!()
                } else {
                    quote!(synchronize();)
                };
                let synchronize_binding = if pure_callbacks.contains(callback) {
                    quote!()
                } else {
                    quote!(let synchronize = synchronize.clone();)
                };
                callbacks.push(quote! {
                    {
                        let weak_owner = owner.as_weak();
                        #synchronize_binding
                        #view.#ident(move |#(#args),*| {
                            #synchronize_call
                            let owner = weak_owner.upgrade().expect("library owner outlives its window");
                            let result = #owner.#invoke(#(#args),*);
                            #synchronize_call
                            result
                        });
                    }
                });
            }
        }
    }
    let output = quote! {
        #[allow(clippy::clone_on_copy, clippy::let_unit_value, clippy::let_and_return)]
        pub(super) fn bridge(owner: &crate::AppWindow, view: &crate::LibraryWindow) -> std::rc::Rc<dyn Fn()> {
            use slint::ComponentHandle;
            let mut sync: Vec<Box<dyn FnMut()>> = Vec::new();
            #(#setup)*
            let sync = std::cell::RefCell::new(sync);
            let synchronize: std::rc::Rc<dyn Fn()> = std::rc::Rc::new(move || {
                if let Ok(mut updates) = sync.try_borrow_mut() {
                    for update in updates.iter_mut() { update(); }
                }
            });
            #(#callbacks)*
            synchronize
        }
    };
    fs::write(dir.join("library_bridge.rs"), output.to_string()).unwrap();
}
