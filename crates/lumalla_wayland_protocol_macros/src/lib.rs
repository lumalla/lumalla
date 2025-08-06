#![allow(dead_code)]

use anyhow::{Context, Result};
use proc_macro::TokenStream;
use quick_xml::de::from_str;
use quote::quote;
use schema::{Interface, Protocol};
use std::{fs, path::Path};
use syn::{LitStr, parse_macro_input};

mod schema;

fn parse_wayland_xml(xml_path: &str) -> Result<Protocol> {
    let xml_content = fs::read_to_string(xml_path)
        .with_context(|| format!("Failed to read XML file: {}", xml_path))?;

    let protocol: Protocol =
        from_str(&xml_content).map_err(|e| anyhow::anyhow!("Failed to parse XML file: {}", e))?;

    Ok(protocol)
}

fn rust_type_from_wayland_type(
    wayland_type: &str,
    _interface: Option<&str>,
    allow_null: bool,
) -> proc_macro2::TokenStream {
    let base_type = match wayland_type {
        "int" => quote! { i32 },
        "uint" => quote! { u32 },
        "fixed" => quote! { i32 },     // Wayland fixed-point number
        "string" => quote! { String }, // Use String for struct fields to avoid lifetime issues
        "object" => quote! { ObjectId },
        "new_id" => quote! { ObjectId },
        "array" => quote! { Vec<u8> },
        "fd" => quote! { std::os::unix::io::RawFd },
        _ => quote! { () }, // Unknown type
    };

    if allow_null {
        quote! { Option<#base_type> }
    } else {
        base_type
    }
}

fn rust_type_from_wayland_type_for_method(
    wayland_type: &str,
    _interface: Option<&str>,
    allow_null: bool,
) -> proc_macro2::TokenStream {
    let base_type = match wayland_type {
        "int" => quote! { i32 },
        "uint" => quote! { u32 },
        "fixed" => quote! { i32 },   // Wayland fixed-point number
        "string" => quote! { &str }, // Use &str for method parameters
        "object" => quote! { ObjectId },
        "new_id" => quote! { ObjectId },
        "array" => quote! { &[u8] },
        "fd" => quote! { std::os::unix::io::RawFd },
        _ => quote! { () }, // Unknown type
    };

    if allow_null {
        quote! { Option<#base_type> }
    } else {
        base_type
    }
}

fn escape_rust_keyword(name: &str) -> String {
    match name {
        "move" | "type" | "ref" | "box" | "impl" | "trait" | "struct" | "enum" | "fn" | "let"
        | "mut" | "const" | "static" | "if" | "else" | "while" | "for" | "loop" | "match"
        | "where" | "use" | "mod" | "pub" | "return" | "break" | "continue" => {
            format!("{}_", name)
        }
        _ => name.to_string(),
    }
}

fn snake_to_pascal_case(s: &str) -> String {
    let escaped = escape_rust_keyword(s);
    escaped
        .split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => {
                    first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                }
            }
        })
        .collect()
}

