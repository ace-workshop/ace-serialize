use crate::attr;
use quote::format_ident;
use std::collections::BTreeSet;
use syn::{
    punctuated::Punctuated, token::Comma, Field, GenericArgument, Ident, PathArguments,
    PathSegment, Type, TypePath,
};

pub(crate) const NAMED_FIELDS_ONLY: &str = "ace: S1 仅支持具名字段结构体";
const UNSUPPORTED_TYPE: &str = "ace: unsupported field type";
const UNSUPPORTED_SIZE_TYPE: &str =
    "ace: unsupported field type; isize and usize have no fixed wire width";
const OWNED_CELL: &str =
    "ace: owned RefCell/RwLock unsupported; use Rc<RefCell<T>> or Arc<RwLock<T>>";
const SCALAR_TYPES: [&str; 12] = [
    "bool", "char", "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64", "f32", "f64",
];
/// FR-T01 未列出的具名类型（闭合列表）；派生期逐处诊断，避免下游 E0277。
const UNSUPPORTED_STD_TYPES: [&str; 6] =
    ["i128", "u128", "HashMap", "HashSet", "BTreeSet", "VecDeque"];

pub(crate) enum Kind {
    /// 可省略的标量；携带 `schema::value::is_default_<类型>` 的名字。
    Scalar(Ident),
    /// 可省略的 `Option<T>`，缺席解码为 `None`。
    Option,
    /// 必写字段，缺席解码返回 `MissingRequiredField`。
    Required,
    /// 不写不读，构造时取 `Default::default()`，仍占用其编号。
    Skip,
}

pub(crate) struct FieldInfo {
    pub(crate) ident: Ident,
    pub(crate) ty: Type,
    pub(crate) order: u32,
    pub(crate) kind: Kind,
    /// 字段类型语法中含类型参数；生成的 impl 需为其加 where 谓词。
    pub(crate) generic: bool,
}

pub(crate) struct Fields {
    pub(crate) items: Vec<FieldInfo>,
    /// 任一线上字段含 `Rc`/`Arc`/`Weak` 路径段，结构体只生成循环路径所需的 impl。
    pub(crate) shared: bool,
}

pub(crate) fn collect(
    fields: &Punctuated<Field, Comma>,
    type_params: &[Ident],
    reserved: &[u32],
) -> syn::Result<Fields> {
    let mut errors = Errors::default();
    let mut seen = BTreeSet::new();
    let mut items = Vec::new();
    let mut shared = false;
    for (index, field) in fields.iter().enumerate() {
        let Some(ident) = field.ident.clone() else {
            errors.push(syn::Error::new_spanned(field, NAMED_FIELDS_ONLY));
            continue;
        };
        let attrs = match attr::parse_field(&field.attrs) {
            Ok(attrs) => attrs,
            Err(error) => {
                errors.push(error);
                continue;
            }
        };
        let order = attrs.order.unwrap_or(index as u32);
        if reserved.contains(&order) {
            errors.push(syn::Error::new_spanned(
                &ident,
                format!("ace: order {order} is reserved"),
            ));
        }
        if !seen.insert(order) {
            errors.push(syn::Error::new_spanned(
                &ident,
                format!("ace: duplicate order {order}"),
            ));
        }
        let kind = if attrs.skip {
            Kind::Skip
        } else {
            let mut facts = Facts::default();
            inspect(&field.ty, false, &mut facts, &mut errors);
            if facts.cell {
                errors.push(syn::Error::new_spanned(&field.ty, OWNED_CELL));
            }
            shared |= facts.shared;
            kind_of(&field.ty)
        };
        let generic = mentions_type_param(&field.ty, type_params);
        items.push(FieldInfo {
            ident,
            ty: field.ty.clone(),
            order,
            kind,
            generic,
        });
    }
    errors.into_result()?;
    Ok(Fields { items, shared })
}

/// 同一结构体内收集多个诊断；合并后一次性返回，避免只报告首个问题。
#[derive(Default)]
struct Errors(Option<syn::Error>);

impl Errors {
    fn push(&mut self, error: syn::Error) {
        if let Some(existing) = self.0.as_mut() {
            existing.combine(error);
        } else {
            self.0 = Some(error);
        }
    }

    fn into_result(self) -> syn::Result<()> {
        self.0.map_or(Ok(()), Err)
    }
}

#[derive(Default)]
struct Facts {
    shared: bool,
    cell: bool,
}

/// 按类型语法逐层检查：只接受路径类型与括号包裹，其余形式与不支持的整数类型逐处诊断。
/// `shared_pointee` 为真表示 `ty` 是 `Rc`、`Arc` 或 `Weak` 的直接泛型参数：其中的 `RefCell`、`RwLock` 是共享单元，
/// 其它位置出现的 `RefCell`、`RwLock` 计为拥有型。
fn inspect(ty: &Type, shared_pointee: bool, facts: &mut Facts, errors: &mut Errors) {
    match ty {
        Type::Path(path) => inspect_path(path, shared_pointee, facts, errors),
        Type::Paren(paren) => inspect(&paren.elem, shared_pointee, facts, errors),
        Type::Group(group) => inspect(&group.elem, shared_pointee, facts, errors),
        _ => errors.push(syn::Error::new_spanned(ty, UNSUPPORTED_TYPE)),
    }
}

