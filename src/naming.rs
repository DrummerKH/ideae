//! Branch names from task titles. A title starting with an issue key gives
//! `DEV-123_camelCaseSummary`; any other a slug with a random suffix, so two
//! tasks with the same title never collide: `adding-admob-ads-fj3yhvbd`.

const SLUG_MAX: usize = 40;

/// How a keyed title's branch is named unless a layer says otherwise
/// (`layers`, `branch`): `{key}`, `{summary}` (camelCase words), `{slug}`
/// (dashed words), `{rand}`.
pub const DEFAULT_TEMPLATE: &str = "{key}_{summary}";

#[cfg(test)]
pub fn branch_name(title: &str) -> String {
    build(title, &random_suffix(), DEFAULT_TEMPLATE)
}

/// A branch for a title, by a project's template.
pub fn branch_name_as(title: &str, template: &str) -> String {
    build(title, &random_suffix(), template)
}

/// The branch name shown while the title is typed; the random part is a
/// placeholder.
pub fn preview(title: &str, template: &str) -> String {
    build(title, "…", template)
}

/// A template git can take a branch of: one with `{key}` in it, whose
/// branch for a key and words git takes.
pub fn template_problem(template: &str) -> Option<&'static str> {
    if !template.contains("{key}") {
        return Some("It needs {key}: the tracker finds the branch by it.");
    }
    branch_problem(&fill(template, "DEV-1", "fixIt", "fix-it", "abc12345"))
}

fn fill(template: &str, key: &str, summary: &str, slug: &str, rand: &str) -> String {
    let out = template.replace("{key}", key).replace("{summary}", summary).replace("{slug}", slug).replace("{rand}", rand);
    // A part left empty takes its separator with it: `DEV-5_` is `DEV-5`.
    out.trim_matches(|c| matches!(c, '_' | '-' | '/' | '.')).to_string()
}

fn build(title: &str, suffix: &str, template: &str) -> String {
    let title = title.trim();
    if let Some((key, rest)) = split_key(title) {
        let words = words(rest);
        let template = if template_problem(template).is_none() { template } else { DEFAULT_TEMPLATE };
        let slug: String = words.join("-").chars().take(SLUG_MAX).collect();
        return fill(template, &key, &camel(&words), &slug, suffix);
    }
    let mut slug = String::new();
    for word in words(title) {
        if !slug.is_empty() && slug.len() + 1 + word.len() > SLUG_MAX {
            break;
        }
        if !slug.is_empty() {
            slug.push('-');
        }
        slug.push_str(&word);
    }
    slug.truncate(SLUG_MAX);
    if slug.is_empty() { format!("task-{suffix}") } else { format!("{slug}-{suffix}") }
}

/// Why git would not take `name` as a branch (its check-ref-format rules),
/// or None when it would.
pub fn branch_problem(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("A branch needs a name.");
    }
    if name.chars().any(|c| c.is_whitespace() || c.is_control() || "~^:?*[\\".contains(c)) {
        return Some("No spaces, nor any of ~ ^ : ? * [ \\");
    }
    if name.contains("..") || name.contains("@{") || name == "@" {
        return Some("No \"..\" or \"@{\" in it, and not \"@\" alone.");
    }
    if name.starts_with('-') {
        return Some("It cannot start with \"-\".");
    }
    if name.split('/').any(|part| part.is_empty() || part.starts_with('.') || part.ends_with(".lock")) {
        return Some("No empty part between slashes, none starting with \".\" or ending in \".lock\".");
    }
    if name.ends_with('.') {
        return Some("It cannot end with \".\".");
    }
    None
}

/// `DEV-123 summary`, `dev-123: summary`, `[DEV-123] summary`.
fn split_key(title: &str) -> Option<(String, &str)> {
    let t = title.trim_start_matches('[');
    let letters = t.chars().take_while(|c| c.is_ascii_alphanumeric()).count();
    let (project, rest) = t.split_at(letters);
    if project.is_empty() || !project.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return None;
    }
    let rest = rest.strip_prefix('-')?;
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let (number, rest) = rest.split_at(digits);
    if rest.starts_with(|c: char| c.is_alphanumeric()) {
        return None;
    }
    Some((format!("{}-{number}", project.to_ascii_uppercase()), rest))
}

