// Rendered UI components

use serde::{Deserialize, Serialize};

use crate::action::Action;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Component {
    Text(Text),
    Image(Image),
    Gallery(Gallery),
    Button(Button),
    Box(Box),
    Input(Input),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Layout {
    // Primary content
    pub top: Vec<Component>,
    pub main: Vec<Component>,
    pub metadata: Vec<Component>,

    // Left/right swipe destination, preloads
    pub left: Option<String>,
    pub right: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LayoutOrAction {
    Layout(Layout),
    Action(Action),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TextVariant {
    Title,
    Body,
    Hint,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Text {
    pub content: String,
    pub variant: TextVariant,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<Action>,
}

// Main image, taking full width
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Image {
    pub resource: String,
    pub mime: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GalleryImageType {
    Directory,
    Archive,
    File,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GalleryImage {
    pub ty: GalleryImageType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<Action>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gallery {
    pub images: Vec<GalleryImage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Button {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    pub action: Action,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Box {
    pub horizontal: bool,
    pub children: Vec<Component>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<Action>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InputType {
    Text,
    Number,
    Password,
}

impl InputType {
    pub fn as_str(&self) -> &'static str {
        match self {
            InputType::Text => "text",
            InputType::Number => "number",
            InputType::Password => "password",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Input {
    pub bidir: bool,
    /// Binding segment, counting from the start of the patth
    pub segment: usize,
    /// Name of the bound parameter
    pub param: String,
    pub ty: InputType,
    pub placeholder: Option<String>,
    pub button: Option<String>,
}
