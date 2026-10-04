use base64::Engine;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::thread;
use std::time::Duration;

const GENERATIONS_URL: &str = "https://api.openai.com/v1/images/generations";
const EDITS_URL: &str = "https://api.openai.com/v1/images/edits";

/// The model `halftone generate` uses unless told otherwise.
pub const DEFAULT_MODEL: &str = "gpt-image-1";

/// What to generate, beyond the prompt.
pub struct Options<'a> {
    pub model: &'a str,
    /// "1024x1024", "1536x1024", "1024x1536", or (on models that allow it)
    /// any size whose sides are multiples of 16.
    pub size: &'a str,
    /// "low", "medium", "high", or "auto"; the gpt-image-2.5 models also
    /// take "xhigh" and "max".
    pub quality: &'a str,
    pub count: u32,
    /// An image to work from. With one, the request goes to the edits
    /// endpoint and the model repaints the reference following the prompt,
    /// keeping its composition.
    pub reference: Option<&'a [u8]>,
    /// More images sent along after `reference`, for the prompt to refer to
    /// (a palette or style to match, say). Only sent with a `reference`.
    pub extra_references: &'a [&'a [u8]],
}

impl Default for Options<'_> {
    fn default() -> Self {
        Options {
            model: DEFAULT_MODEL,
            size: "1024x1024",
            // The output usually gets reduced to a halftone dot pattern, so
            // the fine detail higher qualities pay for is mostly wasted --
            // "low" cuts the per-image cost roughly 4-15x.
            quality: "low",
            count: 1,
            reference: None,
            extra_references: &[],
        }
    }
}

/// Whether a model takes `input_fidelity`, which makes edits follow the
/// reference more closely. Only the gpt-image-1 family does; the newer
/// models reject it outright.
fn supports_input_fidelity(model: &str) -> bool {
    model.starts_with("gpt-image-1") && !model.contains("mini")
}

/// The text fields of an edit request.
fn edit_fields(prompt: &str, options: &Options, n: u32) -> Vec<(&'static str, String)> {
    let mut fields = vec![
        ("model", options.model.to_string()),
        ("prompt", prompt.to_string()),
        ("n", n.to_string()),
        ("size", options.size.to_string()),
        ("quality", options.quality.to_string()),
    ];
    if supports_input_fidelity(options.model) {
        fields.push(("input_fidelity", "high".to_string()));
    }
    fields
}

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

