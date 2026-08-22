#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImageCapabilities {
    pub supported: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ImageRender {
    Inline { id: String },
    Placeholder { label: String },
}

pub fn render_image(id: impl Into<String>, capabilities: ImageCapabilities) -> ImageRender {
    let id = id.into();
    if capabilities.supported {
        ImageRender::Inline { id }
    } else {
        ImageRender::Placeholder {
            label: format!("[image unavailable: {id}]"),
        }
    }
}
