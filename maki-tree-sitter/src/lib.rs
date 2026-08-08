mod resolver;

pub use resolver::{BlockResolveError, resolve_block};

use tree_sitter::Language as TsLanguage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
    Python,
    TypeScript,
    JavaScript,
    Gleam,
    Go,
    Html,
    Java,
    C,
    Cpp,
    CSharp,
    Ruby,
    Php,
    Swift,
    Kotlin,
    Scala,
    Bash,
    Lua,
    Elixir,
    Markdown,
    Starlark,
    Zig,
    Nix,
    Dart,
    Sql,
    Toml,
    Yaml,
    Containerfile,
    Css,
    Hcl,
    Json,
    Make,
}

impl Language {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "rust" => Some(Self::Rust),
            "python" => Some(Self::Python),
            "typescript" => Some(Self::TypeScript),
            "javascript" => Some(Self::JavaScript),
            "gleam" => Some(Self::Gleam),
            "go" => Some(Self::Go),
            "html" => Some(Self::Html),
            "java" => Some(Self::Java),
            "c" => Some(Self::C),
            "cpp" => Some(Self::Cpp),
            "c_sharp" => Some(Self::CSharp),
            "ruby" => Some(Self::Ruby),
            "php" => Some(Self::Php),
            "swift" => Some(Self::Swift),
            "kotlin" => Some(Self::Kotlin),
            "scala" => Some(Self::Scala),
            "bash" => Some(Self::Bash),
            "lua" => Some(Self::Lua),
            "elixir" => Some(Self::Elixir),
            "markdown" => Some(Self::Markdown),
            "starlark" => Some(Self::Starlark),
            "zig" => Some(Self::Zig),
            "nix" => Some(Self::Nix),
            "dart" => Some(Self::Dart),
            "sql" => Some(Self::Sql),
            "toml" => Some(Self::Toml),
            "yaml" => Some(Self::Yaml),
            "containerfile" | "dockerfile" => Some(Self::Containerfile),
            "css" => Some(Self::Css),
            "hcl" => Some(Self::Hcl),
            "json" => Some(Self::Json),
            "make" => Some(Self::Make),
            _ => None,
        }
    }

    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "rs" => Some(Self::Rust),
            "py" | "pyi" => Some(Self::Python),
            "ts" | "tsx" => Some(Self::TypeScript),
            "js" | "jsx" | "mjs" | "cjs" => Some(Self::JavaScript),
            "gleam" => Some(Self::Gleam),
            "go" => Some(Self::Go),
            "html" | "htm" => Some(Self::Html),
            "java" => Some(Self::Java),
            "c" | "h" => Some(Self::C),
            "cpp" | "cc" | "cxx" | "hpp" | "hxx" | "hh" | "ixx" => Some(Self::Cpp),
            "cs" => Some(Self::CSharp),
            "rb" | "rake" | "gemspec" => Some(Self::Ruby),
            "php" => Some(Self::Php),
            "swift" => Some(Self::Swift),
            "kt" | "kts" => Some(Self::Kotlin),
            "scala" | "sc" => Some(Self::Scala),
            "sh" | "bash" | "zsh" => Some(Self::Bash),
            "lua" => Some(Self::Lua),
            "ex" | "exs" => Some(Self::Elixir),
            "md" | "markdown" => Some(Self::Markdown),
            "bzl" => Some(Self::Starlark),
            "zig" => Some(Self::Zig),
            "nix" => Some(Self::Nix),
            "dart" => Some(Self::Dart),
            "sql" => Some(Self::Sql),
            "toml" => Some(Self::Toml),
            "yaml" | "yml" => Some(Self::Yaml),
            "dockerfile" => Some(Self::Containerfile),
            "css" => Some(Self::Css),
            "hcl" | "tf" | "tfvars" => Some(Self::Hcl),
            "json" => Some(Self::Json),
            "mk" => Some(Self::Make),
            _ => None,
        }
    }

    pub fn ts_language(&self) -> TsLanguage {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Self::Gleam => tree_sitter_gleam::LANGUAGE.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            Self::Html => tree_sitter_html::LANGUAGE.into(),
            Self::Java => tree_sitter_java::LANGUAGE.into(),
            Self::C => tree_sitter_c::LANGUAGE.into(),
            Self::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Self::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            Self::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            Self::Php => tree_sitter_php::LANGUAGE_PHP.into(),
            Self::Swift => tree_sitter_swift::LANGUAGE.into(),
            Self::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
            Self::Scala => tree_sitter_scala::LANGUAGE.into(),
            Self::Bash => tree_sitter_bash::LANGUAGE.into(),
            Self::Lua => tree_sitter_lua::LANGUAGE.into(),
            Self::Elixir => tree_sitter_elixir::LANGUAGE.into(),
            Self::Markdown => tree_sitter_md::LANGUAGE.into(),
            Self::Starlark => tree_sitter_starlark::LANGUAGE.into(),
            Self::Zig => tree_sitter_zig::LANGUAGE.into(),
            Self::Nix => tree_sitter_nix::LANGUAGE.into(),
            Self::Dart => tree_sitter_dart::LANGUAGE.into(),
            Self::Sql => tree_sitter_sequel::LANGUAGE.into(),
            Self::Toml => tree_sitter_toml_ng::LANGUAGE.into(),
            Self::Yaml => tree_sitter_yaml::LANGUAGE.into(),
            Self::Containerfile => tree_sitter_containerfile::LANGUAGE.into(),
            Self::Css => tree_sitter_css::LANGUAGE.into(),
            Self::Hcl => tree_sitter_hcl::LANGUAGE.into(),
            Self::Json => tree_sitter_json::LANGUAGE.into(),
            Self::Make => tree_sitter_make::LANGUAGE.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::Language;

    #[test_case("rust", Language::Rust)]
    #[test_case("python", Language::Python)]
    #[test_case("typescript", Language::TypeScript)]
    #[test_case("javascript", Language::JavaScript)]
    #[test_case("c_sharp", Language::CSharp)]
    #[test_case("dockerfile", Language::Containerfile)]
    #[test_case("containerfile", Language::Containerfile)]
    #[test_case("make", Language::Make)]
    fn maps_language_names(name: &str, expected: Language) {
        assert_eq!(Language::from_name(name), Some(expected));
    }

    #[test_case("rs", Language::Rust)]
    #[test_case("pyi", Language::Python)]
    #[test_case("tsx", Language::TypeScript)]
    #[test_case("cjs", Language::JavaScript)]
    #[test_case("ixx", Language::Cpp)]
    #[test_case("gemspec", Language::Ruby)]
    #[test_case("zsh", Language::Bash)]
    #[test_case("markdown", Language::Markdown)]
    #[test_case("dockerfile", Language::Containerfile)]
    #[test_case("tfvars", Language::Hcl)]
    #[test_case("mk", Language::Make)]
    fn maps_language_extensions(extension: &str, expected: Language) {
        assert_eq!(Language::from_extension(extension), Some(expected));
    }

    #[test]
    fn every_language_provides_a_parser() {
        let languages = [
            Language::Rust,
            Language::Python,
            Language::TypeScript,
            Language::JavaScript,
            Language::Gleam,
            Language::Go,
            Language::Html,
            Language::Java,
            Language::C,
            Language::Cpp,
            Language::CSharp,
            Language::Ruby,
            Language::Php,
            Language::Swift,
            Language::Kotlin,
            Language::Scala,
            Language::Bash,
            Language::Lua,
            Language::Elixir,
            Language::Markdown,
            Language::Starlark,
            Language::Zig,
            Language::Nix,
            Language::Dart,
            Language::Sql,
            Language::Toml,
            Language::Yaml,
            Language::Containerfile,
            Language::Css,
            Language::Hcl,
            Language::Json,
            Language::Make,
        ];

        for language in languages {
            let mut parser = tree_sitter::Parser::new();
            parser.set_language(&language.ts_language()).unwrap();
        }
    }
}