/// Generates `options.count` image(s) from `prompt` -- repainting
/// `options.reference` if there is one -- and returns the raw image bytes
/// for each.
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
pub fn generate(prompt: &str, options: &Options) -> Result<Vec<Vec<u8>>, String> {
    let count = options.count.max(1);
    let (model, size) = (options.model, options.size);
    let api_key = std::env::var("OPENAI_API_KEY")
        .map_err(|_| "OPENAI_API_KEY environment variable is not set".to_string())?;

    let client = reqwest::blocking::Client::builder()
        // High-quality edits can take a few minutes.
        .timeout(Duration::from_secs(600))
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
            "Requesting {this_batch} {plural} from OpenAI ({model}, size {size}, {} of {count} total)... this can take a minute or more.",
            count - remaining + 1
        );

        match request_batch(&client, &api_key, prompt, options, this_batch) {
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
    options: &Options,
    n: u32,
) -> Result<Vec<ImageData>, BatchError> {
    let request = match options.reference {
        // Edits are multipart: the reference image plus text fields.
        Some(reference) => {
            let image = reqwest::blocking::multipart::Part::bytes(reference.to_vec())
                .file_name("reference.png")
                .mime_str("image/png")
                .map_err(|err| BatchError::Other(format!("bad reference image: {err}")))?;
            let mut form = edit_fields(prompt, options, n)
                .into_iter()
                .fold(reqwest::blocking::multipart::Form::new(), |form, (k, v)| {
                    form.text(k, v)
                })
                .part("image[]", image);
            for (i, extra) in options.extra_references.iter().enumerate() {
                let part = reqwest::blocking::multipart::Part::bytes(extra.to_vec())
                    .file_name(format!("reference-{}.png", i + 2))
                    .mime_str("image/png")
                    .map_err(|err| BatchError::Other(format!("bad reference image: {err}")))?;
                form = form.part("image[]", part);
            }
            client.post(EDITS_URL).multipart(form)
        }
        None => client.post(GENERATIONS_URL).json(&ImageRequest {
            model: options.model,
            prompt,
            n,
            size: options.size,
            quality: options.quality,
        }),
    };

    let response = request.bearer_auth(api_key).send().map_err(|err| {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_limit_from_rate_limit_message() {
        let message = r#"{"error":{"message":"Rate limit reached for gpt-image-1 in organization org-abc on input-images per min: Limit 5, Requested 10. Please try again in 12s."}}"#;
        assert_eq!(parse_rate_limit(message), Some(5));
        assert_eq!(parse_rate_limit("Limit 250, Requested 300"), Some(250));
    }

    #[test]
    fn rate_limit_without_a_limit_is_none() {
        assert_eq!(parse_rate_limit(""), None);
        assert_eq!(parse_rate_limit("Too many requests"), None);
        assert_eq!(parse_rate_limit("Limit reached"), None);
    }

    #[test]
    fn request_body_matches_the_images_api() {
        let body = serde_json::to_value(ImageRequest {
            model: "gpt-image-1",
            prompt: "a dragon",
            n: 3,
            size: "1024x1536",
            quality: "low",
        })
        .unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "model": "gpt-image-1",
                "prompt": "a dragon",
                "n": 3,
                "size": "1024x1536",
                "quality": "low",
            })
        );
    }

    #[test]
    fn only_the_gpt_image_1_family_gets_input_fidelity() {
        assert!(supports_input_fidelity("gpt-image-1"));
        assert!(!supports_input_fidelity("gpt-image-1-mini"));
        assert!(!supports_input_fidelity("gpt-image-2"));
        assert!(!supports_input_fidelity("gpt-image-2.5-sunburst"));
    }

    #[test]
    fn edit_requests_carry_the_options() {
        let options = Options {
            model: "gpt-image-2.5-sunburst",
            size: "1536x768",
            quality: "medium",
            count: 1,
            reference: Some(b"png"),
            extra_references: &[],
        };
        let fields = edit_fields("paint it", &options, 2);
        let get = |k| {
            fields
                .iter()
                .find(|(f, _)| *f == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("model"), Some("gpt-image-2.5-sunburst"));
        assert_eq!(get("prompt"), Some("paint it"));
        assert_eq!(get("n"), Some("2"));
        assert_eq!(get("size"), Some("1536x768"));
        assert_eq!(get("quality"), Some("medium"));
        assert_eq!(get("input_fidelity"), None);

        let old = Options {
            model: "gpt-image-1",
            ..options
        };
        let fields = edit_fields("paint it", &old, 1);
        assert!(fields.contains(&("input_fidelity", "high".to_string())));
    }

    #[test]
    fn defaults_keep_generation_cheap() {
        let options = Options::default();
        assert_eq!(
            (options.model, options.quality, options.count),
            ("gpt-image-1", "low", 1)
        );
        assert!(options.reference.is_none());
    }

    #[test]
    fn response_accepts_either_image_field() {
        let parsed: ImageResponse = serde_json::from_str(
            r#"{"created": 1, "data": [{"b64_json": "aGk="}, {"url": "https://example.com/a.png"}]}"#,
        )
        .unwrap();
        assert_eq!(parsed.data[0].b64_json.as_deref(), Some("aGk="));
        assert_eq!(parsed.data[0].url, None);
        assert_eq!(
            parsed.data[1].url.as_deref(),
            Some("https://example.com/a.png")
        );
    }

    #[test]
    fn decodes_base64_image_data() {
        let client = reqwest::blocking::Client::new();
        let image = ImageData {
            b64_json: Some("aGVsbG8=".into()),
            url: None,
        };
        assert_eq!(decode_image(&client, image).unwrap(), b"hello");
    }

    #[test]
    fn reports_bad_or_missing_image_data() {
        let client = reqwest::blocking::Client::new();
        let bad = ImageData {
            b64_json: Some("not base64!".into()),
            url: None,
        };
        assert!(
            decode_image(&client, bad)
                .unwrap_err()
                .contains("failed to decode base64")
        );
        let empty = ImageData {
            b64_json: None,
            url: None,
        };
        assert!(
            decode_image(&client, empty)
                .unwrap_err()
                .contains("neither b64_json nor url")
        );
    }

    #[derive(Debug)]
    struct Chained(&'static str, Option<Box<Chained>>);

    impl std::fmt::Display for Chained {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }

    impl Error for Chained {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.1.as_deref().map(|e| e as _)
        }
    }

    #[test]
    fn error_descriptions_include_every_cause() {
        let err = Chained(
            "error sending request",
            Some(Box::new(Chained(
                "connection reset",
                Some(Box::new(Chained("os error 54", None))),
            ))),
        );
        assert_eq!(
            describe_error(&err),
            "error sending request\n  caused by: connection reset\n  caused by: os error 54"
        );
    }
}
