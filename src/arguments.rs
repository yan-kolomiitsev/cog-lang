//! Shared semantic argument binding for functions and constructors.
use crate::ast::{CallArgument, StructField};

pub(crate) struct BindingError {
    pub argument: Option<usize>,
    pub message: String,
}

pub(crate) struct Bindings {
    pub by_parameter: Vec<Option<usize>>,
    pub by_argument: Vec<Option<usize>>,
    pub errors: Vec<BindingError>,
}

pub(crate) fn bind(name: &str, fields: &[StructField], args: &[CallArgument]) -> Bindings {
    let mut result = Bindings {
        by_parameter: vec![None; fields.len()],
        by_argument: vec![None; args.len()],
        errors: vec![],
    };
    let mut named_seen = false;
    let mut positional = 0;
    for (i, arg) in args.iter().enumerate() {
        let index = if let Some(key) = &arg.name {
            named_seen = true;
            fields.iter().position(|field| &field.name == key)
        } else {
            if named_seen {
                result.errors.push(BindingError {
                    argument: Some(i),
                    message: format!("positional argument after named argument in `{name}`"),
                });
            }
            let index = positional;
            positional += 1;
            (index < fields.len()).then_some(index)
        };
        if let Some(index) = index {
            result.by_argument[i] = Some(index);
            if result.by_parameter[index].is_some() {
                result.errors.push(BindingError {
                    argument: Some(i),
                    message: format!("duplicate argument `{}` for `{name}`", fields[index].name),
                });
            } else {
                result.by_parameter[index] = Some(i);
            }
        } else {
            result.errors.push(BindingError {
                argument: Some(i),
                message: format!(
                    "unknown or excess argument {} for `{name}`",
                    arg.name.as_deref().unwrap_or("<positional>")
                ),
            });
        }
    }
    for (index, field) in fields.iter().enumerate() {
        if result.by_parameter[index].is_none() && field.default.is_none() {
            result.errors.push(BindingError {
                argument: None,
                message: format!("`{name}` expects argument `{}`", field.name),
            });
        }
    }
    result
}
