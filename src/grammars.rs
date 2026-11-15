//! Grammars gpui-kit does not bundle: XML (Android resources, manifests),
//! Java properties (also `.env`), Groovy (Gradle scripts), Dockerfiles,
//! Windows batch, INI, nginx, PowerShell, Twig, and a batch more from their
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
        ("scss", tree_sitter_scss::language(), tree_sitter_scss::HIGHLIGHTS_QUERY),
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
}
