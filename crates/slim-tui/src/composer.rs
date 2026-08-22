#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TextElement {
    Text(String),
    Paste { id: u64, payload: String },
}

pub const MAX_DRAFT_CHARS: usize = 1_048_576;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComposerError {
    DraftTooLarge,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Composer {
    elements: Vec<TextElement>,
    next_paste_id: u64,
}

impl Composer {
    pub fn insert_text(&mut self, text: impl Into<String>) {
        let _ = self.try_insert_text(text);
    }

    pub fn try_insert_text(&mut self, text: impl Into<String>) -> Result<(), ComposerError> {
        let text = text.into();
        if self.payload().chars().count() + text.chars().count() > MAX_DRAFT_CHARS {
            return Err(ComposerError::DraftTooLarge);
        }
        self.elements.push(TextElement::Text(text));
        Ok(())
    }

    pub fn paste(&mut self, payload: impl Into<String>) -> u64 {
        self.try_paste(payload).unwrap_or(self.next_paste_id)
    }

    pub fn try_paste(&mut self, payload: impl Into<String>) -> Result<u64, ComposerError> {
        let payload = payload.into();
        if self.payload().chars().count() + payload.chars().count() > MAX_DRAFT_CHARS {
            return Err(ComposerError::DraftTooLarge);
        }
        let id = self.next_paste_id;
        self.next_paste_id += 1;
        self.elements.push(TextElement::Paste { id, payload });
        Ok(id)
    }

    pub fn display(&self) -> String {
        self.display_for_width(80)
    }

    pub fn display_for_width(&self, width: usize) -> String {
        self.elements
            .iter()
            .map(|element| match element {
                TextElement::Text(text) => text.clone(),
                TextElement::Paste { id, payload } => {
                    let chars = payload.chars().count();
                    if !payload.contains('\n') && !payload.contains('\r') && chars <= width {
                        payload.clone()
                    } else {
                        format!("[Pasted Content {id} {chars} chars]")
                    }
                }
            })
            .collect()
    }

    pub fn payload(&self) -> String {
        self.elements
            .iter()
            .map(|element| match element {
                TextElement::Text(text) => text.clone(),
                TextElement::Paste { payload, .. } => payload.clone(),
            })
            .collect()
    }

    pub fn remove_last_element(&mut self) -> Option<TextElement> {
        self.elements.pop()
    }

    pub fn clear(&mut self) {
        self.elements.clear();
    }
}
