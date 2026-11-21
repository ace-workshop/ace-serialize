use crate::attr::{self, INVALID_POSITION};
use crate::field::{self, unwrap_group, FieldInfo, Kind, NAMED_FIELDS_ONLY};
use proc_macro2::{Literal, TokenStream};
use quote::{format_ident, quote, quote_spanned, ToTokens};
use std::collections::BTreeSet;
use syn::{
    parse_quote, punctuated::Punctuated, spanned::Spanned, Attribute, Data, DataEnum, DataStruct,
    DeriveInput, Expr, Fields, GenericArgument, Generics, Ident, Index, Lit, LitStr, Meta,
    PathArguments, Token, Type, UnOp, WherePredicate,
};

const LIFETIME_PARAMETER: &str = "ace: 不支持生命周期参数";
/// 被拒绝的 enum 的专用诊断（用户裁决 B，2026-11-13）；不得沿用 `NAMED_FIELDS_ONLY`。
const UNSUPPORTED_ENUM: &str = "ace: 仅支持无载荷整数枚举与具名字段结构体";
/// FR-T01 的 enum repr 白名单；其它表示（`repr(C)`、isize/usize、transparent、align 等）一律拒绝。
const UNSUPPORTED_REPR: &str =
    "ace: unsupported enum repr; integer enums allow only i8, i16, i32, i64, u8, u16, u32, u64";
/// FR-D07：`#[ace(default = Variant)]` 的变体名必须是本 enum 的变体。
const UNKNOWN_DEFAULT_VARIANT: &str = "ace: unknown default variant";
/// 用户裁决 A（2026-11-14）：未标 repr 的 wire 按 i32，判别值超出 i32 一律拒绝，不得静默截断。
const OVER_I32_DISCRIMINANT: &str =
    "ace: unmarked integer enum discriminant must fit in i32; add #[repr(i64)] or #[repr(u64)] to the enum";
/// FR-D05：多态枚举的每个变体必须显式声明数值 tag 或 type_name。
const POLY_VARIANT_ATTR: &str =
    "ace: polymorphic enum variant requires #[ace(tag = n)] or #[ace(type_name = \"...\")]";
/// FR-D05：同一 enum 不得混用数值 tag 与 type_name。
const POLY_MODE_MIXED: &str =
    "ace: polymorphic enum variants must all use #[ace(tag = n)] or all use #[ace(type_name = \"...\")]; the two modes cannot be mixed";
const POLY_BOTH: &str = "ace: variant cannot declare both tag and type_name";
const POLY_FIELD_SHAPE: &str =
    "ace: polymorphic enum variants support only unit variants or named fields";
/// 多态对象的共享/环不在本阶段（规格 r1 §二）；拒绝而不是留下下游类型错误。
const POLY_SHARED_FIELD: &str =
    "ace: polymorphic enum variants do not support Rc, Arc or Weak fields in this stage";
/// FR-W07：65535 专供显式 type_name 逃生口。
const POLY_ESCAPE_TAG: u16 = 65535;

pub(crate) fn expand(input: &DeriveInput) -> syn::Result<TokenStream> {
    match &input.data {
        Data::Struct(data) => expand_struct(input, data),
        Data::Enum(data) => expand_enum(input, data),
        _ => Err(syn::Error::new_spanned(&input.ident, NAMED_FIELDS_ONLY)),
    }
}

fn expand_struct(input: &DeriveInput, data: &DataStruct) -> syn::Result<TokenStream> {
    let Fields::Named(named) = &data.fields else {
        return Err(syn::Error::new_spanned(&input.ident, NAMED_FIELDS_ONLY));
    };
    if let Some(lifetime) = input.generics.lifetimes().next() {
        return Err(syn::Error::new_spanned(lifetime, LIFETIME_PARAMETER));
    }
    let attrs = attr::parse_type(&input.attrs)?;
    if let Some(default) = &attrs.default {
        return Err(syn::Error::new_spanned(default, INVALID_POSITION));
    }
    let reserved = attrs.reserved;
    let type_params: Vec<Ident> = input
        .generics
        .type_params()
        .map(|param| param.ident.clone())
        .collect();
    let fields = field::collect(&named.named, &type_params, &reserved)?;
    let mut wire: Vec<&FieldInfo> = fields
        .items
        .iter()
        .filter(|field| !matches!(field.kind, Kind::Skip))
        .collect();
    wire.sort_by_key(|field| field.order);
    let shape = Shape {
        name: &input.ident,
        generics: &input.generics,
        fields: &fields.items,
        wire,
        shared: fields.shared,
    };
    Ok(shape.emit())
}

/// 整数枚举（FR-T01、FR-D07）：全部变体无载荷、repr 在白名单内；未标 repr 时 wire 按 i32。
/// 出现变体级 `#[ace(...)]` 时转入多态枚举（FR-D05、FR-W07）。
fn expand_enum(input: &DeriveInput, data: &DataEnum) -> syn::Result<TokenStream> {
    if let Some(lifetime) = input.generics.lifetimes().next() {
        return Err(syn::Error::new_spanned(lifetime, LIFETIME_PARAMETER));
    }
    let attrs = attr::parse_type(&input.attrs)?;
    if !attrs.reserved.is_empty() {
        return Err(syn::Error::new_spanned(&input.ident, INVALID_POSITION));
    }
    let has_variant_attrs = data
        .variants
        .iter()
        .any(|variant| variant.attrs.iter().any(|attr| attr.path().is_ident("ace")));
    if has_variant_attrs {
        return expand_polymorphic_enum(input, data, &attrs);
    }
    let repr = enum_repr(&input.attrs)?;
    if data
        .variants
        .iter()
        .any(|variant| !matches!(variant.fields, Fields::Unit))
    {
        return Err(syn::Error::new_spanned(&input.ident, UNSUPPORTED_ENUM));
    }
    if !repr.explicit {
        reject_wide_discriminants(data)?;
    }
    let default = match &attrs.default {
        Some(default)
            if data
                .variants
                .iter()
                .any(|variant| variant.ident == *default) =>
        {
            Some(default)
        }
        Some(default) => return Err(syn::Error::new_spanned(default, UNKNOWN_DEFAULT_VARIANT)),
        None => None,
    };
    let variants: Vec<&Ident> = data.variants.iter().map(|variant| &variant.ident).collect();
    Ok(EnumShape {
        name: &input.ident,
        repr: &repr,
        default,
        variants,
    }
    .emit())
}

/// 多态枚举判别方式（FR-D05）：同一 enum 只能选一种。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PolyMode {
    Tag,
    TypeName,
}

/// 一个多态变体的判别信息与字段。
enum PolyDiscriminant {
    Tag(u16),
    TypeName(LitStr),
}

/// 变体字段形态：保留 `syn::Fields` 的 Unit 与 Named 区分。具名空变体 `Empty {}` 字段集合为空，
/// 但生成代码必须保留花括号；按字段数反推会把 `Empty {}` 误当成单元变体（复审 I2）。
enum PolyFields {
    Unit,
    Named(Vec<FieldInfo>),
}

impl PolyFields {
    fn is_unit(&self) -> bool {
        matches!(self, Self::Unit)
    }

    fn items(&self) -> &[FieldInfo] {
        match self {
            Self::Unit => &[],
            Self::Named(items) => items,
        }
    }
}

struct PolyVariant {
    ident: Ident,
    fields: PolyFields,
    discriminant: PolyDiscriminant,
}

