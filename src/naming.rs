//! Branch names from task titles. A title starting with an issue key gives
//! `DEV-123_camelCaseSummary`; any other a slug with a random suffix, so two
//! tasks with the same title never collide: `adding-admob-ads-fj3yhvbd`.

const SLUG_MAX: usize = 40;

pub fn branch_name(title: &str) -> String {
    build(title, &random_suffix())
}

/// The branch name shown while the title is typed; the random part is a
/// placeholder.
pub fn preview(title: &str) -> String {
    build(title, "…")
}

fn build(title: &str, suffix: &str) -> String {
    let title = title.trim();
    if let Some((key, rest)) = split_key(title) {
        let summary = camel(&words(rest));
        return if summary.is_empty() { key } else { format!("{key}_{summary}") };
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
