//! Native builtin metadata shared by checking and lowering.
pub(crate) fn native_module(name: &str) -> bool {
    matches!(name, "err" | "build")
        || crate::runtime::names().any(|function| {
            function
                .split_once("::")
                .is_some_and(|(module, _)| module == name)
        })
}

pub(crate) fn native_modules() -> std::collections::BTreeSet<String> {
    ["err", "build"]
        .map(str::to_owned)
        .into_iter()
        .chain(
            crate::runtime::names()
                .filter_map(|name| name.split_once("::").map(|(module, _)| module.to_owned())),
        )
        .collect()
}

#[derive(Clone)]
pub(crate) enum Builtin {
    Native(crate::runtime::NativeImport),
    Len,
    Exclusive,
    ErrorKind,
    BuildInput,
}

impl Builtin {
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name {
            "len" => Some(Builtin::Len),
            "exclusive" => Some(Builtin::Exclusive),
            "err::kind" => Some(Builtin::ErrorKind),
            "build::include_str"
            | "build::include_bytes"
            | "build::try_include_str"
            | "build::try_include_bytes" => Some(Builtin::BuildInput),
            _ => crate::runtime::NativeImport::named(name)
                .ok()
                .map(Self::Native),
        }
    }

    // VM primitives retain explicit arities; runtime calls use linked signatures.
    pub(crate) fn arity(&self) -> usize {
        match self {
            Builtin::Native(import) => import.parameters().len(),
            _ => 1,
        }
    }

    pub(crate) fn instruction(&self) -> Option<crate::instruction::Instruction> {
        Some(match self {
            Builtin::Native(_) | Builtin::BuildInput => return None,
            Builtin::Len => crate::instruction::Instruction::Len,
            Builtin::Exclusive => crate::instruction::Instruction::Exclusive,
            Builtin::ErrorKind => crate::instruction::Instruction::ErrorKind,
        })
    }
}
