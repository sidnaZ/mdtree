//! Deterministic title-to-slug generation rules.

use std::collections::HashSet;

use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

use crate::Slug;

/// Controls whether renaming a node changes its canonical slug.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenameSlugPolicy {
    /// Keep the current slug, preserving canonical paths by default.
    Preserve,
    /// Generate a new unique slug from the new title.
    Regenerate,
}

/// Generates a normalized slug unique among the supplied sibling slugs.
///
/// Latin diacritics are decomposed and removed, ASCII letters are lowercased,
/// and runs of punctuation or whitespace become one hyphen. Other scripts are
/// omitted. A title with no remaining ASCII letters or digits uses `node`.
#[must_use]
pub fn generate_slug<'a>(title: &str, sibling_slugs: impl IntoIterator<Item = &'a Slug>) -> Slug {
    let occupied: HashSet<&str> = sibling_slugs.into_iter().map(Slug::as_str).collect();
    let base = normalize(title);

    if !occupied.contains(base.as_str()) {
        return Slug::from_normalized(base);
    }

    for suffix in 2_u64.. {
        let candidate = format!("{base}-{suffix}");
        if !occupied.contains(candidate.as_str()) {
            return Slug::from_normalized(candidate);
        }
    }

    unreachable!("u64 slug suffix space cannot be exhausted")
}

/// Applies the explicit slug policy for a node rename.
#[must_use]
pub fn slug_for_rename<'a>(
    current: &Slug,
    new_title: &str,
    sibling_slugs: impl IntoIterator<Item = &'a Slug>,
    policy: RenameSlugPolicy,
) -> Slug {
    match policy {
        RenameSlugPolicy::Preserve => current.clone(),
        RenameSlugPolicy::Regenerate => generate_slug(new_title, sibling_slugs),
    }
}

fn normalize(title: &str) -> String {
    let mut result = String::new();
    let mut separator_pending = false;

    for character in title
        .nfd()
        .filter(|character| !is_combining_mark(*character))
    {
        if character.is_ascii_alphanumeric() {
            if separator_pending && !result.is_empty() {
                result.push('-');
            }
            result.push(character.to_ascii_lowercase());
            separator_pending = false;
        } else if !result.is_empty() {
            separator_pending = true;
        }
    }

    if result.is_empty() {
        "node".into()
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::{generate_slug, slug_for_rename, RenameSlugPolicy};
    use crate::Slug;

    #[test]
    fn normalization_is_table_driven() {
        let cases = [
            ("Database Models", "database-models"),
            ("API: v2 / Routes!", "api-v2-routes"),
            ("Crème brûlée", "creme-brulee"),
            ("東京", "node"),
            ("  ---  ", "node"),
            ("already--spaced", "already-spaced"),
        ];

        for (title, expected) in cases {
            assert_eq!(generate_slug(title, []).as_str(), expected);
        }
    }

    #[test]
    fn collisions_use_first_available_numeric_suffix() {
        let siblings = ["database", "database-2", "database-4"]
            .map(|value| Slug::from_str(value).expect("fixture slug"));

        assert_eq!(generate_slug("Database", &siblings).as_str(), "database-3");
    }

    #[test]
    fn rename_preserves_slug_unless_regeneration_is_requested() {
        let current = Slug::from_str("stable-path").expect("fixture slug");
        let sibling = Slug::from_str("new-title").expect("fixture slug");

        assert_eq!(
            slug_for_rename(
                &current,
                "New Title",
                [&sibling],
                RenameSlugPolicy::Preserve
            ),
            current
        );
        assert_eq!(
            slug_for_rename(
                &current,
                "New Title",
                [&sibling],
                RenameSlugPolicy::Regenerate
            )
            .as_str(),
            "new-title-2"
        );
    }
}