/// Generate Wayland protocol structs from an XML file
#[proc_macro]
pub fn wayland_protocol(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as LitStr);
    let xml_path = input.value();

    // Make path relative to CARGO_MANIFEST_DIR if it's not absolute
    let xml_path = if Path::new(&xml_path).is_absolute() {
        xml_path
    } else {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
        format!("{}/{}", manifest_dir, xml_path)
    };

    let protocol = match parse_wayland_xml(&xml_path) {
        Ok(protocol) => protocol,
        Err(e) => {
            return syn::Error::new(
                proc_macro2::Span::call_site(),
                format!("Failed to parse Wayland XML: {}", e),
            )
            .to_compile_error()
            .into();
        }
    };

    let mut all_writer_methods = Vec::new();
    let mut all_builder_structs = Vec::new();

    // Collect all interface data first
    let interface_data: Vec<_> = protocol
        .interface
        .into_iter()
        .map(|interface| {
            let has_requests = !interface.request.as_ref().unwrap_or(&Vec::new()).is_empty();
            let trait_name = if has_requests {
                Some(snake_to_pascal_case(&interface.name))
            } else {
                None
            };
            let result = generate_interface_code_parts(interface);
            (result, trait_name)
        })
        .collect();

    // Extract interface trait names and code parts
    let mut interface_trait_names = Vec::new();
    let mut interface_codes = Vec::new();

    for ((interface_code, writer_methods, builder_structs), trait_name) in interface_data {
        interface_codes.push(interface_code);
        all_writer_methods.extend(writer_methods);
        all_builder_structs.extend(builder_structs);

        if let Some(name) = trait_name {
            interface_trait_names.push(name);
        }
    }

    // Generate protocol supertrait
    let protocol_trait_name = syn::Ident::new(
        &format!("{}Protocol", snake_to_pascal_case(&protocol.name)),
        proc_macro2::Span::call_site(),
    );

    let interface_bounds = interface_trait_names.iter().map(|name| {
        let trait_ident = syn::Ident::new(name, proc_macro2::Span::call_site());
        quote! { #trait_ident }
    });

    let protocol_supertrait = if !interface_trait_names.is_empty() {
        quote! {
            /// Supertrait combining all interfaces in this protocol
            pub trait #protocol_trait_name: #(#interface_bounds)+* {}
        }
    } else {
        quote! {}
    };

    // Generate single Writer impl block
    let writer_impl = if !all_writer_methods.is_empty() {
        quote! {
            impl Writer {
                #(#all_writer_methods)*
            }
        }
    } else {
        quote! {}
    };

    let expanded = quote! {
        use anyhow::Context;
        use crate::{
            ObjectId,
            buffer::{MessageHeader, Writer},
            client::Ctx,
        };

        #(#interface_codes)*

        #protocol_supertrait

        // Builder structs
        #(#all_builder_structs)*

        // Writer methods
        #writer_impl
    };

    TokenStream::from(expanded)
}

