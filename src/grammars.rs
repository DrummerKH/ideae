//! Grammars gpui-kit does not bundle: XML (Android resources, manifests),
//! Java properties (also `.env`), Groovy (Gradle scripts), Dockerfiles,
//! Windows batch, INI, nginx, PowerShell, Twig, HCL (Terraform), and a batch more from their
//! crates. Which file gets which grammar: `lsp::language_for`.

use gpui_kit::component::highlighter::{LanguageConfig, LanguageRegistry};

const GROOVY_HIGHLIGHTS: &str = include_str!("../queries/groovy-highlights.scm");
/// From tree-sitter-julia's repository; the crate does not export it.
const JULIA_HIGHLIGHTS: &str = include_str!("../queries/julia-highlights.scm");
/// tree-sitter-solidity's, with one pattern this tree-sitter rejects rewritten.
const SOLIDITY_HIGHLIGHTS: &str = include_str!("../queries/solidity-highlights.scm");
const TWIG_HIGHLIGHTS: &str = include_str!("../vendor/tree-sitter-twig/queries/highlights.scm");

unsafe extern "C" {
    fn tree_sitter_twig() -> *const ();
}

fn twig() -> tree_sitter::Language {
    // SAFETY: the vendored parser.c defines this tree-sitter language function.
    tree_sitter::Language::new(unsafe { tree_sitter_language::LanguageFn::from_raw(tree_sitter_twig) })
}
/// From tree-sitter-nginx's repository; the crate does not export it.
const NGINX_HIGHLIGHTS: &str = include_str!("../queries/nginx-highlights.scm");
/// tree-sitter-hcl ships no highlights.
const HCL_HIGHLIGHTS: &str = include_str!("../queries/hcl-highlights.scm");
/// C++'s own query names only what C++ adds to C (gpui-kit uses it alone):
/// with C's after it, keywords, types and strings are colored too.
static CPP_HIGHLIGHTS: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| format!("{}\n{}", tree_sitter_cpp::HIGHLIGHT_QUERY, tree_sitter_c::HIGHLIGHT_QUERY));

/// (name, grammar, highlights) for everything registered here.
fn grammars() -> Vec<(&'static str, tree_sitter::Language, &'static str)> {
    vec![
        ("xml", tree_sitter_xml::LANGUAGE_XML.into(), tree_sitter_xml::XML_HIGHLIGHT_QUERY),
        ("properties", tree_sitter_properties::LANGUAGE.into(), tree_sitter_properties::HIGHLIGHTS_QUERY),
        ("groovy", tree_sitter_groovy::LANGUAGE.into(), GROOVY_HIGHLIGHTS),
        ("dockerfile", tree_sitter_containerfile::LANGUAGE.into(), tree_sitter_containerfile::HIGHLIGHTS_QUERY),
        ("batch", tree_sitter_batch::LANGUAGE.into(), tree_sitter_batch::HIGHLIGHTS_QUERY),
        ("ini", tree_sitter_ini::LANGUAGE.into(), tree_sitter_ini::HIGHLIGHTS_QUERY),
        ("nginx", tree_sitter_nginx::LANGUAGE.into(), NGINX_HIGHLIGHTS),
        ("powershell", tree_sitter_powershell::LANGUAGE.into(), tree_sitter_powershell::HIGHLIGHTS_QUERY),
        ("twig", twig(), TWIG_HIGHLIGHTS),
        ("less", tree_sitter_less::language(), tree_sitter_less::HIGHLIGHTS_QUERY),
        ("dart", tree_sitter_dart::LANGUAGE.into(), tree_sitter_dart::HIGHLIGHTS_QUERY),
        ("nix", tree_sitter_nix::LANGUAGE.into(), tree_sitter_nix::HIGHLIGHTS_QUERY),
        ("haskell", tree_sitter_haskell::LANGUAGE.into(), tree_sitter_haskell::HIGHLIGHTS_QUERY),
        ("r", tree_sitter_r::LANGUAGE.into(), tree_sitter_r::HIGHLIGHTS_QUERY),
        ("ocaml", tree_sitter_ocaml::LANGUAGE_OCAML.into(), tree_sitter_ocaml::HIGHLIGHTS_QUERY),
        ("erlang", tree_sitter_erlang::LANGUAGE.into(), tree_sitter_erlang::HIGHLIGHTS_QUERY),
        ("fish", tree_sitter_fish::language(), tree_sitter_fish::HIGHLIGHTS_QUERY),
        ("glsl", tree_sitter_glsl::LANGUAGE_GLSL.into(), tree_sitter_glsl::HIGHLIGHTS_QUERY),
        ("solidity", tree_sitter_solidity::LANGUAGE.into(), SOLIDITY_HIGHLIGHTS),
        ("julia", tree_sitter_julia::LANGUAGE.into(), JULIA_HIGHLIGHTS),
        ("elm", tree_sitter_elm::LANGUAGE.into(), tree_sitter_elm::HIGHLIGHTS_QUERY),
        ("gleam", tree_sitter_gleam::LANGUAGE.into(), tree_sitter_gleam::HIGHLIGHT_QUERY),
        ("prisma", tree_sitter_prisma::LANGUAGE.into(), tree_sitter_prisma::HIGHLIGHTS_QUERY),
        ("hcl", tree_sitter_hcl::LANGUAGE.into(), HCL_HIGHLIGHTS),
        ("cpp", tree_sitter_cpp::LANGUAGE.into(), CPP_HIGHLIGHTS.as_str()),
    ]
}

