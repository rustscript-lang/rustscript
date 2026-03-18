use std::fmt;

use super::{ParseError, SourceFlavor, frontends, parser, source_map::SourceMap};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatError {
    Parse(ParseError),
    UnsupportedFlavor(SourceFlavor),
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormatError::Parse(err) => write!(f, "{err}"),
            FormatError::UnsupportedFlavor(flavor) => {
                write!(f, "formatting is unsupported for {flavor:?} source")
            }
        }
    }
}

impl std::error::Error for FormatError {}

pub fn format_source(source: &str) -> Result<String, FormatError> {
    format_source_with_flavor(source, SourceFlavor::RustScript)
}

pub fn format_source_with_flavor(
    source: &str,
    flavor: SourceFlavor,
) -> Result<String, FormatError> {
    let Some(dialect) = frontends::parser_dialect_for_flavor(flavor) else {
        return Err(FormatError::UnsupportedFlavor(flavor));
    };

    let mut source_map = SourceMap::new();
    let source_id = source_map.add_source("<source>", source.to_string());
    parser::format_source(source, dialect)
        .map_err(|err| FormatError::Parse(err.with_line_span_from_source(&source_map, source_id)))
}

#[cfg(test)]
mod tests {
    use super::format_source_with_flavor;
    use crate::compiler::SourceFlavor;

    #[test]
    fn keeps_tail_expression_addition_on_one_line() {
        let input = "fn mix(seed) {\n    v\n        +\n        seed\n}\n";
        let formatted = format_source_with_flavor(input, SourceFlavor::RustScript)
            .expect("formatting should succeed");

        assert_eq!(formatted, "fn mix(seed) {\n    v + seed\n}\n");
    }

    #[test]
    fn adds_space_before_closure_literals_after_assignment() {
        let input = "let base = 7;\nlet add =|value| value + base;\n";
        let formatted = format_source_with_flavor(input, SourceFlavor::RustScript)
            .expect("formatting should succeed");

        assert_eq!(
            formatted,
            "let base = 7;\nlet add = |value| value + base;\n"
        );
    }

    #[test]
    fn adds_space_before_unary_bang_in_if_conditions() {
        let input = concat!(
            "use stdlib::rss::strings as string;\n\n",
            "let total = if!string::non_empty(\"rustscript\") => {\n",
            "    1\n",
            "} else => {\n",
            "    0\n",
            "};\n"
        );
        let formatted = format_source_with_flavor(input, SourceFlavor::RustScript)
            .expect("formatting should succeed");

        assert_eq!(
            formatted,
            concat!(
                "use stdlib::rss::strings as string;\n\n",
                "let total = if !string::non_empty(\"rustscript\") => {\n",
                "    1\n",
                "} else => {\n",
                "    0\n",
                "};\n"
            )
        );
    }

    #[test]
    fn adds_space_before_array_literals_after_assignment() {
        let input = "let a =[1, \"a\"];\n";
        let formatted = format_source_with_flavor(input, SourceFlavor::RustScript)
            .expect("formatting should succeed");

        assert_eq!(formatted, "let a = [1, \"a\"];\n");
    }

    #[test]
    fn adds_space_before_prefix_borrows_and_negation() {
        let input = "let neg=-1;\nlet borrow=&mut value;\nconsume(a,&b,neg);\n";
        let formatted = format_source_with_flavor(input, SourceFlavor::RustScript)
            .expect("formatting should succeed");

        assert_eq!(
            formatted,
            "let neg = -1;\nlet borrow = &mut value;\nconsume(a, &b, neg);\n"
        );
    }

    #[test]
    fn keeps_namespace_keyword_calls_tight_to_open_paren() {
        let input = "let regex_ok = re::match (\"(?i)^rustscript$\", \"RUSTSCRIPT\");\n";
        let formatted = format_source_with_flavor(input, SourceFlavor::RustScript)
            .expect("formatting should succeed");

        assert_eq!(
            formatted,
            "let regex_ok = re::match(\"(?i)^rustscript$\", \"RUSTSCRIPT\");\n"
        );
    }

