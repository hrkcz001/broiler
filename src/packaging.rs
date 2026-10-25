//! Packaging contract checks.
//!
//! The Scoop manifest is a shipped artifact with storage rules that are easy to break silently: a
//! dropped `persist` entry or a pre-seeded `data` directory changes where a user's configuration
//! lives and can shadow the files migration would otherwise copy. The checks here are deliberately
//! about the manifest text, not about a release: an actual archive, hash, bucket CI run and install
//! test remain separate gates.

#[cfg(test)]
mod tests {
    use serde_json::Value;

    fn template() -> Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("packaging")
            .join("scoop")
            .join("broiler.json.template");
        let text = std::fs::read_to_string(&path).expect("manifest template");
        serde_json::from_str(&text).expect("manifest template is JSON")
    }

    #[test]
    fn the_manifest_persists_the_data_directory() {
        let manifest = template();
        assert_eq!(
            manifest["persist"], "data",
            "Scoop must own the data directory, or an upgrade loses the user's configuration"
        );
    }

    #[test]
    fn the_manifest_prepares_the_marker_and_an_empty_data_directory() {
        let manifest = template();
        let hooks = manifest["pre_install"]
            .as_array()
            .expect("pre_install hooks");
        let joined = hooks
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<&str>>()
            .join("\n");
        assert!(
            joined.contains("broiler.portable"),
            "the marker is what selects portable storage; it must exist before the app runs"
        );
        assert!(
            joined.contains("'data'"),
            "the data directory must exist for Scoop's persist linking"
        );
        // Creating and linking data must not create or apply a profile.
        for forbidden in ["profiles", "settings.toml", "startup.launch_at_login"] {
            assert!(
                !joined.contains(forbidden),
                "install hooks must not seed {forbidden}"
            );
        }
    }

    #[test]
    fn the_manifest_installs_no_service_driver_or_elevated_step() {
        let manifest = template();
        let text = manifest.to_string();
        for forbidden in ["sudo", "RunAsAdmin", "elevate", "service", "driver"] {
            assert!(
                !text.contains(forbidden),
                "the manifest must not contain {forbidden}"
            );
        }
        assert_eq!(manifest["bin"], "broiler.exe");
        assert!(
            manifest["architecture"]["64bit"]["hash"].is_string(),
            "the hash is filled from the uploaded archive at release time"
        );
    }
}
