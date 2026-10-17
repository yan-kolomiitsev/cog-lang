use cog::backend::{CompiledProgram, SourceMapEntry};
use cog::diagnostic::SourceLocation;
use cog::error::RuntimeError;
use cog::runtime::{RuntimeContext, VmCapabilities};
use cog::{bytecode, interpreter, project};

#[test]
fn source_maps_round_trip_and_locate_terminal_runtime_errors() {
    let source = "include std;\nfn main() {\n    1 / 0\n}\n";
    let program = project::compile_source("src/main.cog", source).unwrap();
    let encoded = bytecode::encode(&program).unwrap();
    assert_eq!(u16::from_le_bytes(encoded[4..6].try_into().unwrap()), 10);

    let decoded = bytecode::decode(&encoded).unwrap();
    assert_eq!(decoded.source_map, program.source_map);
    let division = decoded
        .instructions
        .iter()
        .position(|instruction| matches!(instruction, cog::instruction::Instruction::Div))
        .unwrap();
    assert_eq!(
        decoded.source_location(division),
        Some(&SourceLocation {
            file: "src/main.cog".into(),
            line: 3,
            column: 5,
        })
    );

    let mut output = Vec::new();
    let mut context = RuntimeContext::new(&mut output, VmCapabilities::default());
    let result = interpreter::run_with_context(&decoded, &mut context);
    assert!(
        matches!(result, Err(RuntimeError::DivisionByZero)),
        "{result:?}"
    );
    assert_eq!(
        context.last_source_location(),
        Some(&SourceLocation {
            file: "src/main.cog".into(),
            line: 3,
            column: 5,
        })
    );
}

#[test]
fn source_map_ranges_and_coordinates_are_validated_before_encoding_and_loading() {
    let mut program = CompiledProgram::new(vec![cog::instruction::Instruction::Halt]);
    program.source_map.push(SourceMapEntry {
        start: 0,
        end: 2,
        location: SourceLocation {
            file: "test.cog".into(),
            line: 1,
            column: 1,
        },
    });
    assert!(
        bytecode::encode(&program)
            .unwrap_err()
            .contains("source map")
    );

    program.source_map[0].end = 1;
    let mut encoded = bytecode::encode(&program).unwrap();
    let last = encoded.len() - 4;
    encoded[last..].copy_from_slice(&0u32.to_le_bytes());
    assert!(
        bytecode::decode(&encoded)
            .unwrap_err()
            .contains("source map")
    );
}