    #[test]
    fn folds_long_comma_delimited_groups() {
        let input = concat!(
            "fn borrow_with_really_long_parameter_names(first_parameter, &second_parameter, third_parameter, fourth_parameter, fifth_parameter, sixth_parameter) {\n",
            "    call_with_really_long_parameter_names(first_parameter, &second_parameter, third_parameter, fourth_parameter, fifth_parameter, sixth_parameter)\n",
            "}\n"
        );
        let formatted = format_source_with_flavor(input, SourceFlavor::RustScript)
            .expect("formatting should succeed");

        assert_eq!(
            formatted,
            concat!(
                "fn borrow_with_really_long_parameter_names(\n",
                "    first_parameter,\n",
                "    &second_parameter,\n",
                "    third_parameter,\n",
                "    fourth_parameter,\n",
                "    fifth_parameter,\n",
                "    sixth_parameter\n",
                ") {\n",
                "    call_with_really_long_parameter_names(\n",
                "        first_parameter,\n",
                "        &second_parameter,\n",
                "        third_parameter,\n",
                "        fourth_parameter,\n",
                "        fifth_parameter,\n",
                "        sixth_parameter\n",
                "    )\n",
                "}\n"
            )
        );
    }

    #[test]
    fn keeps_generic_type_params_and_turbofish_tight() {
        let input = concat!(
            "struct Box < T > {\n",
            "    value: T\n",
            "}\n",
            "fn wrap < T >(value: Box < T >) {\n",
            "    let decoded = json::decode:: < Box < T > >(payload);\n",
            "    decoded\n",
            "}\n"
        );
        let formatted = format_source_with_flavor(input, SourceFlavor::RustScript)
            .expect("formatting should succeed");

        assert_eq!(
            formatted,
            concat!(
                "struct Box<T> {\n",
                "    value: T\n",
                "}\n",
                "fn wrap<T>(value: Box<T>) {\n",
                "    let decoded = json::decode::<Box<T>>(payload);\n",
                "    decoded\n",
                "}\n"
            )
        );
    }

    #[test]
    fn keeps_nested_generic_type_annotations_tight() {
        let input = concat!(
            "struct LinkState < V > {\n",
            "    nodes: map < LruNode < V > >,\n",
            "    head: int,\n",
            "    tail: int\n",
            "}\n",
            "let cache: LruCacheState < FeedCacheItem > = new:: < FeedCacheItem >(3);\n",
            "let relinked = append_existing_node:: < FeedCacheItem >(nodes, head, tail, id, node);\n"
        );
        let formatted = format_source_with_flavor(input, SourceFlavor::RustScript)
            .expect("formatting should succeed");

        assert_eq!(
            formatted,
            concat!(
                "struct LinkState<V> {\n",
                "    nodes: map<LruNode<V>>,\n",
                "    head: int,\n",
                "    tail: int\n",
                "}\n",
                "let cache: LruCacheState<FeedCacheItem> = new::<FeedCacheItem>(3);\n",
                "let relinked = append_existing_node::<FeedCacheItem>(nodes, head, tail, id, node);\n"
            )
        );
    }

    #[test]
    fn keeps_multi_param_generic_fields_tight_inside_struct_bodies() {
        let input = concat!(
            "struct LruGetResult < K, V > {\n",
            "    cache: LruCacheState < K,\n",
            "    V >,\n",
            "    found: bool,\n",
            "    value: V\n",
            "}\n"
        );
        let formatted = format_source_with_flavor(input, SourceFlavor::RustScript)
            .expect("formatting should succeed");

        assert_eq!(
            formatted,
            concat!(
                "struct LruGetResult<K, V> {\n",
                "    cache: LruCacheState<K, V>,\n",
                "    found: bool,\n",
                "    value: V\n",
                "}\n"
            )
        );
    }

    #[test]
    fn keeps_multi_param_generic_calls_tight_inside_collection_literals() {
        let input = concat!(
            "let out = {\n",
            "    cache: make_cache:: < K,\n",
            "    V >(limit, size),\n",
            "    found: false,\n",
            "    value: null\n",
            "};\n"
        );
        let formatted = format_source_with_flavor(input, SourceFlavor::RustScript)
            .expect("formatting should succeed");

        assert_eq!(
            formatted,
            concat!(
                "let out = {\n",
                "    cache: make_cache::<K, V>(limit, size),\n",
                "    found: false,\n",
                "    value: null\n",
                "};\n"
            )
        );
    }

    #[test]
    fn keeps_space_before_grouped_expression_after_assignment() {
        let input = "let mut next_node: LruNode<K, V> =(&next_nodes)[next_head];\n";
        let formatted = format_source_with_flavor(input, SourceFlavor::RustScript)
            .expect("formatting should succeed");

        assert_eq!(
            formatted,
            "let mut next_node: LruNode<K, V> = (&next_nodes)[next_head];\n"
        );
    }
}
