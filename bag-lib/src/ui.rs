// Rendered UI components

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Component {
    Text(Text),
    Image(Image),
    Gallery(Gallery),
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
pub struct Text {
    pub content: String,
}

// Main image, taking full width
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Image {
    pub resource: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GalleryImage {
    pub thumbnail: Option<String>,
    pub name: String,
    // TODO: click action
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gallery {
    pub images: Vec<GalleryImage>,
}
