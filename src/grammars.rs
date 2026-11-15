//! Grammars gpui-kit does not bundle: XML (Android resources, manifests),
//! Java properties (also `.env`), Groovy (Gradle scripts), Dockerfiles,
//! Windows batch, INI, nginx and PowerShell.

use gpui_kit::component::highlighter::{LanguageConfig, LanguageRegistry};

const GROOVY_HIGHLIGHTS: &str = include_str!("../queries/groovy-highlights.scm");
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
