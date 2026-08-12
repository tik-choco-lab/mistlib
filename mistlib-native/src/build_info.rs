use serde::Serialize;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Serialize)]
struct BuildInfo<'a> {
    version: &'a str,
    commit: &'a str,
    dirty: bool,
    profile: &'a str,
    target: &'a str,
}

pub fn get_version() -> &'static str {
    VERSION
}

pub fn get_build_info() -> String {
    let info = BuildInfo {
        version: VERSION,
        commit: env!("MISTLIB_BUILD_COMMIT"),
        dirty: env!("MISTLIB_BUILD_DIRTY") == "true",
        profile: env!("MISTLIB_BUILD_PROFILE"),
        target: env!("MISTLIB_BUILD_TARGET"),
    };

    serde_json::to_string(&info).expect("serializing static build information cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_matches_the_package_version() {
        assert_eq!(get_version(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn build_info_contains_the_version_and_target() {
        let value: serde_json::Value = serde_json::from_str(&get_build_info()).unwrap();
        assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(value["target"], env!("MISTLIB_BUILD_TARGET"));
        assert!(value["dirty"].is_boolean());
    }
}
