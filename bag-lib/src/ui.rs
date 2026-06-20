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
    top: Vec<Component>,
    main: Vec<Component>,
    metadata: Vec<Component>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Text {
    content: String,
}

// Main image, taking full width
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Image {
    resource: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GalleryImage {
    thumbnail: String,
    name: String,
    // TODO: click action
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gallery {
    images: Vec<GalleryImage>,
}
