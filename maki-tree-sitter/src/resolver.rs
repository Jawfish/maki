use std::ops::Range;
use std::path::Path;

use thiserror::Error;
use tree_sitter::{Node, Parser};

use crate::Language;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum BlockResolveError {
    #[error("unsupported file type for syntax block resolution: {path}")]
    UnsupportedFile { path: String },
    #[error("line {line} is out of range (file has {line_count} lines)")]
    LineOutOfRange { line: usize, line_count: usize },
    #[error("no complete syntax block contains line {line}")]
    NoBlock { line: usize },
    #[error("line {line} is inside an ambiguous syntax error")]
    AmbiguousSyntax { line: usize },
    #[error("Tree-sitter failed to parse the source")]
    ParseFailed,
}

pub fn resolve_block(
    source: &str,
    path: &Path,
    line: usize,
) -> Result<Range<usize>, BlockResolveError> {
    let language = path
        .extension()
        .and_then(|extension| extension.to_str())
        .and_then(|extension| Language::from_extension(&extension.to_ascii_lowercase()))
        .ok_or_else(|| BlockResolveError::UnsupportedFile {
            path: path.display().to_string(),
        })?;
    let lines = line_ranges(source);
    let Some(target) = line.checked_sub(1).and_then(|index| lines.get(index)) else {
        return Err(BlockResolveError::LineOutOfRange {
            line,
            line_count: lines.len(),
        });
    };

    let mut parser = Parser::new();
    parser
        .set_language(&language.ts_language())
        .map_err(|_| BlockResolveError::ParseFailed)?;
    let tree = parser
        .parse(source, None)
        .ok_or(BlockResolveError::ParseFailed)?;
    let root = tree.root_node();
    let mut candidates = Vec::new();
    collect_candidates(root, language, source, target, &mut candidates);
    candidates.sort_by_key(|candidate| {
        candidate.end_byte() - attached_start(*candidate, language, source)
    });

    if let Some(node) = candidates.first().copied() {
        if node.has_error() {
            return Err(BlockResolveError::AmbiguousSyntax { line });
        }
        return Ok(attached_start(node, language, source)..node.end_byte());
    }
    if contains_error(root, target) {
        Err(BlockResolveError::AmbiguousSyntax { line })
    } else {
        Err(BlockResolveError::NoBlock { line })
    }
}

fn line_ranges(source: &str) -> Vec<Range<usize>> {
    if source.is_empty() {
        return Vec::new();
    }
    let mut starts = vec![0];
    starts.extend(
        source
            .match_indices('\n')
            .map(|(index, _)| index + 1)
            .filter(|start| *start < source.len()),
    );
    starts
        .iter()
        .enumerate()
        .map(|(index, start)| *start..starts.get(index + 1).copied().unwrap_or(source.len()))
        .collect()
}

