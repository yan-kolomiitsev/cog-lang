use cog::sdk::{Engine, Signature, Type, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut engine = Engine::new();
    engine.register(
        "host::double",
        Signature::new(vec![Type::Int], Type::Int),
        |_, arguments| {
            arguments[0]
                .as_int()?
                .checked_mul(2)
                .map(Value::int)
                .ok_or_else(|| cog::sdk::Error::new("integer overflow"))
        },
    )?;
    let program = engine.compile(
        "embedded.cog",
        "include std; use io; use host; fn main() { io::println(host::double(21)); }",
    )?;
    let program = engine.decode(&program.encode()?)?;
    drop(engine);
    program.run(&mut std::io::stdout())?;
    Ok(())
}