pub fn register() {
    let registry = LanguageRegistry::singleton();
    for (name, language, highlights) in grammars() {
        registry.register(name, &LanguageConfig::new(name, language, vec![], highlights, "", ""));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A query naming a node the grammar lacks fails to compile, and the
    /// highlighter then shows plain text: catch that here.
    #[test]
    fn queries_compile() {
        for (name, language, highlights) in grammars() {
            if let Err(e) = tree_sitter::Query::new(&language, highlights) {
                panic!("{name}: {e}");
            }
        }
    }

    /// Every language the editor names for a file gets colors, not plain text:
    /// a grammar registered with an empty query parses and shows nothing.
    #[test]
    fn languages_are_colored() {
        use gpui_kit::component::highlighter::{HighlightTheme, SyntaxHighlighter};
        register();
        let theme = HighlightTheme::default_dark();
        let samples = [
            ("a.yml", "a: 1\nb: \"x\" # c\n"),
            ("phpunit.xml.dist", "<a b=\"c\"><d>t</d></a>\n"),
            ("a.json", "{\"a\": 1}\n"),
            ("a.swift", "import Foundation\nfunc run() -> Int { return 1 }\n"),
            ("a.cs", "using System;\nclass A { int Run() { return 1; } }\n"),
            ("a.graphql", "query Q($id: ID!) { user(id: $id) { name } }\n"),
            ("a.proto", "syntax = \"proto3\";\nmessage A { string name = 1; }\n"),
            ("CMakeLists.txt", "project(a)\nadd_executable(a main.c)\n"),
            ("main.tf", "resource \"aws_s3_bucket\" \"b\" {\n  bucket = \"x\"\n  count  = 2\n}\n"),
            ("a.scala", "object A { def f(x: Int) = if (x > 0) 1 else 2 }\n"),
        ];
        for (path, code) in samples {
            let mut highlighter = SyntaxHighlighter::new(crate::lsp::language_for(std::path::Path::new(path)));
            highlighter.update(None, &gpui_kit::component::Rope::from(code), None);
            let styles = highlighter.styles(&(0..code.len()), &*theme);
            assert!(styles.iter().any(|s| s.1.color.is_some()), "{path} is plain");
        }
        let code = "int main() { return 0; }\n";
        let mut highlighter = SyntaxHighlighter::new(crate::lsp::language_for(std::path::Path::new("a.cpp")));
        highlighter.update(None, &gpui_kit::component::Rope::from(code), None);
        let at = code.find("return").unwrap();
        assert!(highlighter.styles(&(0..code.len()), &*theme).iter().any(|(r, s)| r.start == at && s.color.is_some()), "C++ keywords are plain");
        // `if` is a `@conditional` in Scala's query, a name themes lack.
        let code = samples.last().unwrap().1;
        let mut highlighter = SyntaxHighlighter::new("scala");
        highlighter.update(None, &gpui_kit::component::Rope::from(code), None);
        let at = code.find("if").unwrap();
        assert!(highlighter.styles(&(0..code.len()), &*theme).iter().any(|(r, s)| r.start == at && s.color.is_some()));
    }
}


