//! Password strength: zxcvbn, the same code (and dictionaries) on the server,
//! which enforces it, and in the browser, which shows it as the user types.

use serde::Serialize;

/// The floor kept at every strength level.
pub const MIN_LEN: usize = 8;

/// A password's strength.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Strength {
    /// zxcvbn's score, 0 (guessed at once) to 4 (very hard to guess).
    pub score: u8,
    /// What's wrong with it, as a code (`this_is_a_common_password`), if anything.
    pub warning: Option<String>,
    /// How to do better, as codes (`add_another_word_or_two`).
    pub suggestions: Vec<String>,
}

/// The password's strength. The account's email is always a user input: a
/// password made of it, or of its parts, scores low.
pub fn strength(password: &str, email: &str) -> Strength {
    let email = email.trim().to_lowercase();
    let mut inputs: Vec<&str> = email
        .split(|c: char| !c.is_alphanumeric())
        .filter(|part| !part.is_empty())
        .collect();
    inputs.push(&email);
    let entropy = zxcvbn::zxcvbn(password, &inputs);
    let feedback = entropy.feedback();
    Strength {
        score: entropy.score().into(),
        warning: feedback.and_then(|f| f.warning()).map(|w| code(&w)),
        suggestions: feedback
            .map(|f| f.suggestions().iter().map(code).collect())
            .unwrap_or_default(),
    }
}

/// Whether the password may be chosen: at least [`MIN_LEN`] characters, and
/// zxcvbn's score at least `level` (0: the length only).
pub fn acceptable(password: &str, email: &str, level: u8) -> bool {
    password.chars().count() >= MIN_LEN && (level == 0 || strength(password, email).score >= level)
}

/// A feedback variant as a stable code: its name in snake case.
fn code(variant: &impl std::fmt::Debug) -> String {
    let mut code = String::new();
    for c in format!("{variant:?}").chars() {
        if c.is_uppercase() && !code.is_empty() {
            code.push('_');
        }
        code.push(c.to_ascii_lowercase());
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scores_and_codes() {
        let weak = strength("password", "someone@example.com");
        assert_eq!(weak.score, 0);
        assert_eq!(weak.warning.as_deref(), Some("this_is_a_top10_password"));
        assert!(strength("correct horse battery staple", "a@example.com").score >= 3);
    }

    #[test]
    fn the_email_scores_low() {
        let email = "marguerite.vandenberghe@example.com";
        assert!(
            strength("vandenberghe1", "a@example.com").score
                > strength("vandenberghe1", email).score
        );
    }

    #[test]
    fn levels() {
        assert!(
            !acceptable("short1!", "a@example.com", 0),
            "the floor holds at 0"
        );
        assert!(acceptable("password", "a@example.com", 0));
        assert!(!acceptable("password", "a@example.com", 1));
        assert!(acceptable(
            "correct horse battery staple",
            "a@example.com",
            3
        ));
    }
}
