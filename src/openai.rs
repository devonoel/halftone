use base64::Engine;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::time::Duration;

const API_URL: &str = "https://api.openai.com/v1/images/generations";

#[derive(Serialize)]
struct ImageRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    n: u32,
    size: &'a str,
}

#[derive(Deserialize)]
struct ImageResponse {
    data: Vec<ImageData>,
}

// `response_format` used to select between these two, but OpenAI has since
// dropped that parameter (it now errors as unknown) without documenting what
// replaced it -- so instead of asserting one shape, accept whichever field
// actually comes back.
#[derive(Deserialize)]
struct ImageData {
    b64_json: Option<String>,
    url: Option<String>,
}

/// reqwest's top-level error message (e.g. "error sending request for url
/// (...)") is usually just a wrapper -- the actual cause (DNS failure, TLS
/// error, timeout, connection reset) is chained underneath via `source()`
/// and gets silently dropped if you only print the outer error.
fn describe_error(err: &dyn Error) -> String {
    let mut message = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        message.push_str(&format!("\n  caused by: {cause}"));
        source = cause.source();
    }
    message
}

/// Generates an image from `prompt` via OpenAI's gpt-image-1 and returns the
/// raw image bytes. `size` must be one of the sizes gpt-image-1 accepts:
/// "1024x1024", "1536x1024", or "1024x1536".
pub fn generate_image(prompt: &str, size: &str) -> Result<Vec<u8>, String> {
    let api_key = std::env::var("OPENAI_API_KEY")
        .map_err(|_| "OPENAI_API_KEY environment variable is not set".to_string())?;

    let body = ImageRequest {
        model: "gpt-image-1",
        prompt,
        n: 1,
        size,
    };

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|err| format!("failed to build HTTP client: {}", describe_error(&err)))?;

    eprintln!(
        "Requesting image from OpenAI (gpt-image-1, size {size})... this can take up to a minute or so."
    );
    let response = client
        .post(API_URL)
        .bearer_auth(&api_key)
        .json(&body)
        .send()
        .map_err(|err| format!("request to OpenAI failed: {}", describe_error(&err)))?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().unwrap_or_default();
        return Err(format!("OpenAI returned {status}: {text}"));
    }
    eprintln!("Image received, parsing response...");

    let parsed: ImageResponse = response
        .json()
        .map_err(|err| format!("failed to parse OpenAI response: {}", describe_error(&err)))?;

    let image = parsed
        .data
        .into_iter()
        .next()
        .ok_or_else(|| "OpenAI response contained no image data".to_string())?;

    if let Some(b64_json) = image.b64_json {
        eprintln!("Decoding image data...");
        return base64::engine::general_purpose::STANDARD
            .decode(&b64_json)
            .map_err(|err| format!("failed to decode base64 image data: {err}"));
    }

    if let Some(url) = image.url {
        eprintln!("Downloading generated image...");
        let bytes = client
            .get(&url)
            .send()
            .map_err(|err| {
                format!(
                    "failed to download generated image: {}",
                    describe_error(&err)
                )
            })?
            .bytes()
            .map_err(|err| format!("failed to read downloaded image: {}", describe_error(&err)))?;
        return Ok(bytes.to_vec());
    }

    Err("OpenAI response contained neither b64_json nor url".to_string())
}
