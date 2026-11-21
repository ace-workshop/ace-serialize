use syn::{parenthesized, parse::ParseStream, Attribute, Ident, LitInt, LitStr, Path, Token};

const UNKNOWN_ATTRIBUTE: &str = "ace: unknown attribute";
pub(crate) const INVALID_POSITION: &str = "ace: attribute in invalid position";
const DUPLICATE_ATTRIBUTE: &str = "ace: duplicate attribute";
const EXPECTED_INTEGER: &str = "ace: expected an integer literal";
const ORDER_OUT_OF_RANGE: &str = "ace: order out of range";
/// FR-D05：数值 tag 为 0..=65534；65535 专供 `type_name` 逃生口。
const TAG_OUT_OF_RANGE: &str =
    "ace: tag must be in 0..=65534; 65535 is reserved for the type_name escape";

#[derive(Default)]
pub(crate) struct FieldAttrs {
    pub(crate) order: Option<u32>,
    pub(crate) skip: bool,
}

/// 多态枚举变体的 `#[ace(...)]`：数值 tag 或显式 type_name，二选一（FR-D05）。
#[derive(Default)]
pub(crate) struct VariantAttrs {
    pub(crate) tag: Option<u16>,
    pub(crate) type_name: Option<LitStr>,
}

#[derive(Default)]
pub(crate) struct TypeAttrs {
    pub(crate) reserved: Vec<u32>,
    /// `#[ace(default = Variant)]` 的变体名；是否为该 enum 的变体由生成期校验（FR-D07）。
    pub(crate) default: Option<Ident>,
}

pub(crate) fn parse_field(attrs: &[Attribute]) -> syn::Result<FieldAttrs> {
    let mut parsed = FieldAttrs::default();
    for attr in attrs.iter().filter(|attr| attr.path().is_ident("ace")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("order") {
                reject_duplicate(parsed.order.is_some(), &meta.path)?;
                parsed.order = Some(parse_order_number(meta.value()?)?);
            } else if meta.path.is_ident("skip") {
                reject_duplicate(parsed.skip, &meta.path)?;
                parsed.skip = true;
            } else if meta.path.is_ident("reserved") || meta.path.is_ident("default") {
                return Err(syn::Error::new_spanned(&meta.path, INVALID_POSITION));
            } else {
                return Err(syn::Error::new_spanned(&meta.path, UNKNOWN_ATTRIBUTE));
            }
            Ok(())
        })
        .map_err(with_ace_prefix)?;
    }
    Ok(parsed)
}

pub(crate) fn parse_type(attrs: &[Attribute]) -> syn::Result<TypeAttrs> {
    let mut parsed = TypeAttrs::default();
    let mut seen_reserved = false;
    for attr in attrs.iter().filter(|attr| attr.path().is_ident("ace")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("reserved") {
                reject_duplicate(seen_reserved, &meta.path)?;
                seen_reserved = true;
                let content;
                parenthesized!(content in meta.input);
                while !content.is_empty() {
                    parsed.reserved.push(parse_order_number(&content)?);
                    if !content.is_empty() {
                        content.parse::<Token![,]>()?;
                    }
                }
            } else if meta.path.is_ident("default") {
                reject_duplicate(parsed.default.is_some(), &meta.path)?;
                parsed.default = Some(meta.value()?.parse()?);
            } else if meta.path.is_ident("order") || meta.path.is_ident("skip") {
                return Err(syn::Error::new_spanned(&meta.path, INVALID_POSITION));
            } else {
                return Err(syn::Error::new_spanned(&meta.path, UNKNOWN_ATTRIBUTE));
            }
            Ok(())
        })
        .map_err(with_ace_prefix)?;
    }
    Ok(parsed)
}

pub(crate) fn parse_variant(attrs: &[Attribute]) -> syn::Result<VariantAttrs> {
    let mut parsed = VariantAttrs::default();
    for attr in attrs.iter().filter(|attr| attr.path().is_ident("ace")) {
        // 类型/字段位置的属性出现在变体上时报整个属性的 span，与既有变体位置诊断一致。
        let position_error = || syn::Error::new_spanned(attr, INVALID_POSITION);
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("tag") {
                reject_duplicate(parsed.tag.is_some(), &meta.path)?;
                parsed.tag = Some(parse_tag(meta.value()?)?);
            } else if meta.path.is_ident("type_name") {
                reject_duplicate(parsed.type_name.is_some(), &meta.path)?;
                parsed.type_name = Some(meta.value()?.parse::<LitStr>()?);
            } else if meta.path.is_ident("order")
                || meta.path.is_ident("skip")
                || meta.path.is_ident("reserved")
                || meta.path.is_ident("default")
            {
                return Err(position_error());
            } else {
                return Err(syn::Error::new_spanned(&meta.path, UNKNOWN_ATTRIBUTE));
            }
            Ok(())
        })
        .map_err(with_ace_prefix)?;
    }
    Ok(parsed)
}

/// 解析变体 tag 字面量：非负、可用 u16 表示且小于 65535（FR-D05）。
fn parse_tag(input: ParseStream) -> syn::Result<u16> {
    if input.peek(Token![-]) {
        return Err(input.error(TAG_OUT_OF_RANGE));
    }
    let literal: LitInt = input
        .parse()
        .map_err(|error| syn::Error::new(error.span(), EXPECTED_INTEGER))?;
    let value = literal
        .base10_parse::<i64>()
        .map_err(|_| syn::Error::new(literal.span(), TAG_OUT_OF_RANGE))?;
    u16::try_from(value)
        .ok()
        .filter(|&tag| tag < 65535)
        .ok_or_else(|| syn::Error::new(literal.span(), TAG_OUT_OF_RANGE))
}

fn reject_duplicate(seen: bool, path: &Path) -> syn::Result<()> {
    if seen {
        Err(syn::Error::new_spanned(path, DUPLICATE_ATTRIBUTE))
    } else {
        Ok(())
    }
}

/// syn 直接返回的错误不带 `ace: ` 前缀；已带前缀的诊断原样保留，span 不变。
pub(crate) fn with_ace_prefix(error: syn::Error) -> syn::Error {
    if error.to_string().starts_with("ace: ") {
        error
    } else {
        syn::Error::new(error.span(), format!("ace: {error}"))
    }
}

/// 解析非负编号字面量；带负号、非整数字面量或超出 `i32::MAX` 的值分别报错。
fn parse_order_number(input: ParseStream) -> syn::Result<u32> {
    if input.peek(Token![-]) {
        return Err(input.error(ORDER_OUT_OF_RANGE));
    }
    let literal: LitInt = input
        .parse()
        .map_err(|error| syn::Error::new(error.span(), EXPECTED_INTEGER))?;
    let value = literal
        .base10_parse::<i32>()
        .map_err(|_| syn::Error::new(literal.span(), ORDER_OUT_OF_RANGE))?;
    u32::try_from(value).map_err(|_| syn::Error::new(literal.span(), ORDER_OUT_OF_RANGE))
}
