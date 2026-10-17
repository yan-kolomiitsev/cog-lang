#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(program) = cog::bytecode::decode(data) {
        let encoded = cog::bytecode::encode(&program).expect("accepted program must encode");
        assert_eq!(encoded, data, "accepted bytecode must be canonical");
    }
    // Never execute mutated bytecode in the host process.
});
