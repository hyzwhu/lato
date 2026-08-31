#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolKind {
    Read,
    Edit,
    Execute,
    Search,
    ListDir,
    Other,
}

pub struct ToolSpec {
    pub id: &'static str,
    pub kind: ToolKind,
}