fn inspect_path(path: &TypePath, shared_pointee: bool, facts: &mut Facts, errors: &mut Errors) {
    if path.qself.is_some() {
        errors.push(syn::Error::new_spanned(path, UNSUPPORTED_TYPE));
        return;
    }
    for segment in &path.path.segments {
        let name = segment.ident.to_string();
        let shared_pointer = matches!(name.as_str(), "Rc" | "Arc" | "Weak");
        facts.shared |= shared_pointer;
        if !shared_pointee && matches!(name.as_str(), "RefCell" | "RwLock") {
            facts.cell = true;
        }
        match &segment.arguments {
            PathArguments::None => {}
            PathArguments::AngleBracketed(arguments) => {
                for argument in &arguments.args {
                    match argument {
                        GenericArgument::Type(inner) => {
                            inspect(inner, shared_pointer, facts, errors)
                        }
                        GenericArgument::Lifetime(_) => {}
                        other => errors.push(syn::Error::new_spanned(other, UNSUPPORTED_TYPE)),
                    }
                }
            }
            PathArguments::Parenthesized(_) => {
                errors.push(syn::Error::new_spanned(segment, UNSUPPORTED_TYPE));
            }
        }
    }
    if is_size_type(path) {
        errors.push(syn::Error::new_spanned(path, UNSUPPORTED_SIZE_TYPE));
    }
    if let Some(name) = unsupported_std_type(path) {
        errors.push(syn::Error::new_spanned(
            path,
            format!("ace: unsupported field type; {name} is not in the FR-T01 supported set"),
        ));
    }
}

fn unsupported_std_type(path: &TypePath) -> Option<&Ident> {
    let segment = path.path.segments.last()?;
    UNSUPPORTED_STD_TYPES
        .contains(&segment.ident.to_string().as_str())
        .then_some(&segment.ident)
}

fn is_size_type(path: &TypePath) -> bool {
    let Some(segment) = primitive_segment(path) else {
        return false;
    };
    matches!(segment.ident.to_string().as_str(), "isize" | "usize")
}

/// 解开括号与分组，使 `(T)` 与 `T` 的判定一致；只用于判定，用户写出的类型 token 不改写。
pub(crate) fn unwrap_group(ty: &Type) -> &Type {
    match ty {
        Type::Paren(paren) => unwrap_group(&paren.elem),
        Type::Group(group) => unwrap_group(&group.elem),
        _ => ty,
    }
}

fn single_type_argument(segment: &PathSegment) -> Option<&Type> {
    let PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    if arguments.args.len() != 1 {
        return None;
    }
    match arguments.args.first()? {
        GenericArgument::Type(ty) => Some(ty),
        _ => None,
    }
}

fn kind_of(ty: &Type) -> Kind {
    if let Type::Path(path) = unwrap_group(ty) {
        if let Some(ident) = scalar_ident(path) {
            return Kind::Scalar(format_ident!("is_default_{}", ident));
        }
        if let Some(segment) = path.path.segments.last() {
            if segment.ident == "Option" && single_type_argument(segment).is_some() {
                return Kind::Option;
            }
        }
    }
    Kind::Required
}

fn scalar_ident(path: &TypePath) -> Option<&Ident> {
    let segment = primitive_segment(path)?;
    SCALAR_TYPES
        .contains(&segment.ident.to_string().as_str())
        .then_some(&segment.ident)
}

/// 原生类型路径的末段：单段名称，或 `core::primitive::X`、`std::primitive::X`（可带前导 `::`）。
/// 其它多段路径不是原生类型，例如用户模块内的同名类型，不参与标量分类与 isize、usize 拒绝。
fn primitive_segment(path: &TypePath) -> Option<&PathSegment> {
    if path.qself.is_some() {
        return None;
    }
    let segments: Vec<&PathSegment> = path.path.segments.iter().collect();
    let segment = match segments.as_slice() {
        [only] => *only,
        [root, module, segment]
            if (root.ident == "core" || root.ident == "std") && module.ident == "primitive" =>
        {
            *segment
        }
        _ => return None,
    };
    matches!(segment.arguments, PathArguments::None).then_some(segment)
}

fn mentions_type_param(ty: &Type, params: &[Ident]) -> bool {
    match ty {
        Type::Path(path) => {
            path.qself
                .as_ref()
                .is_some_and(|qself| mentions_type_param(&qself.ty, params))
                || path.path.segments.iter().any(|segment| {
                    params.contains(&segment.ident)
                        || match &segment.arguments {
                            PathArguments::AngleBracketed(arguments) => {
                                arguments.args.iter().any(|argument| {
                                    matches!(
                                        argument,
                                        GenericArgument::Type(inner)
                                            if mentions_type_param(inner, params)
                                    )
                                })
                            }
                            _ => false,
                        }
                })
        }
        Type::Paren(paren) => mentions_type_param(&paren.elem, params),
        Type::Group(group) => mentions_type_param(&group.elem, params),
        Type::Tuple(tuple) => tuple
            .elems
            .iter()
            .any(|elem| mentions_type_param(elem, params)),
        Type::Array(array) => mentions_type_param(&array.elem, params),
        Type::Slice(slice) => mentions_type_param(&slice.elem, params),
        Type::Reference(reference) => mentions_type_param(&reference.elem, params),
        Type::Ptr(pointer) => mentions_type_param(&pointer.elem, params),
        _ => false,
    }
}
