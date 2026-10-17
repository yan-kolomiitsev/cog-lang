use crate::value::Value;

#[derive(Debug, Clone)]
pub enum ArrayTemplate {
    Scalar(Value),
    Array(Vec<ArrayItem>),
}

#[derive(Debug, Clone)]
pub struct ArrayItem {
    pub template: ArrayTemplate,
    pub repeat: usize,
}
