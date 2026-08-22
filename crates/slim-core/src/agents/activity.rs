#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Activity {
    Tool { name: String },
    Mcp { name: String },
    Child { id: String },
}