fn generate_interface_code_parts(
    interface: Interface,
) -> (
    proc_macro2::TokenStream,
    Vec<proc_macro2::TokenStream>,
    Vec<proc_macro2::TokenStream>,
) {
    let interface_name = syn::Ident::new(
        &snake_to_pascal_case(&interface.name),
        proc_macro2::Span::call_site(),
    );

    // Clone interface_enum to avoid borrow conflicts
    let interface_enums = interface.interface_enum.clone().unwrap_or_default();

    // Generate constants for enums
    let enum_constants = interface_enums.iter().flat_map(|enum_def| {
        let enum_prefix = format!(
            "{}_{}",
            interface.name.to_uppercase(),
            enum_def.name.to_uppercase()
        );
        enum_def
            .entry
            .iter()
            .map(move |entry| {
                let const_name = syn::Ident::new(
                    &format!("{}_{}", enum_prefix, entry.name.to_uppercase()),
                    proc_macro2::Span::call_site(),
                );
                let value = entry.value.parse::<u32>().unwrap_or(0);
                quote! { pub const #const_name: u32 = #value; }
            })
            .collect::<Vec<_>>()
    });

    // Generate parameter structs for requests
    let empty = Vec::new();
    let request_param_structs = interface
        .request
        .as_ref()
        .unwrap_or(&empty)
        .iter()
        .map(|request| {
            let struct_name = syn::Ident::new(
                &format!(
                    "{}{}",
                    snake_to_pascal_case(&interface.name),
                    snake_to_pascal_case(&request.name)
                ),
                proc_macro2::Span::call_site(),
            );

            let empty = Vec::new();
            let fields = request.arg.as_ref().unwrap_or(&empty).iter().map(|arg| {
                let field_name = syn::Ident::new(
                    &escape_rust_keyword(&arg.name),
                    proc_macro2::Span::call_site(),
                );
                let field_type = rust_type_from_wayland_type(
                    &arg.arg_type,
                    arg.interface.as_deref(),
                    arg.allow_null.unwrap_or(false),
                );
                quote! { pub #field_name: #field_type }
            });

            // Generate accessor methods for each field
            let accessor_methods = request.arg.as_ref().unwrap_or(&empty).iter().map(|arg| {
                let field_name = syn::Ident::new(
                    &escape_rust_keyword(&arg.name),
                    proc_macro2::Span::call_site(),
                );
                let method_name = syn::Ident::new(
                    &escape_rust_keyword(&arg.name),
                    proc_macro2::Span::call_site(),
                );
                let field_type = rust_type_from_wayland_type(
                    &arg.arg_type,
                    arg.interface.as_deref(),
                    arg.allow_null.unwrap_or(false),
                );

                // For Copy types, return by value; for non-Copy types, return by reference
                let return_type_and_value = match arg.arg_type.as_str() {
                    "string" => {
                        if arg.allow_null.unwrap_or(false) {
                            (quote! { Option<&str> }, quote! { self.#field_name.as_deref() })
                        } else {
                            (quote! { &str }, quote! { &self.#field_name })
                        }
                    }
                    "array" => {
                        if arg.allow_null.unwrap_or(false) {
                            (quote! { Option<&[u8]> }, quote! { self.#field_name.as_deref() })
                        } else {
                            (quote! { &[u8] }, quote! { &self.#field_name })
                        }
                    }
                    _ => {
                        // For numeric types and ObjectId, return by value (Copy types)
                        (field_type.clone(), quote! { self.#field_name })
                    }
                };

                let (return_type, return_value) = return_type_and_value;

                quote! {
                    pub fn #method_name(&self) -> #return_type {
                        #return_value
                    }
                }
            });

            quote! {
                #[derive(Debug)]
                pub struct #struct_name {
                    #(#fields,)*
                }

                impl #struct_name {
                    #(#accessor_methods)*
                }
            }
        });

    // Generate interface trait
    let empty = Vec::new();
    let trait_methods = interface
        .request
        .as_ref()
        .unwrap_or(&empty)
        .iter()
        .map(|request| {
            let method_name = syn::Ident::new(
                &escape_rust_keyword(&request.name),
                proc_macro2::Span::call_site(),
            );
            let param_type = syn::Ident::new(
                &format!(
                    "{}{}",
                    snake_to_pascal_case(&interface.name),
                    snake_to_pascal_case(&request.name)
                ),
                proc_macro2::Span::call_site(),
            );

            quote! {
                fn #method_name(&mut self, ctx: &Ctx, object_id: ObjectId, params: &#param_type);
            }
        });

    // Generate handle_request method
    let empty = Vec::new();
    let request_match_arms = interface
        .request
        .as_ref()
        .unwrap_or(&empty)
        .iter()
        .enumerate()
        .map(|(opcode, request)| {
            let method_name = syn::Ident::new(
                &escape_rust_keyword(&request.name),
                proc_macro2::Span::call_site(),
            );
            let param_type = syn::Ident::new(
                &format!(
                    "{}{}",
                    snake_to_pascal_case(&interface.name),
                    snake_to_pascal_case(&request.name)
                ),
                proc_macro2::Span::call_site(),
            );
            let opcode_lit = (opcode + 1) as u16; // Opcodes start from 1

            quote! {
                #opcode_lit => self.#method_name(ctx, header.object_id, unsafe {
                    &*(data.as_ptr() as *const #param_type)
                }),
            }
        });

    let interface_trait = if !interface.request.as_ref().unwrap_or(&Vec::new()).is_empty() {
        // Check if this interface has an error enum with invalid_method entry
        let has_error_enum = interface_enums.iter().any(|e| {
            e.name == "error" && e.entry.iter().any(|entry| entry.name == "invalid_method")
        });

        let error_handling = if has_error_enum {
            let error_constant = syn::Ident::new(
                &format!("{}_ERROR_INVALID_METHOD", interface.name.to_uppercase()),
                proc_macro2::Span::call_site(),
            );
            quote! {
                ctx.writer
                    .wl_display_error(header.object_id)?
                    .object_id(header.object_id)
                    .code(#error_constant)
                    .message("Invalid method");
            }
        } else {
            quote! {
                // No error enum defined for this interface
            }
        };

        quote! {
            pub trait #interface_name {
                #(#trait_methods)*

                fn handle_request(
                    &mut self,
                    ctx: &mut Ctx,
                    header: &MessageHeader,
                    data: &[u8],
                ) -> anyhow::Result<()> {
                    match header.opcode {
                        #(#request_match_arms)*
                        _ => {
                            #error_handling
                            anyhow::bail!("Invalid method");
                        }
                    }

                    Ok(())
                }
            }
        }
    } else {
        quote! {}
    };

    // Generate Writer methods and collect builder structs for events
    let empty = Vec::new();
    let events = interface.event.as_ref().unwrap_or(&empty);

    let mut writer_methods = Vec::new();
    let mut builder_structs = Vec::new();

    for (opcode, event) in events.iter().enumerate() {
        let method_name = syn::Ident::new(
            &format!("{}_{}", interface.name, event.name),
            proc_macro2::Span::call_site(),
        );
        let opcode_lit = opcode as u16;

        let empty = Vec::new();
        let args = event.arg.as_ref().unwrap_or(&empty);

        if args.is_empty() {
            // Simple case: no arguments
            writer_methods.push(quote! {
                pub fn #method_name(&mut self, object_id: ObjectId) -> anyhow::Result<()> {
                    self.start_message(object_id, #opcode_lit)
                        .context("Failed to start message")?;
                    self.write_message_length();
                    Ok(())
                }
            });
        } else {
            // Generate builder pattern for events with arguments
            let first_builder_name = syn::Ident::new(
                &format!(
                    "{}{}{}",
                    snake_to_pascal_case(&interface.name),
                    snake_to_pascal_case(&event.name),
                    snake_to_pascal_case(&args[0].name)
                ),
                proc_macro2::Span::call_site(),
            );

            writer_methods.push(quote! {
                pub fn #method_name(
                    &mut self,
                    object_id: ObjectId,
                ) -> anyhow::Result<#first_builder_name<'_>> {
                    self.start_message(object_id, #opcode_lit)
                        .context("Failed to start message")?;
                    Ok(#first_builder_name { writer: self })
                }
            });

            // Generate builder structs and their impls
            for (i, arg) in args.iter().enumerate() {
                let current_builder_name = syn::Ident::new(
                    &format!(
                        "{}{}{}",
                        snake_to_pascal_case(&interface.name),
                        snake_to_pascal_case(&event.name),
                        snake_to_pascal_case(&arg.name)
                    ),
                    proc_macro2::Span::call_site(),
                );

                let arg_name = syn::Ident::new(
                    &escape_rust_keyword(&arg.name),
                    proc_macro2::Span::call_site(),
                );
                let arg_type = rust_type_from_wayland_type_for_method(
                    &arg.arg_type,
                    arg.interface.as_deref(),
                    arg.allow_null.unwrap_or(false),
                );

                let (write_method, param_conversion) = match arg.arg_type.as_str() {
                    "uint" => (quote! { write_u32 }, quote! { #arg_name }),
                    "int" => (quote! { write_i32 }, quote! { #arg_name }),
                    "string" => {
                        if arg.allow_null.unwrap_or(false) {
                            (quote! { write_str }, quote! { #arg_name.unwrap_or("") })
                        } else {
                            (quote! { write_str }, quote! { #arg_name })
                        }
                    }
                    "object" | "new_id" => {
                        if arg.allow_null.unwrap_or(false) {
                            (quote! { write_u32 }, quote! { #arg_name.unwrap_or(0) })
                        } else {
                            (quote! { write_u32 }, quote! { #arg_name })
                        }
                    }
                    "array" => (quote! { write_u32 }, quote! { #arg_name.len() as u32 }), // Write array length for now
                    "fd" => (quote! { write_u32 }, quote! { #arg_name as u32 }), // Cast fd to u32 for now
                    _ => (quote! { write_u32 }, quote! { 0u32 }),                // Default fallback
                };

                let builder_struct_and_impl = if i == args.len() - 1 {
                    // Last builder - no return type, just finish
                    quote! {
                        pub struct #current_builder_name<'client> {
                            writer: &'client mut Writer,
                        }

                        impl<'client> #current_builder_name<'client> {
                            pub fn #arg_name(self, #arg_name: #arg_type) {
                                self.writer.#write_method(#param_conversion);
                                self.writer.write_message_length();
                            }
                        }
                    }
                } else {
                    // Intermediate builder - return next builder
                    let next_arg = &args[i + 1];
                    let next_builder_name = syn::Ident::new(
                        &format!(
                            "{}{}{}",
                            snake_to_pascal_case(&interface.name),
                            snake_to_pascal_case(&event.name),
                            snake_to_pascal_case(&next_arg.name)
                        ),
                        proc_macro2::Span::call_site(),
                    );

                    quote! {
                        pub struct #current_builder_name<'client> {
                            writer: &'client mut Writer,
                        }

                        impl<'client> #current_builder_name<'client> {
                            pub fn #arg_name(self, #arg_name: #arg_type) -> #next_builder_name<'client> {
                                self.writer.#write_method(#param_conversion);
                                #next_builder_name {
                                    writer: self.writer,
                                }
                            }
                        }
                    }
                };

                builder_structs.push(builder_struct_and_impl);
            }
        }
    }

    let interface_code = quote! {
        // Constants
        #(#enum_constants)*

        // Parameter structs
        #(#request_param_structs)*

        // Interface trait
        #interface_trait
    };

    (interface_code, writer_methods, builder_structs)
}