impl PolyVariant {
    /// 打开多态对象（FR-W07）：数值 tag 不写类型名；逃生口写 65535 加显式别名。
    fn enter_call(&self) -> TokenStream {
        match &self.discriminant {
            PolyDiscriminant::Tag(tag) => {
                let tag = Literal::u16_suffixed(*tag);
                quote!(e.enter_polymorphic_object(#tag, ::core::option::Option::None)?;)
            }
            PolyDiscriminant::TypeName(name) => {
                let tag = Literal::u16_suffixed(POLY_ESCAPE_TAG);
                quote!(e.enter_polymorphic_object(
                    #tag,
                    ::core::option::Option::Some(#name),
                )?;)
            }
        }
    }
    /// 解码头部的匹配模式：数值 tag 忽略 type_name，逃生口要求别名恰好相等。
    fn decode_pattern(&self) -> TokenStream {
        match &self.discriminant {
            PolyDiscriminant::Tag(tag) => {
                let tag = Literal::u16_suffixed(*tag);
                quote!((#tag, _))
            }
            PolyDiscriminant::TypeName(name) => {
                let tag = Literal::u16_suffixed(POLY_ESCAPE_TAG);
                quote!((#tag, ::core::option::Option::Some(#name)))
            }
        }
    }
}

/// 多态枚举（FR-D05、FR-W07）：变体带载荷且有变体级 tag/type_name。
struct PolyEnumShape<'a> {
    name: &'a Ident,
    generics: &'a Generics,
    variants: Vec<PolyVariant>,
}

/// 多态枚举的展开期校验：每个变体恰有一种判别方式、同一 enum 不混用、tag 与别名不重复。
fn expand_polymorphic_enum(
    input: &DeriveInput,
    data: &DataEnum,
    attrs: &attr::TypeAttrs,
) -> syn::Result<TokenStream> {
    if let Some(default) = &attrs.default {
        return Err(syn::Error::new_spanned(default, INVALID_POSITION));
    }
    let type_params: Vec<Ident> = input
        .generics
        .type_params()
        .map(|param| param.ident.clone())
        .collect();
    let mut mode: Option<PolyMode> = None;
    let mut seen_tags = BTreeSet::new();
    let mut seen_names = BTreeSet::new();
    let mut variants = Vec::new();
    for variant in &data.variants {
        let parsed = attr::parse_variant(&variant.attrs)?;
        let discriminant = match (parsed.tag, parsed.type_name) {
            (Some(tag), None) => {
                if !seen_tags.insert(tag) {
                    return Err(syn::Error::new_spanned(
                        variant,
                        format!("ace: duplicate tag {tag}"),
                    ));
                }
                reject_mode(&mut mode, PolyMode::Tag, variant)?;
                PolyDiscriminant::Tag(tag)
            }
            (None, Some(name)) => {
                if !seen_names.insert(name.value()) {
                    return Err(syn::Error::new_spanned(
                        variant,
                        format!("ace: duplicate type_name \"{}\"", name.value()),
                    ));
                }
                reject_mode(&mut mode, PolyMode::TypeName, variant)?;
                PolyDiscriminant::TypeName(name)
            }
            (Some(_), Some(_)) => {
                return Err(syn::Error::new_spanned(variant, POLY_BOTH));
            }
            (None, None) => return Err(syn::Error::new_spanned(variant, POLY_VARIANT_ATTR)),
        };
        let fields = match &variant.fields {
            Fields::Unit => PolyFields::Unit,
            Fields::Named(named) => {
                let collected = field::collect(&named.named, &type_params, &[])?;
                if collected.shared {
                    return Err(syn::Error::new_spanned(variant, POLY_SHARED_FIELD));
                }
                PolyFields::Named(collected.items)
            }
            Fields::Unnamed(_) => {
                return Err(syn::Error::new_spanned(variant, POLY_FIELD_SHAPE));
            }
        };
        variants.push(PolyVariant {
            ident: variant.ident.clone(),
            fields,
            discriminant,
        });
    }
    let shape = PolyEnumShape {
        name: &input.ident,
        generics: &input.generics,
        variants,
    };
    Ok(shape.emit())
}

/// FR-D05：同一 enum 不得混用两种模式。
fn reject_mode(
    mode: &mut Option<PolyMode>,
    next: PolyMode,
    variant: &syn::Variant,
) -> syn::Result<()> {
    match mode {
        Some(current) if *current != next => Err(syn::Error::new_spanned(variant, POLY_MODE_MIXED)),
        Some(_) => Ok(()),
        None => {
            *mode = Some(next);
            Ok(())
        }
    }
}

impl PolyEnumShape<'_> {
    fn emit(&self) -> TokenStream {
        let encode = self.encode_impl();
        let decode = self.decode_impl();
        let element = self.element_impl();
        let cyclic = self.cyclic_impl();
        let non_optional = self.non_optional_impl();
        quote! {
            #encode
            #decode
            #element
            #cyclic
            #non_optional
        }
    }

    /// 所有变体的字段按声明顺序串联，供 where 谓词生成。
    fn fields(&self) -> impl Iterator<Item = &FieldInfo> {
        self.variants
            .iter()
            .flat_map(|variant| variant.fields.items().iter())
    }

    /// 变体的线上字段（skip 除外），按编号升序（FR-D02）。
    fn wire_fields(variant: &PolyVariant) -> Vec<&FieldInfo> {
        let mut wire: Vec<&FieldInfo> = variant
            .fields
            .items()
            .iter()
            .filter(|field| !matches!(field.kind, Kind::Skip))
            .collect();
        wire.sort_by_key(|field| field.order);
        wire
    }

    fn encode_impl(&self) -> TokenStream {
        let generics = with_predicates(
            self.generics,
            bounds(self.fields(), &[encode_trait()], false),
        );
        let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
        let name = self.name;
        let arms = self
            .variants
            .iter()
            .map(|variant| self.encode_arm(variant, false));
        let element_arms = self
            .variants
            .iter()
            .map(|variant| self.encode_arm(variant, true));
        quote! {
            impl #impl_generics ::ace_serialize::schema::AceEncode for #name #ty_generics #where_clause {
                fn ace_encode(
                    &self,
                    e: &mut ::ace_serialize::encode::Encoder<'_>,
                ) -> ::ace_serialize::error::Result<()> {
                    match self {
                        #(#arms)*
                    }
                }
                fn ace_encode_element(
                    &self,
                    e: &mut ::ace_serialize::encode::Encoder<'_>,
                ) -> ::ace_serialize::error::Result<()> {
                    match self {
                        #(#element_arms)*
                    }
                }
            }
        }
    }

    /// 变体模式只绑定线上字段，skip 字段由 `..` 略过；局部分绑名走 [`binding`]，
    /// 否则字段名 `e` 会遮蔽生成代码的 Encoder 参数（复审 I1）。
    fn pattern(&self, variant: &PolyVariant) -> TokenStream {
        let name = self.name;
        let ident = &variant.ident;
        if variant.fields.is_unit() {
            return quote!(#name::#ident);
        }
        let wire = Self::wire_fields(variant);
        let bindings = wire.iter().map(|field| {
            let ident = &field.ident;
            let local = binding(field);
            quote!(#ident: #local)
        });
        quote!(#name::#ident { #(#bindings,)* .. })
    }

    fn encode_arm(&self, variant: &PolyVariant, element: bool) -> TokenStream {
        let pattern = self.pattern(variant);
        let wire = Self::wire_fields(variant);
        let body = encode_body_with(&wire, |field| {
            let local = binding(field);
            quote!(#local)
        });
        let enter = variant.enter_call();
        let finish = if element {
            quote! {
                let encoded = body();
                let left = e.leave_object();
                encoded?;
                let id = left?;
                e.write_element_bytes(&id.to_le_bytes())
            }
        } else {
            quote! {
                let encoded = body();
                let left = e.leave_object();
                encoded?;
                left?;
                ::core::result::Result::Ok(())
            }
        };
        quote! {
            #pattern => {
                #enter
                #body
                #finish
            }
        }
    }

    fn decode_impl(&self) -> TokenStream {
        let generics = with_predicates(
            self.generics,
            bounds(self.fields(), &[decode_trait()], true),
        );
        let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
        let name = self.name;
        let out = out_parameter();
        let body = self.decode_body_tokens(quote!(::core::primitive::u64::from_le_bytes(payload)));
        let arms = self.into_arms();
        quote! {
            impl #impl_generics ::ace_serialize::schema::AceDecode for #name #ty_generics #where_clause {
                fn ace_decode(
                    d: &mut ::ace_serialize::decode::Decoder<'_>,
                ) -> ::ace_serialize::error::Result<Self> {
                    let payload = d.decode_required_scalar()?;
                    #body
                }
                fn ace_decode_into(
                    d: &mut ::ace_serialize::decode::Decoder<'_>,
                    #out: &mut Self,
                ) -> ::ace_serialize::error::Result<()> {
                    let payload = d.decode_required_scalar()?;
                    let (tag, type_name) = d.enter_polymorphic_object(
                        ::core::primitive::u64::from_le_bytes(payload),
                    )?;
                    let decoded = match (tag, type_name.as_deref()) {
                        #(#arms)*
                    };
                    let left = d.leave_object();
                    decoded?;
                    left?;
                    ::core::result::Result::Ok(())
                }
            }
        }
    }

    fn element_impl(&self) -> TokenStream {
        let generics = with_predicates(
            self.generics,
            bounds(self.fields(), &[encode_trait(), decode_trait()], true),
        );
        let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
        let name = self.name;
        let body = self.decode_body_tokens(quote!(::core::primitive::u64::from(id)));
        let into_arms = self.into_arms();
        let out = out_parameter();
        quote! {
            impl #impl_generics ::ace_serialize::schema::AceElement for #name #ty_generics #where_clause {
                fn element_kind() -> ::ace_serialize::schema::ElementKind {
                    ::ace_serialize::schema::ElementKind::Ref
                }
                fn encode_element(
                    &self,
                    e: &mut ::ace_serialize::encode::Encoder<'_>,
                ) -> ::ace_serialize::error::Result<()> {
                    <Self as ::ace_serialize::schema::AceEncode>::ace_encode_element(self, e)
                }
                fn decode_element(
                    d: &mut ::ace_serialize::decode::Decoder<'_>,
                ) -> ::ace_serialize::error::Result<Self> {
                    let id = d.read_element_id()?;
                    #body
                }
                /// 元素级复用（R22①）：同判别变体时按字段的 `ace_decode_into` 写入既有载荷，
                /// 跨变体整体替换（与 `ace_decode_into` 同一套规则）。
                fn decode_element_into(
                    d: &mut ::ace_serialize::decode::Decoder<'_>,
                    #out: &mut Self,
                ) -> ::ace_serialize::error::Result<()> {
                    let id = d.read_element_id()?;
                    let (tag, type_name) = d.enter_polymorphic_object(
                        <::core::primitive::u64 as ::core::convert::From<::core::primitive::u32>>::from(id),
                    )?;
                    let decoded = match (tag, type_name.as_deref()) {
                        #(#into_arms)*
                    };
                    let left = d.leave_object();
                    decoded?;
                    left?;
                    ::core::result::Result::Ok(())
                }
            }
        }
    }

    /// 读取多态体头并按判别值分派到变体；未知 tag 或无匹配别名→4 InvalidTag。
    /// `payload` 为引用载荷（字段/根为根槽 payload，元素为元素 id）的表达式。
    fn decode_body_tokens(&self, payload: TokenStream) -> TokenStream {
        let arms = self.decode_arms();
        quote! {
            let (tag, type_name) = d.enter_polymorphic_object(#payload)?;
            let decoded = match (tag, type_name.as_deref()) {
                #(#arms)*
            };
            let left = d.leave_object();
            let value = decoded?;
            left?;
            ::core::result::Result::Ok(value)
        }
    }

    fn decode_arms(&self) -> Vec<TokenStream> {
        let mut arms: Vec<TokenStream> = self
            .variants
            .iter()
            .map(|variant| {
                let pattern = variant.decode_pattern();
                let wire = Self::wire_fields(variant);
                let construct = self.construct(variant);
                let body = decode_body_with(&wire, construct);
                quote! {
                    #pattern => {
                        #body
                        body()
                    }
                }
            })
            .collect();
        arms.push(quote! {
            _ => ::core::result::Result::Err(::ace_serialize::error::Error::InvalidTag)
        });
        arms
    }

    /// 写入既有实例（FR-T09/R21）：同一判别变体时复用该变体已有载荷逐字段写入，跨变体或从
    /// 单元变体进入时按 [`Self::construct`] 构造新载荷后整体替换（跨变体没有可复用布局，也不
    /// 检查旧变体内可能存在的 `RefCell`/`RwLock` 单元格——整体替换不触发该检查，FR-T09⑥/R18）。
    fn into_arms(&self) -> Vec<TokenStream> {
        let name = self.name;
        let out = out_parameter();
        let mut arms: Vec<TokenStream> = self
            .variants
            .iter()
            .map(|variant| {
                let pattern = variant.decode_pattern();
                let ident = &variant.ident;
                let wire = Self::wire_fields(variant);
                let arm = if variant.fields.is_unit() {
                    // 单元变体没有可复用载荷，替换即写入。
                    quote! {
                        *#out = #name::#ident;
                        ::core::result::Result::Ok(())
                    }
                } else {
                    let bindings = variant.fields.items().iter().map(|field| {
                        let ident = &field.ident;
                        let local = binding(field);
                        quote!(#ident: #local)
                    });
                    let statements: Vec<TokenStream> = wire
                        .iter()
                        .map(|&field| {
                            let local = binding(field);
                            into_statement(field, quote!(*#local))
                        })
                        .collect();
                    let skips = variant
                        .fields
                        .items()
                        .iter()
                        .filter(|field| matches!(field.kind, Kind::Skip))
                        .map(|field| {
                            let local = binding(field);
                            let value = skip_default(field);
                            quote!(*#local = #value;)
                        });
                    // 同 `into_body`：线上字段经捕获的 `d` 改写，skip 只经 `&mut` 绑定写入；两者都有
                    // 字段写入时闭包需要 `FnMut`，仅真正空载荷（无线上字段也无 skip）才不加 `mut`。
                    let has_skips = variant
                        .fields
                        .items()
                        .iter()
                        .any(|field| matches!(field.kind, Kind::Skip));
                    let mutability = if statements.is_empty() && !has_skips {
                        quote!()
                    } else {
                        quote!(mut)
                    };
                    let body = decode_body_with(&wire, self.construct(variant));
                    quote! {
                        if let #name::#ident { #(#bindings,)* } = #out {
                            let #mutability __ace_into = || -> ::ace_serialize::error::Result<()> {
                                #(#statements)*
                                #(#skips)*
                                ::core::result::Result::Ok(())
                            };
                            __ace_into()
                        } else {
                            #body
                            body().map(|value| *#out = value)
                        }
                    }
                };
                quote! {
                    #pattern => {
                        #arm
                    }
                }
            })
            .collect();
        arms.push(quote! {
            _ => ::core::result::Result::Err(::ace_serialize::error::Error::InvalidTag)
        });
        arms
    }

    fn construct(&self, variant: &PolyVariant) -> TokenStream {
        let name = self.name;
        let ident = &variant.ident;
        if variant.fields.is_unit() {
            return quote!(#name::#ident);
        }
        let inits: Vec<TokenStream> = variant.fields.items().iter().map(field_init).collect();
        quote!(#name::#ident { #(#inits),* })
    }

    fn cyclic_impl(&self) -> TokenStream {
        let generics = with_predicates(
            self.generics,
            bounds(
                self.fields(),
                &[encode_trait(), decode_trait(), cyclic_trait()],
                true,
            ),
        );
        let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
        let name = self.name;
        quote! {
            impl #impl_generics ::ace_serialize::schema::AceDecodeCyclic for #name #ty_generics #where_clause {
                type Plan = Self;
                fn decode_plan(
                    ctx: &mut ::ace_serialize::decode::DecodeCtx,
                ) -> ::ace_serialize::error::Result<Self::Plan> {
                    ctx.value::<Self>(::ace_serialize::decode::PlanValueLayout::Leaf)
                }
                fn build_from_plan(
                    _ctx: &mut ::ace_serialize::decode::BuildCtx<'_>,
                    plan: Self::Plan,
                ) -> Self {
                    plan
                }
            }
        }
    }

    fn non_optional_impl(&self) -> TokenStream {
        let generics = with_predicates(self.generics, Vec::new());
        let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
        let name = self.name;
        quote! {
            impl #impl_generics ::ace_serialize::schema::NonOptional for #name #ty_generics #where_clause {}
        }
    }
}

/// 未标 repr 时逐变体检查字面量判别值；非字面量由生成代码的 const 断言覆盖。
fn reject_wide_discriminants(data: &DataEnum) -> syn::Result<()> {
    for variant in &data.variants {
        let Some((_, expr)) = &variant.discriminant else {
            continue;
        };
        let Some(value) = literal_discriminant(expr) else {
            continue;
        };
        if i32::try_from(value).is_err() {
            return Err(syn::Error::new_spanned(variant, OVER_I32_DISCRIMINANT));
        }
    }
    Ok(())
}

/// 判别值字面量（含负号与括号）解析为 `i128`；其它表达式返回 `None`。
fn literal_discriminant(expr: &Expr) -> Option<i128> {
    match expr {
        Expr::Lit(lit) => match &lit.lit {
            Lit::Int(int) => int.base10_parse().ok(),
            _ => None,
        },
        Expr::Unary(unary) if matches!(unary.op, UnOp::Neg(_)) => {
            literal_discriminant(&unary.expr).map(|value| -value)
        }
        Expr::Paren(paren) => literal_discriminant(&paren.expr),
        _ => None,
    }
}

/// 整数枚举的判别值表示。`ty` 为绝对路径的 repr 类型；未标 repr 时为 `i32`（FR-T01）。
struct EnumRepr {
    ty: TokenStream,
    signed: bool,
    /// 自然宽度（FR-W12）：元素体按此宽度写，槽 payload 总是 8B。
    width: usize,
    /// 是否显式标注了 repr：未标时 wire 按 i32，需自行拒绝超 i32 的判别值（用户裁决 A）。
    explicit: bool,
}

fn enum_repr(attrs: &[Attribute]) -> syn::Result<EnumRepr> {
    let mut repr = None;
    for attr in attrs.iter().filter(|attr| attr.path().is_ident("repr")) {
        let metas = attr
            .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            .map_err(attr::with_ace_prefix)?;
        for meta in metas {
            let Meta::Path(path) = &meta else {
                return Err(syn::Error::new_spanned(&meta, UNSUPPORTED_REPR));
            };
            let ident = path
                .get_ident()
                .ok_or_else(|| syn::Error::new_spanned(path, UNSUPPORTED_REPR))?;
            let parsed = integer_repr(&ident.to_string())
                .ok_or_else(|| syn::Error::new_spanned(ident, UNSUPPORTED_REPR))?;
            repr.get_or_insert(parsed);
        }
    }
    Ok(repr.unwrap_or(EnumRepr {
        ty: quote!(::core::primitive::i32),
        signed: true,
        width: 4,
        explicit: false,
    }))
}

/// FR-T01 的 enum repr 白名单；返回 `None` 表示不允许的表示。
fn integer_repr(name: &str) -> Option<EnumRepr> {
    let (ty, signed, width) = match name {
        "i8" => (quote!(::core::primitive::i8), true, 1),
        "i16" => (quote!(::core::primitive::i16), true, 2),
        "i32" => (quote!(::core::primitive::i32), true, 4),
        "i64" => (quote!(::core::primitive::i64), true, 8),
        "u8" => (quote!(::core::primitive::u8), false, 1),
        "u16" => (quote!(::core::primitive::u16), false, 2),
        "u32" => (quote!(::core::primitive::u32), false, 4),
        "u64" => (quote!(::core::primitive::u64), false, 8),
        _ => return None,
    };
    Some(EnumRepr {
        ty,
        signed,
        width,
        explicit: true,
    })
}

/// 一个整数枚举生成所需的全部信息。
struct EnumShape<'a> {
    name: &'a Ident,
    repr: &'a EnumRepr,
    /// 显式声明的默认变体：仅该变体可省略，缺席恢复该变体（FR-D07）。
    default: Option<&'a Ident>,
    variants: Vec<&'a Ident>,
}

impl EnumShape<'_> {
    /// 五个 impl 与两个私有辅助函数放进 `const _: () = { ... }`：impl 仍是全局的，辅助名不进入用户模块命名空间。
    fn emit(&self) -> TokenStream {
        let name = self.name;
        let width = Literal::usize_unsuffixed(self.repr.width);
        let payload = self.payload_fn();
        let from_payload = self.decode_fn();
        let discriminant_checks = self.discriminant_checks();
        let element_load = self.element_load();
        // 仅显式默认变体可省略：字段位置的该变体不写槽（FR-W04 ③）；`Some(v)` 由 `ace_encode_some` 保证必写。
        let omit = self.default.map(|default| {
            quote! {
                if e.current_order().is_some()
                    && __ace_enum_payload(self) == __ace_enum_payload(&#name::#default)
                {
                    return e.encode_none();
                }
            }
        });
        let some_encoder = self.default.map(|_| {
            quote! {
                fn ace_encode_some(
                    &self,
                    e: &mut ::ace_serialize::encode::Encoder<'_>,
                ) -> ::ace_serialize::error::Result<()> {
                    e.encode_scalar(__ace_enum_payload(self))
                }
            }
        });
        // 缺席的字段恢复默认变体；根位置缺席仍是缺槽（FR-W10：≤8B 标量根零值也必有根槽）。
        let absent_default = self.default.map(|default| {
            quote! {
                if d.selected_is_none() {
                    if d.root_is_none() {
                        return ::core::result::Result::Err(
                            ::ace_serialize::error::Error::MissingRequiredField,
                        );
                    }
                    return ::core::result::Result::Ok(#name::#default);
                }
            }
        });
        quote! {
            const _: () = {
                #payload
                #from_payload
                #discriminant_checks
                impl ::ace_serialize::schema::AceEncode for #name {
                    fn ace_encode(
                        &self,
                        e: &mut ::ace_serialize::encode::Encoder<'_>,
                    ) -> ::ace_serialize::error::Result<()> {
                        #omit
                        e.encode_scalar(__ace_enum_payload(self))
                    }
                    #some_encoder
                    fn ace_encode_element(
                        &self,
                        e: &mut ::ace_serialize::encode::Encoder<'_>,
                    ) -> ::ace_serialize::error::Result<()> {
                        e.write_element_bytes(&__ace_enum_payload(self)[..#width])
                    }
                }
                impl ::ace_serialize::schema::AceDecode for #name {
                    fn ace_decode(
                        d: &mut ::ace_serialize::decode::Decoder<'_>,
                    ) -> ::ace_serialize::error::Result<Self> {
                        #absent_default
                        __ace_enum_from_payload(d.decode_required_scalar()?)
                    }
                }
                impl ::ace_serialize::schema::AceElement for #name {
                    fn element_kind() -> ::ace_serialize::schema::ElementKind {
                        ::ace_serialize::schema::ElementKind::Value { size: #width }
                    }
                    fn encode_element(
                        &self,
                        e: &mut ::ace_serialize::encode::Encoder<'_>,
                    ) -> ::ace_serialize::error::Result<()> {
                        e.write_element_bytes(&__ace_enum_payload(self)[..#width])
                    }
                    fn decode_element(
                        d: &mut ::ace_serialize::decode::Decoder<'_>,
                    ) -> ::ace_serialize::error::Result<Self> {
                        #element_load
                        __ace_enum_from_payload(payload)
                    }
                }
                impl ::ace_serialize::schema::AceDecodeCyclic for #name {
                    type Plan = Self;
                    fn decode_plan(
                        ctx: &mut ::ace_serialize::decode::DecodeCtx,
                    ) -> ::ace_serialize::error::Result<Self::Plan> {
                        ctx.value::<Self>(::ace_serialize::decode::PlanValueLayout::Leaf)
                    }
                    fn build_from_plan(
                        _ctx: &mut ::ace_serialize::decode::BuildCtx<'_>,
                        plan: Self::Plan,
                    ) -> Self {
                        plan
                    }
                }
                impl ::ace_serialize::schema::NonOptional for #name {}
            };
        }
    }

    /// 判别值按 repr 位模式零扩展或符号扩展成 8B payload（FR-W12）。
    fn payload_fn(&self) -> TokenStream {
        let name = self.name;
        let repr = &self.repr.ty;
        let extend = if self.repr.signed {
            quote!((discriminant as ::core::primitive::i64) as ::core::primitive::u64)
        } else {
            quote!(discriminant as ::core::primitive::u64)
        };
        if self.variants.is_empty() {
            // 零变体枚举没有可构造的值；载荷函数只为满足 trait 形状，解码侧没有合法判别值。
            return quote! {
                fn __ace_enum_payload(_value: &#name) -> [::core::primitive::u8; 8] {
                    [0; 8]
                }
            };
        }
        let arms = self
            .variants
            .iter()
            .map(|variant| quote!(#name::#variant => #name::#variant as #repr,));
        quote! {
            fn __ace_enum_payload(value: &#name) -> [::core::primitive::u8; 8] {
                let discriminant: #repr = match value {
                    #(#arms)*
                };
                ::core::primitive::u64::to_le_bytes(#extend)
            }
        }
    }

    /// 反解：先按 repr 宽度收窄（越界即 4 InvalidTag），再逐变体比对判别值，不构造无效值。
    fn decode_fn(&self) -> TokenStream {
        let name = self.name;
        let repr = &self.repr.ty;
        let (wire, payload) = if self.repr.signed {
            (
                quote!(::core::primitive::i64),
                quote!(::core::primitive::i64::from_le_bytes(payload)),
            )
        } else {
            (
                quote!(::core::primitive::u64),
                quote!(::core::primitive::u64::from_le_bytes(payload)),
            )
        };
        let binding = if self.variants.is_empty() {
            quote!(let _value =)
        } else {
            quote!(let value =)
        };
        let arms = self.variants.iter().map(|variant| {
            quote! {
                if value == #name::#variant as #repr {
                    return ::core::result::Result::Ok(#name::#variant);
                }
            }
        });
        quote! {
            fn __ace_enum_from_payload(
                payload: [::core::primitive::u8; 8],
            ) -> ::ace_serialize::error::Result<#name> {
                #binding <#repr as ::core::convert::TryFrom<#wire>>::try_from(#payload)
                    .map_err(|_| ::ace_serialize::error::Error::InvalidTag)?;
                #(#arms)*
                ::core::result::Result::Err(::ace_serialize::error::Error::InvalidTag)
            }
        }
    }

    /// 未标 repr 时 wire 按 i32：非字面量判别值无法在展开期取值，由 const 断言在编译期兜底（用户裁决 A）。
    fn discriminant_checks(&self) -> TokenStream {
        if self.repr.explicit {
            return TokenStream::new();
        }
        let name = self.name;
        let message = Literal::string(OVER_I32_DISCRIMINANT);
        let checks = self.variants.iter().map(|variant| {
            quote! {
                const _: () = {
                    let discriminant = #name::#variant as ::core::primitive::i64;
                    ::core::assert!(
                        discriminant >= ::core::primitive::i32::MIN as ::core::primitive::i64
                            && discriminant <= ::core::primitive::i32::MAX as ::core::primitive::i64,
                        #message,
                    );
                };
            }
        });
        quote! { #(#checks)* }
    }

    /// 元素体只有 repr 宽度个字节；有符号 repr 必须按最高位回填高位，否则负值被读成大正数（复审 I1）。
    fn element_load(&self) -> TokenStream {
        let width = Literal::usize_unsuffixed(self.repr.width);
        if !self.repr.signed {
            return quote! {
                let mut payload = [0u8; 8];
                payload[..#width].copy_from_slice(d.read_element_bytes(#width)?);
            };
        }
        let high = Literal::usize_unsuffixed(self.repr.width - 1);
        quote! {
            let bytes = d.read_element_bytes(#width)?;
            let fill = if bytes[#high] & 0x80 != 0 { 0xFFu8 } else { 0u8 };
            let mut payload = [fill; 8];
            payload[..#width].copy_from_slice(bytes);
        }
    }
}

/// 一个结构体生成所需的全部信息。`wire` 为线上字段，按编号升序排列；`fields` 为声明顺序。
struct Shape<'a> {
    name: &'a Ident,
    generics: &'a Generics,
    fields: &'a [FieldInfo],
    wire: Vec<&'a FieldInfo>,
    shared: bool,
}

impl Shape<'_> {
    fn emit(&self) -> TokenStream {
        let encode = self.encode_impl();
        let (decode, element) = if self.shared {
            (self.shared_decode_impl(), TokenStream::new())
        } else {
            (self.decode_impl(), self.element_impl())
        };
        let cyclic = self.cyclic_impl();
        let non_optional = self.non_optional_impl();
        quote! {
            #encode
            #decode
            #element
            #cyclic
            #non_optional
        }
    }

    fn encode_impl(&self) -> TokenStream {
        let generics = with_predicates(
            self.generics,
            bounds(self.fields.iter(), &[encode_trait()], false),
        );
        let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
        let name = self.name;
        let body = encode_body(&self.wire);
        let element_body = encode_body(&self.wire);
        let element_method = quote! {
            fn ace_encode_element(
                &self,
                e: &mut ::ace_serialize::encode::Encoder<'_>,
            ) -> ::ace_serialize::error::Result<()> {
                e.enter_object()?;
                #element_body
                let encoded = body();
                let left = e.leave_object();
                encoded?;
                let id = left?;
                e.write_element_bytes(&id.to_le_bytes())
            }
        };
        quote! {
            impl #impl_generics ::ace_serialize::schema::AceEncode for #name #ty_generics #where_clause {
                fn ace_encode(
                    &self,
                    e: &mut ::ace_serialize::encode::Encoder<'_>,
                ) -> ::ace_serialize::error::Result<()> {
                    e.enter_object()?;
                    #body
                    let encoded = body();
                    let left = e.leave_object();
                    encoded?;
                    left?;
                    ::core::result::Result::Ok(())
                }
                #element_method
            }
        }
    }

    fn decode_impl(&self) -> TokenStream {
        let generics = with_predicates(
            self.generics,
            bounds(self.fields.iter(), &[decode_trait()], true),
        );
        let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
        let name = self.name;
        let out = out_parameter();
        let body = decode_body(self.fields, &self.wire);
        let into_body = self.into_body();
        quote! {
            impl #impl_generics ::ace_serialize::schema::AceDecode for #name #ty_generics #where_clause {
                fn ace_decode(
                    d: &mut ::ace_serialize::decode::Decoder<'_>,
                ) -> ::ace_serialize::error::Result<Self> {
                    let payload = d.decode_required_scalar()?;
                    d.enter_object(::core::primitive::u64::from_le_bytes(payload))?;
                    #body
                    let decoded = body();
                    let left = d.leave_object();
                    let value = decoded?;
                    left?;
                    ::core::result::Result::Ok(value)
                }
                fn ace_decode_into(
                    d: &mut ::ace_serialize::decode::Decoder<'_>,
                    #out: &mut Self,
                ) -> ::ace_serialize::error::Result<()> {
                    let payload = d.decode_required_scalar()?;
                    d.enter_object(::core::primitive::u64::from_le_bytes(payload))?;
                    #into_body
                    let decoded = body();
                    let left = d.leave_object();
                    decoded?;
                    left?;
                    ::core::result::Result::Ok(())
                }
            }
        }
    }

    /// 写入既有实例的闭包体（FR-T09/R21）：线上字段按编号升序走各自类型的 `ace_decode_into`，
    /// 从而复用其容器缓冲；标量缺席恢复 `Default`；`skip` 字段在线上字段之后恢复 `Default`
    /// （与 [`decode_body`] 在构造表达式中取默认值同一语义）。不重建整个结构体。
    fn into_body(&self) -> TokenStream {
        let out = out_parameter();
        let statements: Vec<TokenStream> = self
            .wire
            .iter()
            .map(|&field| {
                let ident = &field.ident;
                into_statement(field, quote!(#out.#ident))
            })
            .collect();
        let skips: Vec<TokenStream> = self
            .fields
            .iter()
            .filter(|field| matches!(field.kind, Kind::Skip))
            .map(|field| {
                let ident = &field.ident;
                let value = skip_default(field);
                quote!(#out.#ident = #value;)
            })
            .collect();
        // `skip` 赋值也会捕获 `__ace_out`，故无线上字段但存在 skip 字段的闭包仍需 `mut`。
        let mutability = if statements.is_empty() && skips.is_empty() {
            quote!()
        } else {
            quote!(mut)
        };
        quote! {
            let #mutability body = || -> ::ace_serialize::error::Result<()> {
                #(#statements)*
                #(#skips)*
                ::core::result::Result::Ok(())
            };
        }
    }

    /// `shared` 语义类型（含 `Rc`/`Arc`/`Weak` 与环）的字节级拥有值入口（需求 R22②）：只生成
    /// [`AceDecode`]，其 `ace_decode` 直接把已传入的 `Decoder` 交给既有 `decode_cyclic` 图路径；
    /// `decode`/`decode_from` 负责 `Decoder::new` 与 `finish`，不新增公共 API、不改
    /// `AceDecodeCyclic` 的形参。`decode_cyclic` 只接受消息根状态，故该入口只在消息根可用，
    /// 图内已构造缓冲不承诺复用：`ace_decode_into` 保持默认的「整体替换」。
    fn shared_decode_impl(&self) -> TokenStream {
        let generics = with_predicates(self.generics, cyclic_bounds(self.fields.iter()));
        let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
        let name = self.name;
        quote! {
            impl #impl_generics ::ace_serialize::schema::AceDecode for #name #ty_generics #where_clause {
                fn ace_decode(
                    d: &mut ::ace_serialize::decode::Decoder<'_>,
                ) -> ::ace_serialize::error::Result<Self> {
                    ::ace_serialize::decode::decode_cyclic::<Self>(d)
                }
            }
        }
    }

    fn element_impl(&self) -> TokenStream {
        let generics = with_predicates(
            self.generics,
            bounds(self.fields.iter(), &[encode_trait(), decode_trait()], true),
        );
        let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
        let name = self.name;
        let body = decode_body(self.fields, &self.wire);
        let into_body = self.into_body();
        let out = out_parameter();
        quote! {
            impl #impl_generics ::ace_serialize::schema::AceElement for #name #ty_generics #where_clause {
                fn element_kind() -> ::ace_serialize::schema::ElementKind {
                    ::ace_serialize::schema::ElementKind::Variable
                }
                fn encode_element(
                    &self,
                    e: &mut ::ace_serialize::encode::Encoder<'_>,
                ) -> ::ace_serialize::error::Result<()> {
                    <Self as ::ace_serialize::schema::AceEncode>::ace_encode_element(self, e)
                }
                fn decode_element(
                    d: &mut ::ace_serialize::decode::Decoder<'_>,
                ) -> ::ace_serialize::error::Result<Self> {
                    let id = d.read_element_id()?;
                    d.enter_object(
                        <::core::primitive::u64 as ::core::convert::From<::core::primitive::u32>>::from(id),
                    )?;
                    #body
                    let decoded = body();
                    let left = d.leave_object();
                    let value = decoded?;
                    left?;
                    ::core::result::Result::Ok(value)
                }
                /// 元素级复用（R22①）：按字段类型的 `ace_decode_into` 逐字段写入既有元素，
                /// 复用其容器缓冲。
                fn decode_element_into(
                    d: &mut ::ace_serialize::decode::Decoder<'_>,
                    #out: &mut Self,
                ) -> ::ace_serialize::error::Result<()> {
                    let id = d.read_element_id()?;
                    d.enter_object(
                        <::core::primitive::u64 as ::core::convert::From<::core::primitive::u32>>::from(id),
                    )?;
                    #into_body
                    let decoded = body();
                    let left = d.leave_object();
                    decoded?;
                    left?;
                    ::core::result::Result::Ok(())
                }
            }
        }
    }

    fn cyclic_impl(&self) -> TokenStream {
        let generics = with_predicates(self.generics, cyclic_bounds(self.fields.iter()));
        let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
        let name = self.name;
        let (plan_types, plan_exprs): (Vec<TokenStream>, Vec<TokenStream>) = plan_parts(&self.wire);
        let build = build_statements(&self.wire);
        let inits: Vec<TokenStream> = self.fields.iter().map(field_init).collect();
        let uses_ctx = self
            .wire
            .iter()
            .any(|field| !matches!(field.kind, Kind::Scalar(_)));
        let build_ctx = if uses_ctx { quote!(ctx) } else { quote!(_ctx) };
        let plan_param = if self.wire.is_empty() {
            quote!(_plan)
        } else {
            quote!(plan)
        };
        let object_param = if self.wire.is_empty() {
            quote!(_)
        } else {
            quote!(ctx)
        };
        quote! {
            impl #impl_generics ::ace_serialize::schema::AceDecodeCyclic for #name #ty_generics #where_clause {
                type Plan = (#(#plan_types,)*);
                fn decode_plan(
                    ctx: &mut ::ace_serialize::decode::DecodeCtx,
                ) -> ::ace_serialize::error::Result<Self::Plan> {
                    ctx.object(|#object_param| ::core::result::Result::Ok((#(#plan_exprs,)*)))
                }
                fn build_from_plan(
                    #build_ctx: &mut ::ace_serialize::decode::BuildCtx<'_>,
                    #plan_param: Self::Plan,
                ) -> Self {
                    #(#build)*
                    Self { #(#inits),* }
                }
            }
        }
    }

    fn non_optional_impl(&self) -> TokenStream {
        let generics = with_predicates(self.generics, Vec::new());
        let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
        let name = self.name;
        quote! {
            impl #impl_generics ::ace_serialize::schema::NonOptional for #name #ty_generics #where_clause {}
        }
    }
}

fn encode_trait() -> TokenStream {
    quote!(::ace_serialize::schema::AceEncode)
}

fn decode_trait() -> TokenStream {
    quote!(::ace_serialize::schema::AceDecode)
}

fn cyclic_trait() -> TokenStream {
    quote!(::ace_serialize::schema::AceDecodeCyclic)
}

/// `shared` 字节级入口（`AceDecode`）与环图解码（`AceDecodeCyclic`）共用的谓词：线上泛型字段需
/// `AceDecodeCyclic`，泛型 `skip` 字段在 `build_from_plan`/`ace_decode_into` 中取 `Default`。
/// 两个 impl 必须同一组——`AceDecode::ace_decode` 调用 `decode_cyclic::<Self>`，缺任一谓词都会
/// 让该调用不成立。
fn cyclic_bounds<'a>(fields: impl IntoIterator<Item = &'a FieldInfo>) -> Vec<WherePredicate> {
    bounds(fields, &[cyclic_trait()], true)
}

/// 生成 where 谓词：泛型字段类型按实际用到的 trait 加约束；skip 字段只在构造时需要 `Default`。
fn bounds<'a>(
    fields: impl IntoIterator<Item = &'a FieldInfo>,
    wire_traits: &[TokenStream],
    default_for_skip: bool,
) -> Vec<WherePredicate> {
    let mut predicates = Vec::new();
    let mut seen = BTreeSet::new();
    for field in fields.into_iter().filter(|field| field.generic) {
        let ty = &field.ty;
        let traits: Vec<TokenStream> = match &field.kind {
            Kind::Skip if default_for_skip => vec![quote!(::core::default::Default)],
            Kind::Skip => Vec::new(),
            _ => wire_traits.to_vec(),
        };
        for trait_path in traits {
            let predicate: WherePredicate = parse_quote!(#ty: #trait_path);
            if seen.insert(predicate.to_token_stream().to_string()) {
                predicates.push(predicate);
            }
        }
    }
    predicates
}

fn with_predicates(generics: &Generics, predicates: Vec<WherePredicate>) -> Generics {
    let mut generics = generics.clone();
    if !predicates.is_empty() {
        generics.make_where_clause().predicates.extend(predicates);
    }
    generics
}

/// 线上字段的局部绑定名，按字段标识符命名，避免与生成代码的 `d`、`e`、`ctx` 冲突。
fn binding(field: &FieldInfo) -> Ident {
    format_ident!("__ace_field_{}", field.ident)
}

/// `ace_decode_into` 的形参名带私有前缀：裸 `out` 在调用方作用域存在同名 `const out` 时，形参会被
/// 解析成该常量而非新绑定（参看 E0308/E0610 的报错），故形参与所有引用统一用该名字插值。
fn out_parameter() -> Ident {
    format_ident!("__ace_out")
}

/// 编码闭包体：按编号升序逐字段发射，每个发射后检查字段选择已被消费。
fn encode_body(wire: &[&FieldInfo]) -> TokenStream {
    encode_body_with(wire, |field| {
        let ident = &field.ident;
        quote!(&self.#ident)
    })
}

/// 同 [`encode_body`]，字段访问表达式由调用方给出：结构体用 `&self.field`，枚举变体用模式绑定的
/// `&field`。
fn encode_body_with(
    wire: &[&FieldInfo],
    access: impl Fn(&FieldInfo) -> TokenStream,
) -> TokenStream {
    let statements: Vec<TokenStream> = wire
        .iter()
        .map(|&field| encode_statement(field, access(field)))
        .collect();
    let mutability = if wire.is_empty() {
        quote!()
    } else {
        quote!(mut)
    };
    quote! {
        let #mutability body = || -> ::ace_serialize::error::Result<()> {
            #(#statements)*
            ::core::result::Result::Ok(())
        };
    }
}

fn encode_statement(field: &FieldInfo, access: TokenStream) -> TokenStream {
    let ty = &field.ty;
    let order = field.order;
    let span = ty.span();
    let emit = quote_spanned! {span=>
        e.begin_field(#order)?;
        <#ty as ::ace_serialize::schema::AceEncode>::ace_encode(#access, e)?;
        e.check_field_consumed()?;
    };
    match &field.kind {
        Kind::Scalar(is_default) => quote_spanned! {span=>
            if !::ace_serialize::schema::value::#is_default(*#access) {
                #emit
            }
        },
        _ => emit,
    }
}

/// 解码闭包体：按编号升序读取到局部绑定，再在声明顺序下构造 `Self`。
fn decode_body(fields: &[FieldInfo], wire: &[&FieldInfo]) -> TokenStream {
    let inits: Vec<TokenStream> = fields.iter().map(field_init).collect();
    decode_body_with(wire, quote!(Self { #(#inits),* }))
}

/// 同 [`decode_body`]，构造表达式由调用方给出：结构体用 `Self { .. }`，多态变体用
/// `Enum::Variant { .. }` 或单元路径。
fn decode_body_with(wire: &[&FieldInfo], construct: TokenStream) -> TokenStream {
    let statements: Vec<TokenStream> = wire.iter().map(|&field| decode_statement(field)).collect();
    let mutability = if wire.is_empty() {
        quote!()
    } else {
        quote!(mut)
    };
    quote! {
        let #mutability body = || -> ::ace_serialize::error::Result<Self> {
            #(#statements)*
            ::core::result::Result::Ok(#construct)
        };
    }
}

fn decode_statement(field: &FieldInfo) -> TokenStream {
    let binding = binding(field);
    let ty = &field.ty;
    let order = field.order;
    let span = ty.span();
    match &field.kind {
        Kind::Scalar(_) => quote_spanned! {span=>
            let #binding: #ty = if d.begin_field(#order)? {
                <#ty as ::ace_serialize::schema::AceDecode>::ace_decode(d)?
            } else {
                ::core::default::Default::default()
            };
        },
        Kind::Option => quote_spanned! {span=>
            d.begin_field(#order)?;
            let #binding: #ty = <#ty as ::ace_serialize::schema::AceDecode>::ace_decode(d)?;
        },
        // 缺席的判定交给字段类型：核心的 `decode_required_scalar`/`decode_record` 对缺席字段返回
        // `MissingRequiredField`；标注默认变体的整数枚举据此恢复该变体（FR-D07）。
        Kind::Required => quote_spanned! {span=>
            d.begin_field(#order)?;
            let #binding: #ty = <#ty as ::ace_serialize::schema::AceDecode>::ace_decode(d)?;
        },
        Kind::Skip => TokenStream::new(),
    }
}

/// 同 [`decode_statement`]，但写入既有位置 `place`（结构体为 `__ace_out.f`，枚举同变体为 `*local`）：
/// 字段类型走 `ace_decode_into` 以复用其容器缓冲；标量缺席恢复 `Default`（与拥有值路径一致）。
fn into_statement(field: &FieldInfo, place: TokenStream) -> TokenStream {
    let ty = &field.ty;
    let order = field.order;
    let span = ty.span();
    match &field.kind {
        Kind::Scalar(_) => quote_spanned! {span=>
            if d.begin_field(#order)? {
                <#ty as ::ace_serialize::schema::AceDecode>::ace_decode_into(d, &mut #place)?;
            } else {
                #place = ::core::default::Default::default();
            }
        },
        // `Kind::Option` 使用完整字段类型（`note: Option<Item>` 时即 `Option<Item>`）：内层复用与
        // `Some→None` 的整体替换都由该类型的覆写完成（D3），此处不额外套一层 `Option`。
        Kind::Option | Kind::Required => quote_spanned! {span=>
            d.begin_field(#order)?;
            <#ty as ::ace_serialize::schema::AceDecode>::ace_decode_into(d, &mut #place)?;
        },
        Kind::Skip => TokenStream::new(),
    }
}

/// skip 字段经块内辅助 trait 构造：缺少 Default 时给出 ace: 诊断，且不在模块级新增名字。
/// 字段类型只出现在块外的 `let` 注解中，块内的辅助名不遮蔽用户同名类型。
fn field_init(field: &FieldInfo) -> TokenStream {
    let ident = &field.ident;
    match &field.kind {
        Kind::Skip => {
            let value = skip_default(field);
            quote!(#ident: #value)
        }
        _ => {
            let binding = binding(field);
            quote!(#ident: #binding)
        }
    }
}

/// `skip` 字段的 `Default` 取值块：块内辅助 trait 提供 ace: 诊断，且不在模块级新增名字。
/// 拥有值构造（[`field_init`]）与写入既有实例（`ace_decode_into`）共用同一取值，保持两者语义一致。
fn skip_default(field: &FieldInfo) -> TokenStream {
    let ty = &field.ty;
    let span = ty.span();
    quote_spanned! {span=>
        {
            let __ace_skip: #ty = {
                #[diagnostic::on_unimplemented(
                    message = "ace: skipped field type must implement Default",
                    label = "this #[ace(skip)] field type does not implement Default",
                    note = "#[ace(skip)] fields are constructed with Default::default()"
                )]
                trait AceSkipDefault: ::core::default::Default {}
                impl<T: ::core::default::Default> AceSkipDefault for T {}
                fn skip_default<T: AceSkipDefault>() -> T {
                    <T as ::core::default::Default>::default()
                }
                skip_default()
            };
            __ace_skip
        }
    }
}

/// 循环计划的元组类型与阶段一表达式，顺序与线上编号升序一致。标量计划为 `Option<T>`，缺席即零值。
/// 泛型字段保持关联类型投影：泛型上下文中 `#ty: AceDecodeCyclic` 无法归约为 `u32`（核心 impl 需 `T` 满足额外约束），
/// 改写会与 `ctx.field` 的返回类型不一致。
fn plan_parts(wire: &[&FieldInfo]) -> (Vec<TokenStream>, Vec<TokenStream>) {
    wire.iter()
        .map(|&field| {
            let ty = &field.ty;
            let order = field.order;
            let span = ty.span();
            match &field.kind {
                Kind::Scalar(_) => (
                    quote_spanned! {span=> ::core::option::Option<#ty>},
                    quote_spanned! {span=> ctx.field::<::core::option::Option<#ty>>(#order)?},
                ),
                _ => {
                    let shared = if field.generic {
                        None
                    } else {
                        shared_plan_type(ty)
                    };
                    let plan = match shared {
                        Some(plan) => plan.to_token_stream(),
                        None => quote_spanned! {span=>
                            <#ty as ::ace_serialize::schema::AceDecodeCyclic>::Plan
                        },
                    };
                    (plan, quote_spanned! {span=> ctx.field::<#ty>(#order)?})
                }
            }
        })
        .unzip()
}

/// 共享引用的计划类型：`Rc`、`Arc`、`Weak` 计划为 `u32`，与核心 `Plan` 一致；沿 `Option`、`Vec`、`Box` 的唯一泛型参数与
/// `BTreeMap` 的值参数递归（键参数不改）。入口先解开括号与分组（递归调用同经此入口）。语法链上没有共享引用时返回 `None`，
/// 沿用 `AceDecodeCyclic::Plan` 投影：自引用字段的投影会回到 `Self::Plan`，形成 E0275。
fn shared_plan_type(ty: &Type) -> Option<Type> {
    let Type::Path(path) = unwrap_group(ty) else {
        return None;
    };
    let segment = path.path.segments.last()?;
    let name = segment.ident.to_string();
    if matches!(name.as_str(), "Rc" | "Arc" | "Weak") {
        return Some(parse_quote!(::core::primitive::u32));
    }
    let value_index = match name.as_str() {
        "Option" | "Vec" | "Box" => 0,
        "BTreeMap" => 1,
        _ => return None,
    };
    let PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    let GenericArgument::Type(value) = arguments.args.get(value_index)? else {
        return None;
    };
    let rewritten = shared_plan_type(value)?;
    let mut plan = path.clone();
    let segment = plan.path.segments.last_mut()?;
    let PathArguments::AngleBracketed(arguments) = &mut segment.arguments else {
        return None;
    };
    let GenericArgument::Type(slot) = arguments.args.iter_mut().nth(value_index)? else {
        return None;
    };
    *slot = rewritten;
    Some(Type::Path(plan))
}

/// 阶段二按线上编号升序构造各字段；标量把缺席的 `None` 映射为零值。
fn build_statements(wire: &[&FieldInfo]) -> Vec<TokenStream> {
    wire.iter()
        .enumerate()
        .map(|(position, &field)| {
            let binding = binding(field);
            let ty = &field.ty;
            let index = Index::from(position);
            let span = ty.span();
            match &field.kind {
                Kind::Scalar(_) => quote_spanned! {span=>
                    let #binding: #ty = plan.#index.unwrap_or_default();
                },
                _ => quote_spanned! {span=>
                    let #binding: #ty =
                        <#ty as ::ace_serialize::schema::AceDecodeCyclic>::build_from_plan(
                            ctx,
                            plan.#index,
                        );
                },
            }
        })
        .collect()
}
