use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, Data, DeriveInput, Expr, ExprLit, ExprMethodCall, Fields, Lit};

/// A procedural macro that adds serde rename/alias attributes to struct fields
/// with support for multiple case conversions simultaneously.
///
/// # Examples
///
/// ```rust
/// use jellyswarrm_macros::multi_case_struct;
/// use serde::{Serialize, Deserialize};
///
/// // PascalCase and camelCase support
/// #[multi_case_struct(pascal, camel)]
/// #[derive(Debug, Serialize, Deserialize, Clone)]
/// pub struct PlaybackRequest {
///     pub always_burn_in_subtitle_when_transcoding: Option<bool>,
///     // Generates: #[serde(rename = "AlwaysBurnInSubtitleWhenTranscoding", alias = "alwaysBurnInSubtitleWhenTranscoding")]
///     
///     pub user_id: String,
///     // Generates: #[serde(rename = "UserId", alias = "userId")]
///     
///     #[serde(flatten)]
///     pub extra: std::collections::HashMap<String, serde_json::Value>,
///     // Preserves existing serde attributes
/// }
/// ```
///
/// Supported cases: `pascal`, `camel`, `snake`, `kebab`, `screaming`
#[proc_macro_attribute]
pub fn multi_case_struct(args: TokenStream, input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);

    // Parse the case types from arguments
    let case_types = if args.is_empty() {
        vec![CaseType::Pascal]
    } else {
        parse_case_types_from_tokens(args)
    };

    let name = &input.ident;
    let vis = &input.vis;
    let attrs = &input.attrs;
    let generics = &input.generics;

    if let Data::Struct(data_struct) = &input.data {
        if let Fields::Named(fields_named) = &data_struct.fields {
            let fields: Vec<_> = fields_named
                .named
                .iter()
                .map(|field| {
                    let field_name = &field.ident;
                    let field_type = &field.ty;
                    let field_vis = &field.vis;
                    let mut field_attrs = field.attrs.clone();

                    // Check if field already has serde rename or alias attributes (syn v2.0 API)
                    let has_serde_rename_or_alias = field_attrs.iter().any(|attr| {
                        if attr.path().is_ident("serde") {
                            if let syn::Meta::List(meta_list) = &attr.meta {
                                let tokens = meta_list.tokens.to_string();
                                tokens.contains("rename") || tokens.contains("alias")
                            } else {
                                false
                            }
                        } else {
                            false
                        }
                    });

                    // Always add rename/alias unless rename/alias is already present
                    if !has_serde_rename_or_alias {
                        if let Some(field_name) = field_name {
                            let field_name_str = field_name.to_string();

                            if case_types.len() == 1 {
                                // Single case - use rename
                                let converted_name = convert_case(&field_name_str, &case_types[0]);
                                let serde_attr: syn::Attribute = syn::parse_quote! {
                                    #[serde(rename = #converted_name)]
                                };
                                field_attrs.push(serde_attr);
                            } else {
                                // Multiple cases - use rename for first, alias for others
                                let primary_name = convert_case(&field_name_str, &case_types[0]);
                                let alias_names: Vec<String> = case_types
                                    .iter()
                                    .skip(1)
                                    .map(|case_type| convert_case(&field_name_str, case_type))
                                    .collect();

                                let serde_attr: syn::Attribute = syn::parse_quote! {
                                    #[serde(rename = #primary_name, alias = #(#alias_names),*)]
                                };
                                field_attrs.push(serde_attr);
                            }
                        }
                    }

                    quote! {
                        #(#field_attrs)*
                        #field_vis #field_name: #field_type
                    }
                })
                .collect();

            let expanded = quote! {
                #(#attrs)*
                #vis struct #name #generics {
                    #(#fields),*
                }
            };

            return TokenStream::from(expanded);
        }
    }

    // If not a named struct, return original
    TokenStream::from(quote! { #input })
}

#[derive(Debug, Clone)]
enum CaseType {
    Pascal,
    Camel,
    Snake,
    Kebab,
    ScreamingSnake,
}

fn parse_case_types_from_tokens(args: TokenStream) -> Vec<CaseType> {
    let mut case_types = Vec::new();
    let args_str = args.to_string();

    // Simple parsing of comma-separated identifiers
    for arg in args_str.split(',') {
        let arg = arg.trim();
        if let Some(case_type) = str_to_case_type(arg) {
            case_types.push(case_type);
        }
    }

    if case_types.is_empty() {
        vec![CaseType::Pascal]
    } else {
        case_types
    }
}

fn str_to_case_type(s: &str) -> Option<CaseType> {
    match s {
        "pascal" | "PascalCase" => Some(CaseType::Pascal),
        "camel" | "camelCase" => Some(CaseType::Camel),
        "snake" | "snake_case" => Some(CaseType::Snake),
        "kebab" | "kebab-case" => Some(CaseType::Kebab),
        "screaming" | "SCREAMING_SNAKE_CASE" => Some(CaseType::ScreamingSnake),
        _ => None,
    }
}

fn convert_case(input: &str, case_type: &CaseType) -> String {
    match case_type {
        CaseType::Pascal => snake_to_pascal_case(input),
        CaseType::Camel => snake_to_camel_case(input),
        CaseType::Snake => input.to_string(),
        CaseType::Kebab => snake_to_kebab_case(input),
        CaseType::ScreamingSnake => input.to_uppercase(),
    }
}

fn snake_to_pascal_case(input: &str) -> String {
    let mut result = String::new();
    let mut capitalize_next = true;

    for ch in input.chars() {
        if ch == '_' {
            capitalize_next = true;
        } else if capitalize_next {
            result.push(ch.to_ascii_uppercase());
            capitalize_next = false;
        } else {
            result.push(ch);
        }
    }
    result
}

fn snake_to_camel_case(input: &str) -> String {
    let pascal = snake_to_pascal_case(input);
    if let Some(first_char) = pascal.chars().next() {
        first_char.to_ascii_lowercase().to_string() + &pascal[1..]
    } else {
        pascal
    }
}

fn snake_to_kebab_case(input: &str) -> String {
    input.replace('_', "-")
}

#[proc_macro]
pub fn lowercase_routes(input: TokenStream) -> TokenStream {
    let mut expr = parse_macro_input!(input as Expr);
    process_routes(&mut expr);

    // Extract routes after processing (so we get both original and lowercase)
    let routes = extract_routes(&expr);
    let log_stmts = routes.iter().map(|r| {
        quote! {
            tracing::debug!("Registered route: {}", #r);
        }
    });

    TokenStream::from(quote! {
        {
            #(#log_stmts)*
            #expr
        }
    })
}

fn process_routes(expr: &mut Expr) {
    if let Expr::MethodCall(method_call) = expr {
        // Process receiver first
        process_routes(&mut method_call.receiver);

        let method_name = method_call.method.to_string();

        // If nest, process the inner router
        if method_name == "nest" {
            if let Some(arg) = method_call.args.get_mut(1) {
                process_routes(arg);
            }
        }

        if method_name == "route" || method_name == "nest" {
            let should_duplicate = if let Some(Expr::Lit(ExprLit {
                lit: Lit::Str(lit_str),
                ..
            })) = method_call.args.first()
            {
                let path = lit_str.value();
                path != path.to_lowercase()
            } else {
                false
            };

            if should_duplicate {
                let original_call = method_call.clone();

                // Create args for the lowercase call
                let mut new_args = original_call.args.clone();
                if let Some(Expr::Lit(ExprLit {
                    lit: Lit::Str(lit_str),
                    ..
                })) = new_args.first_mut()
                {
                    let path = lit_str.value();
                    let lower_path = path.to_lowercase();
                    *lit_str = syn::LitStr::new(&lower_path, lit_str.span());
                }

                // Construct the new MethodCall
                // The receiver is the original method call (wrapped in Expr)
                let new_receiver = Box::new(Expr::MethodCall(original_call));

                *expr = Expr::MethodCall(ExprMethodCall {
                    attrs: vec![],
                    receiver: new_receiver,
                    dot_token: method_call.dot_token,
                    method: method_call.method.clone(),
                    turbofish: method_call.turbofish.clone(),
                    paren_token: method_call.paren_token,
                    args: new_args,
                });
            }
        }
    }
}

fn extract_routes(expr: &Expr) -> Vec<String> {
    if let Expr::MethodCall(method_call) = expr {
        // Collect from receiver (left side of chain)
        let mut routes = extract_routes(&method_call.receiver);

        let method_name = method_call.method.to_string();

        if method_name == "route" {
            // .route("/path", ...)
            if let Some(Expr::Lit(ExprLit {
                lit: Lit::Str(lit_str),
                ..
            })) = method_call.args.first()
            {
                routes.push(lit_str.value());
            }
        } else if method_name == "nest" {
            // .nest("/path", inner_router)
            if let Some(Expr::Lit(ExprLit {
                lit: Lit::Str(lit_str),
                ..
            })) = method_call.args.first()
            {
                let prefix = lit_str.value();

                if let Some(inner_expr) = method_call.args.get(1) {
                    let inner_routes = extract_routes(inner_expr);
                    for inner in inner_routes {
                        routes.push(format!("{}{}", prefix, inner));
                    }
                }
            }
        }

        return routes;
    }

    // Not a method call we track
    Vec::new()
}