/// Lowercase ASCII words, Cyrillic transliterated.
fn words(text: &str) -> Vec<String> {
    let mut ascii = String::new();
    for c in text.chars() {
        let lower: String = c.to_lowercase().collect();
        match translit(&lower) {
            Some(latin) => ascii.push_str(latin),
            None if c.is_ascii_alphanumeric() => ascii.push_str(&lower),
            None => ascii.push(' '),
        }
    }
    ascii.split_whitespace().map(str::to_string).collect()
}

fn camel(words: &[String]) -> String {
    let mut out = String::new();
    for (i, word) in words.iter().enumerate() {
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            if i == 0 {
                out.push(first);
            } else {
                out.extend(first.to_uppercase());
            }
            out.push_str(chars.as_str());
        }
    }
    out
}

fn translit(c: &str) -> Option<&'static str> {
    Some(match c {
        "а" => "a",
        "б" => "b",
        "в" => "v",
        "г" | "ґ" => "g",
        "д" => "d",
        "е" | "ё" | "э" => "e",
        "є" => "ye",
        "ж" => "zh",
        "з" => "z",
        "и" | "і" => "i",
        "ї" => "yi",
        "й" | "ы" => "y",
        "к" => "k",
        "л" => "l",
        "м" => "m",
        "н" => "n",
        "о" => "o",
        "п" => "p",
        "р" => "r",
        "с" => "s",
        "т" => "t",
        "у" => "u",
        "ф" => "f",
        "х" => "kh",
        "ц" => "ts",
        "ч" => "ch",
        "ш" => "sh",
        "щ" => "shch",
        "ъ" | "ь" => "",
        "ю" => "yu",
        "я" => "ya",
        _ => return None,
    })
}

fn random_suffix() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    let mut n = h.finish();
    let alphabet = b"abcdefghijklmnopqrstuvwxyz0123456789";
    (0..8)
        .map(|_| {
            let c = alphabet[(n % alphabet.len() as u64) as usize] as char;
            n /= alphabet.len() as u64;
            c
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names_git_takes() {
        for good in ["DEV-1_fix", "feature/a-b", "fix-ab12cd34", &branch_name("Some title")] {
            assert_eq!(branch_problem(good), None, "{good}");
        }
        for bad in ["", "a b", "a..b", "-x", "a/", "/a", "a//b", "a/.b", "x.lock", "a.", "a~1", "@", "a@{1}", "a:b"] {
            assert!(branch_problem(bad).is_some(), "{bad}");
        }
    }

    #[test]
    fn keyed_titles() {
        assert_eq!(branch_name("DEV-26368 php 8.5 float fix"), "DEV-26368_php85FloatFix");
        assert_eq!(branch_name("dev-12: Fix the Login"), "DEV-12_fixTheLogin");
        assert_eq!(branch_name("[ABC-7] Чиним вход"), "ABC-7_chinimVkhod");
        assert_eq!(branch_name("DEV-5"), "DEV-5");
    }

    #[test]
    fn a_project_s_template() {
        assert_eq!(branch_name_as("DEV-12 Fix the login", "feature/{key}-{slug}"), "feature/DEV-12-fix-the-login");
        assert_eq!(branch_name_as("DEV-12", "feature/{key}-{slug}"), "feature/DEV-12");
        assert_eq!(branch_name_as("DEV-12 Fix", "{summary}"), "DEV-12_fix", "no key in it: the default");
        assert_eq!(branch_name_as("DEV-12 Fix", "a b {key}"), "DEV-12_fix", "one git would not take: the default");
        assert!(branch_name_as("Fix the login", "feature/{key}").starts_with("fix-the-login-"), "no key: a slug as before");
        assert_eq!(preview("DEV-3 x", "{key}"), "DEV-3");
        assert!(template_problem("{summary}").is_some() && template_problem("{key}_{summary}").is_none());
    }

    #[test]
    fn slugs() {
        let b = branch_name("Adding AdMob ads!");
        assert!(b.starts_with("adding-admob-ads-"), "{b}");
        assert_eq!(b.len(), "adding-admob-ads-".len() + 8);
        assert!(branch_name("Добавить рекламу").starts_with("dobavit-reklamu-"));
        assert!(branch_name("!!!").starts_with("task-"));
        assert!(branch_name("DEVELOP-feature now").starts_with("develop-feature-now-"));
        let long = branch_name(&"word ".repeat(30));
        assert!(long.len() <= SLUG_MAX + 9, "{long}");
    }
}
