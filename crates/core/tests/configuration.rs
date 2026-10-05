//! CONFIGURATION.md documents every environment variable and instance setting
//! the server reads. This test reads the source, so nothing is listed twice.

const DOC: &str = include_str!("../../../CONFIGURATION.md");
const CONFIG: &str = include_str!("../src/config.rs");
const SETTINGS: &str = include_str!("../src/settings.rs");

/// String literals of `source` (between double quotes), outside its tests.
fn literals(source: &str) -> Vec<&str> {
    let code = source.split("#[cfg(test)]").next().unwrap_or(source);
    code.split('"').skip(1).step_by(2).collect()
}

#[test]
fn every_environment_variable_is_documented() {
    let vars: Vec<&str> = literals(CONFIG)
        .into_iter()
        .filter(|s| s.len() > 3 && s.chars().all(|c| c.is_ascii_uppercase() || c == '_'))
        .collect();
    assert!(
        vars.contains(&"PUBLIC_URL"),
        "the parser still finds variables"
    );
    for var in vars {
        assert!(
            DOC.contains(&format!("`{var}`")),
            "{var} is read by config.rs but not documented in CONFIGURATION.md"
        );
    }
}

#[test]
fn every_setting_is_documented() {
    // Keys are the string literals right after `&mut values,` in Settings::load.
    let keys: Vec<&str> = SETTINGS
        .split("&mut values,")
        .skip(1)
        .filter_map(|after| after.split('"').nth(1))
        .collect();
    assert!(
        keys.contains(&"presigned_url_lifetime_secs"),
        "the parser still finds settings"
    );
    for key in keys {
        assert!(
            DOC.contains(&format!("`{key}`")),
            "setting {key} is not documented in CONFIGURATION.md"
        );
    }
}