fn collect_candidates<'tree>(
    node: Node<'tree>,
    language: Language,
    source: &str,
    target: &Range<usize>,
    candidates: &mut Vec<Node<'tree>>,
) {
    if eligible(language, node.kind()) && !inside_decorator(node) {
        let range = attached_start(node, language, source)..node.end_byte();
        if intersects(&range, target) {
            candidates.push(node);
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_candidates(child, language, source, target, candidates);
    }
}

fn intersects(left: &Range<usize>, right: &Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
}

fn inside_decorator(node: Node<'_>) -> bool {
    node.parent()
        .is_some_and(|parent| matches!(parent.kind(), "decorated_definition" | "export_statement"))
}

fn attached_start(node: Node<'_>, language: Language, source: &str) -> usize {
    let mut start = node.start_byte();
    let mut sibling = node.prev_named_sibling();
    while let Some(previous) = sibling {
        if !is_attached(previous, language, source, start) {
            break;
        }
        start = previous.start_byte();
        sibling = previous.prev_named_sibling();
    }
    start
}

fn is_attached(node: Node<'_>, language: Language, source: &str, next_start: usize) -> bool {
    if source[node.end_byte()..next_start]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        > 1
    {
        return false;
    }
    match node.kind() {
        "attribute_item" | "attribute" | "decorator" => true,
        "line_comment" | "block_comment" | "comment" => {
            let text = source[node.byte_range()].trim_start();
            match language {
                Language::Rust => {
                    text.starts_with("///")
                        || text.starts_with("//!")
                        || text.starts_with("/**")
                        || text.starts_with("/*!")
                }
                Language::TypeScript | Language::JavaScript => text.starts_with("/**"),
                _ => false,
            }
        }
        _ => false,
    }
}

fn contains_error(node: Node<'_>, target: &Range<usize>) -> bool {
    if (node.is_error() || node.is_missing())
        && intersects(
            &(node.start_byte()..node.end_byte().max(node.start_byte() + 1)),
            target,
        )
    {
        return true;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| contains_error(child, target))
}

fn eligible(language: Language, kind: &str) -> bool {
    match language {
        Language::Markdown => kind == "section",
        Language::Rust => matches!(
            kind,
            "function_item"
                | "struct_item"
                | "enum_item"
                | "union_item"
                | "impl_item"
                | "trait_item"
                | "mod_item"
                | "type_item"
                | "const_item"
                | "static_item"
                | "macro_definition"
        ),
        Language::Python | Language::Starlark => matches!(
            kind,
            "decorated_definition" | "function_definition" | "class_definition"
        ),
        Language::TypeScript | Language::JavaScript => matches!(
            kind,
            "function_declaration"
                | "generator_function_declaration"
                | "class_declaration"
                | "abstract_class_declaration"
                | "interface_declaration"
                | "type_alias_declaration"
                | "enum_declaration"
                | "method_definition"
                | "public_field_definition"
                | "ambient_declaration"
                | "export_statement"
        ),
        _ => matches!(
            kind,
            "function_declaration"
                | "function_definition"
                | "method_declaration"
                | "class_declaration"
                | "class_definition"
                | "interface_declaration"
                | "struct_declaration"
                | "enum_declaration"
                | "trait_declaration"
                | "impl_item"
                | "module"
                | "module_declaration"
                | "type_declaration"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use test_case::test_case;

    use super::{BlockResolveError, resolve_block};

    fn selected<'a>(source: &'a str, path: &str, line: usize) -> &'a str {
        &source[resolve_block(source, Path::new(path), line).unwrap()]
    }

    #[test]
    fn rust_selects_documented_attributed_function() {
        let source = "/// Explains café.\n#[inline]\nfn café() {\n    println!(\"hi\");\n}\n\nfn next() {}\n";
        assert_eq!(
            selected(source, "sample.rs", 4),
            "/// Explains café.\n#[inline]\nfn café() {\n    println!(\"hi\");\n}"
        );
        assert_eq!(
            selected(source, "sample.rs", 1),
            selected(source, "sample.rs", 4)
        );
    }

    #[test]
    fn python_decorator_belongs_to_function() {
        let source = "@route(\"/\")\ndef handler():\n    \"\"\"Attached documentation.\"\"\"\n    return 1\n\ndef next_one(): pass\n";
        assert_eq!(
            selected(source, "sample.py", 3),
            "@route(\"/\")\ndef handler():\n    \"\"\"Attached documentation.\"\"\"\n    return 1"
        );
    }

    #[test]
    fn typescript_selects_method_with_jsdoc_and_decorator() {
        let source = "class Service {\n  /** Fetches data. */\n  @memoize\n  fetch(): string {\n    return \"ok\";\n  }\n\n  stop() {}\n}\n";
        assert_eq!(
            selected(source, "sample.ts", 5),
            "/** Fetches data. */\n  @memoize\n  fetch(): string {\n    return \"ok\";\n  }"
        );
        assert_eq!(
            selected(source, "sample.ts", 2),
            selected(source, "sample.ts", 5)
        );
    }

    #[test]
    fn markdown_heading_selects_its_nested_section() {
        let source = "# Top\nintro\n\n## Child\nchild text\n\n### Nested\nnested text\n\n## Next\nnext text\n";
        assert_eq!(
            selected(source, "sample.md", 5),
            "## Child\nchild text\n\n### Nested\nnested text\n\n"
        );
        assert_eq!(
            selected(source, "sample.md", 8),
            "### Nested\nnested text\n\n"
        );
    }

    #[test]
    fn one_line_block_does_not_consume_adjacent_text() {
        let source = "fn first() {}\nfn second() {}\n";
        let range = resolve_block(source, Path::new("sample.rs"), 1).unwrap();
        assert_eq!(&source[range.clone()], "fn first() {}");
        assert!(source.is_char_boundary(range.start));
        assert!(source.is_char_boundary(range.end));
    }

    #[test_case("sample.txt", "fn main() {}", 1, BlockResolveError::UnsupportedFile { path: "sample.txt".to_owned() }; "unsupported_file")]
    #[test_case("sample.rs", "fn main() {}", 0, BlockResolveError::LineOutOfRange { line: 0, line_count: 1 }; "zero_line")]
    #[test_case("sample.rs", "fn main() {}", 2, BlockResolveError::LineOutOfRange { line: 2, line_count: 1 }; "line_past_end")]
    fn rejects_invalid_targets(path: &str, source: &str, line: usize, expected: BlockResolveError) {
        assert_eq!(resolve_block(source, Path::new(path), line), Err(expected));
    }

    #[test]
    fn rejects_ambiguous_error_at_target() {
        let source = "fn broken( {\nfn valid() {}\n";
        assert_eq!(
            resolve_block(source, Path::new("sample.rs"), 1),
            Err(BlockResolveError::AmbiguousSyntax { line: 1 })
        );
    }
}
