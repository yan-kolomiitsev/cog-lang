#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(source) = std::str::from_utf8(data) {
        let parsed = cog::frontend::parse("fuzz.cog", source);
        // Exercise AST destruction too; parsing alone cannot expose deep drops.
        drop(parsed);
    }
});
