use base64::Engine;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::thread;
use std::time::Duration;

const API_URL: &str = "https://api.openai.com/v1/images/generations";

// How long to wait before retrying a request that got rate limited at a
// batch size the account should be able to handle (i.e. not the "shrink the
// batch" case below, but "the same batch would work again once the account's
// per-minute window has partially refilled"). OpenAI's per-minute image
// limits appear to be a rolling window rather than a hard reset, so a wait
// shorter than a full minute is usually enough to free up some capacity.
const RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(20);

// Gives up on a batch after this many consecutive rate-limit waits at the
// same size, rather than retrying forever against a persistently exhausted
// account quota.
const MAX_RATE_LIMIT_RETRIES: u32 = 5;

#[derive(Serialize)]
struct ImageRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    n: u32,
    size: &'a str,
    quality: &'a str,
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

/// Outcome of one batch request, distinguishing "rejected for asking for too
/// many images in one call" (recoverable by asking for fewer) from "rejected
/// for some other reason" (not recoverable by retrying at all).
enum BatchError {
    RateLimited { allowed: Option<u32> },
    Other(String),
}

/// Generates `count` image(s) from `prompt` via OpenAI's gpt-image-1 and
/// returns the raw image bytes for each. `size` must be one of the sizes
/// gpt-image-1 accepts: "1024x1024", "1536x1024", or "1024x1536". Always
/// requested at "low" quality -- see the comment on `quality` in
/// `request_batch` for why.
///
/// Requesting `count` up front (gpt-image-1's own `n` parameter) rather than
/// making `count` separate calls gets every variation from a single round
/// trip when the account allows it -- the API is already built to return a
/// batch for one prompt. Accounts have their own per-minute image quota
/// though (e.g. "Limit 5, Requested 10"), which varies by usage tier and
/// isn't something this tool can know in advance, so a request that's
/// rejected as too large is retried at a smaller size instead of failing
/// outright, and a request rejected at a size the account should support is
/// retried after a short wait in case the per-minute window just needs to
/// partially refill.
pub fn generate_images(prompt: &str, size: &str, count: u32) -> Result<Vec<Vec<u8>>, String> {
    let api_key = std::env::var("OPENAI_API_KEY")
        .map_err(|_| "OPENAI_API_KEY environment variable is not set".to_string())?;

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|err| format!("failed to build HTTP client: {}", describe_error(&err)))?;

    let mut images = Vec::with_capacity(count as usize);
    let mut remaining = count;
    let mut batch_cap = count;
    let mut retries_at_current_size = 0;

    while remaining > 0 {
        let this_batch = remaining.min(batch_cap);
        let plural = if this_batch == 1 { "image" } else { "images" };
        eprintln!(
            "Requesting {this_batch} {plural} from OpenAI (gpt-image-1, size {size}, {} of {count} total)... this can take up to a minute or so.",
            count - remaining + 1
        );

        match request_batch(&client, &api_key, prompt, size, this_batch) {
            Ok(data) => {
                images.extend(data);
                remaining -= this_batch;
                retries_at_current_size = 0;
            }
            Err(BatchError::RateLimited { allowed }) => {
                if let Some(allowed) = allowed {
                    if allowed < this_batch {
                        eprintln!(
                            "Account allows at most {allowed} per request; retrying in smaller batches."
                        );
                        batch_cap = allowed.max(1);
                        retries_at_current_size = 0;
                        continue;
                    }
                }

                retries_at_current_size += 1;
                if retries_at_current_size > MAX_RATE_LIMIT_RETRIES {
                    return Err(format!(
                        "gave up after {MAX_RATE_LIMIT_RETRIES} rate-limit retries at batch size {this_batch}"
                    ));
                }
                eprintln!(
                    "Rate limited by OpenAI; waiting {}s before retrying ({retries_at_current_size}/{MAX_RATE_LIMIT_RETRIES})...",
                    RATE_LIMIT_BACKOFF.as_secs()
                );
                thread::sleep(RATE_LIMIT_BACKOFF);
            }
            Err(BatchError::Other(err)) => return Err(err),
        }
    }

    eprintln!("Images received, decoding...");
    images
        .into_iter()
        .map(|image| decode_image(&client, image))
        .collect()
}

/// Requests exactly `n` images in one call to OpenAI's images API.
fn request_batch(
    client: &reqwest::blocking::Client,
    api_key: &str,
    prompt: &str,
    size: &str,
    n: u32,
) -> Result<Vec<ImageData>, BatchError> {
    let body = ImageRequest {
        model: "gpt-image-1",
        prompt,
        n,
        size,
        // The output gets reduced to a halftone dot pattern anyway, so the
        // fine detail "medium"/"high" pays for is wasted -- "low" cuts the
        // per-image cost roughly 4-15x with no visible difference downstream.
        quality: "low",
    };

    let response = client
        .post(API_URL)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .map_err(|err| {
            BatchError::Other(format!(
                "request to OpenAI failed: {}",
                describe_error(&err)
            ))
        })?;

    let status = response.status();
    if !status.is_success() {
        let text = response.text().unwrap_or_default();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(BatchError::RateLimited {
                allowed: parse_rate_limit(&text),
            });
        }
        return Err(BatchError::Other(format!(
            "OpenAI returned {status}: {text}"
        )));
    }

    let parsed: ImageResponse = response.json().map_err(|err| {
        BatchError::Other(format!(
            "failed to parse OpenAI response: {}",
            describe_error(&err)
        ))
    })?;

    if parsed.data.is_empty() {
        return Err(BatchError::Other(
            "OpenAI response contained no image data".to_string(),
        ));
    }

    Ok(parsed.data)
}

/// Pulls the account's actual per-request limit out of OpenAI's rate-limit
/// message (e.g. "...in organization ... on input-images per min: Limit 5,
/// Requested 10."), so a request for more than the account allows can be
/// retried at a size that'll actually succeed instead of just failing.
fn parse_rate_limit(text: &str) -> Option<u32> {
    let after = text.split("Limit ").nth(1)?;
    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn decode_image(client: &reqwest::blocking::Client, image: ImageData) -> Result<Vec<u8>, String> {
    if let Some(b64_json) = image.b64_json {
        return base64::engine::general_purpose::STANDARD
            .decode(&b64_json)
            .map_err(|err| format!("failed to decode base64 image data: {err}"));
    }

    if let Some(url) = image.url {
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
