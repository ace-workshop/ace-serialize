use core::fmt;

/// Stable nonzero error codes from the C ABI. Code 0 denotes success, expressed
/// by `Ok(())` or another success branch; `from_code(0)` returns `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum Error {
    InvalidArgument = 1,
    TruncatedInput = 2,
    UnknownTypeKey = 3,
    InvalidTag = 4,
    DepthExceeded = 5,
    ResourceLimit = 6,
    InvalidReference = 7,
    InvalidUtf8 = 8,
    VersionMismatch = 9,
    PanicCaught = 10,
    Unsupported = 11,
    NotFound = 12,
    TypeMismatch = 13,
    CycleUnsupported = 14,
    MissingRequiredField = 15,
    /// Map 解码时同一 key 第二次出现（需求 R24）。Rust 独有码：C# 参考实现无对应项、
    /// C# 侧按通用失败处理；C 头常量与 FFI 映射随门 A 追加。
    DuplicateKey = 16,
}

impl Error {
    pub fn code(&self) -> i32 {
        *self as i32
    }

    pub fn from_code(code: i32) -> Option<Self> {
        Some(match code {
            1 => Self::InvalidArgument,
            2 => Self::TruncatedInput,
            3 => Self::UnknownTypeKey,
            4 => Self::InvalidTag,
            5 => Self::DepthExceeded,
            6 => Self::ResourceLimit,
            7 => Self::InvalidReference,
            8 => Self::InvalidUtf8,
            9 => Self::VersionMismatch,
            10 => Self::PanicCaught,
            11 => Self::Unsupported,
            12 => Self::NotFound,
            13 => Self::TypeMismatch,
            14 => Self::CycleUnsupported,
            15 => Self::MissingRequiredField,
            16 => Self::DuplicateKey,
            _ => return None,
        })
    }
}

impl From<Error> for i32 {
    fn from(error: Error) -> Self {
        error.code()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

pub type Result<T> = core::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::Error;

    #[test]
    fn error_codes_round_trip() {
        let variants = [
            Error::InvalidArgument,
            Error::TruncatedInput,
            Error::UnknownTypeKey,
            Error::InvalidTag,
            Error::DepthExceeded,
            Error::ResourceLimit,
            Error::InvalidReference,
            Error::InvalidUtf8,
            Error::VersionMismatch,
            Error::PanicCaught,
            Error::Unsupported,
            Error::NotFound,
            Error::TypeMismatch,
            Error::CycleUnsupported,
            Error::MissingRequiredField,
            Error::DuplicateKey,
        ];
        for (index, variant) in variants.iter().enumerate() {
            let code = index as i32 + 1;
            assert_eq!(variant.code(), code);
            assert_eq!(i32::from(*variant), code);
            assert_eq!(Error::from_code(code), Some(*variant));
            assert_eq!(Error::from_code(variant.code()), Some(*variant));
        }
        for code in [i32::MIN, -1, 0, 17, i32::MAX] {
            assert_eq!(Error::from_code(code), None);
        }
    }
}
