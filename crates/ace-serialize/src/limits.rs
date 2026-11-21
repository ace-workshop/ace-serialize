use crate::error::Result;

/// 编解码资源限制与格式选项。字段已冻结，除 raw 预算外的使用点见后续 stage。
/// 数值上限为零表示该维度最严格的上限，而非禁用限制；深度零只允许根节点，
/// 槽数零不允许任何槽（包括根槽），记录数零不允许任何记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// 最大嵌套深度，根节点深度为零。
    pub max_depth: usize,
    /// 同时打开的身份构造闭包数上限；不计虚根和普通对象/集合深度，零表示不允许身份节点。
    pub max_build_nesting: usize,
    /// 输入层字节预算（压缩输入与 raw 分别计量）。
    pub max_input_bytes: usize,
    /// 解包后 raw 字节预算，包含 envelope。
    pub max_raw_bytes: usize,
    /// 物化层预算：每次实际物化 record 时累计其 size。
    pub max_decoded_bytes: usize,
    /// 输入的 record 数量上限。
    pub max_records: usize,
    /// 输入的 slot 数量上限，包含根槽。
    pub max_slots: usize,
    /// 单个集合的元素数或 Map 键值对数上限。
    pub max_collection_len: usize,
    /// 引用表条数上限。
    pub max_references: usize,
    /// Map 字符串键的 UTF-8 字节数上限。
    pub max_map_string_key_bytes: usize,
    /// 是否使用 packed 格式。
    pub packed: bool,
}

impl Default for Options {
    fn default() -> Self {
        let max_depth = 256;
        Self {
            max_depth,
            max_build_nesting: max_depth,
            max_input_bytes: 64 * 1024 * 1024,
            max_raw_bytes: 64 * 1024 * 1024,
            max_decoded_bytes: 64 * 1024 * 1024,
            max_records: 1 << 20,
            max_slots: 1 << 20,
            max_collection_len: 1 << 24,
            max_references: 1 << 20,
            max_map_string_key_bytes: 1024 * 1024,
            packed: true,
        }
    }
}

impl Options {
    /// 检查配置。零值上限合法，表示不允许该维度的正数用量。
    pub fn validate(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_options() {
        let opts = Options::default();
        let _: usize = opts.max_depth;
        assert_eq!(opts.max_depth, 256);
        let _: usize = opts.max_build_nesting;
        assert_eq!(opts.max_build_nesting, 256);
        assert_eq!(opts.max_build_nesting, opts.max_depth);
        assert_eq!(opts.max_input_bytes, 64 * 1024 * 1024);
        assert_eq!(opts.max_raw_bytes, 64 * 1024 * 1024);
        assert_eq!(opts.max_decoded_bytes, 64 * 1024 * 1024);
        assert_eq!(opts.max_records, 1 << 20);
        assert_eq!(opts.max_slots, 1 << 20);
        assert_eq!(opts.max_collection_len, 1 << 24);
        assert_eq!(opts.max_references, 1 << 20);
        assert_eq!(opts.max_map_string_key_bytes, 1024 * 1024);
        assert!(opts.packed);
        assert_eq!(opts.validate(), Ok(()));
    }

    #[test]
    fn custom_and_zero_options() {
        let opts = Options {
            max_depth: 3,
            max_build_nesting: 2,
            max_input_bytes: 101,
            max_raw_bytes: 102,
            max_decoded_bytes: 100,
            max_records: 4,
            max_slots: 5,
            max_collection_len: 6,
            max_references: 7,
            max_map_string_key_bytes: 8,
            packed: false,
        };
        assert_eq!(opts.validate(), Ok(()));
        assert!(!opts.packed);
        for zero in [
            Options {
                max_depth: 0,
                ..opts.clone()
            },
            Options {
                max_build_nesting: 0,
                ..opts.clone()
            },
            Options {
                max_input_bytes: 0,
                ..opts.clone()
            },
            Options {
                max_raw_bytes: 0,
                ..opts.clone()
            },
            Options {
                max_decoded_bytes: 0,
                ..opts.clone()
            },
            Options {
                max_records: 0,
                ..opts.clone()
            },
            Options {
                max_slots: 0,
                ..opts.clone()
            },
            Options {
                max_collection_len: 0,
                ..opts.clone()
            },
            Options {
                max_references: 0,
                ..opts.clone()
            },
            Options {
                max_map_string_key_bytes: 0,
                ..opts
            },
        ] {
            assert_eq!(zero.validate(), Ok(()));
        }
    }
}
