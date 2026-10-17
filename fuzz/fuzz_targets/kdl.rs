#![no_main]
use cog::config;
use libfuzzer_sys::fuzz_target;
use std::path::Path;

fuzz_target!(|data: &[u8]| {
    if data.len() > config::MAX_CONFIGURATION_BYTES + 1 {
        return;
    }
    if let Ok(source) = std::str::from_utf8(data) {
        // Each API uses the production size/depth preflight, KDL v2 parser,
        // schema checks and diagnostic paths. None opens referenced paths.
        let _ = config::package_from_source(Path::new("package.kdl"), source);
        let _ = config::dependencies_from_source(Path::new("dependencies.kdl"), source);
        let _ = config::targets_from_source(Path::new("build.kdl"), source);
        let _ = cog::sdk::PluginManifest::parse(source);
    }
});
