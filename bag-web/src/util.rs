use std::sync::Arc;

use web_sys::{Response, js_sys::futures::JsFuture, wasm_bindgen::{JsCast, JsValue}};

#[derive(Clone, Debug)]
pub enum FetchError {
    BrowserError(JsValue),
    JsonError(Arc<serde_json::Error>),
}

impl From<JsValue> for FetchError {
    fn from(err: JsValue) -> Self {
        FetchError::BrowserError(err)
    }
}

impl From<serde_json::Error> for FetchError {
    fn from(err: serde_json::Error) -> Self {
        FetchError::JsonError(Arc::new(err))
    }
}

impl ToString for FetchError {
    fn to_string(&self) -> String {
        match self {
            FetchError::BrowserError(err) => format!("Browser error: {:?}", err),
            FetchError::JsonError(err) => format!("JSON error: {}", err),
        }
    }
}

pub async fn fetch<T>(url: &str) -> Result<T, FetchError>
where
    T: serde::de::DeserializeOwned,
{
    let resp = JsFuture::from(web_sys::window().unwrap().fetch_with_str(url)).await?;
    assert!(resp.is_instance_of::<Response>());
    let resp: Response = resp.dyn_into().unwrap();

    let resp_text = JsFuture::from(resp.text()?).await?.as_string().unwrap();
    let data = serde_json::from_str(&resp_text)?;
    Ok(data)
}
